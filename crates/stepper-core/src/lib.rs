//! `stepper-core` — the orchestrator. Wires providers + tools + permission into
//! a ReAct agent loop, runs the `step` layer pipeline, and speaks the protocol
//! channel contract so it drops in where the Phase-1 mock core was.

pub mod agent;
pub mod approver;
pub mod builtins;
pub mod checkpoint;
pub mod commands;
pub mod compaction;
pub mod dispatch;
pub mod error;
pub mod exit_plan;
pub mod fanout;
pub mod hooks;
pub mod layer;
pub mod model;
pub mod orchestrator;
pub mod ports;
pub mod proc;
pub mod resolver;
pub mod review;
pub mod session;
pub mod setup;
pub mod skills;
pub mod stats;
pub mod tasks;

pub use agent::{AgentLoop, LayerOutcome, LspDiagnostics};
pub use approver::ChannelApprover;
pub use builtins::age_label;
pub use builtins::names as builtin_command_names;
pub use builtins::descriptions as builtin_command_descriptions;
pub use checkpoint::Snapshotter;
pub use dispatch::{
    DispatchRequest, DispatchResult, DispatchTool, Dispatcher, OrchestratorDispatcher, TaskTool,
};
pub use error::CoreError;
pub use fanout::{run_parallel, FanoutTask};
pub use hooks::{HookDecision, HookHost};
pub use stepper_tools::tools::memory::MEMORY_REL_PATH;
pub use layer::{AgentDef, FailurePolicy, Handoff, StepDef, SubTask};
pub use model::{ModelInfo, ModelRegistry};
pub use orchestrator::{Orchestrator, SessionLimits, TurnOutput};
pub use ports::ProviderResolver;
pub use resolver::ConfigProviderResolver;
pub use session::{SessionRecord, SessionStore, TurnRecord};
pub use stats::{aggregate_stats, SessionStats};
pub use setup::{
    build_steps, compose_system, load_agents, load_base_context, resolve_formatters,
    AGENT_DIRECTIVES, DEFAULT_SYSTEM_PROMPT,
};
pub use tasks::AssignTasksTool;

use std::sync::Arc;
use stepper_protocol::{Action, ActionRx, AppEvent, EventRx, NoticeLevel};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Spawn the real core: it owns the orchestrator and serves the same
/// `Action`→`AppEvent` channel contract as the Phase-1 mock, so the TUI is
/// unchanged. `session` is a fresh record (new id) or a loaded one for
/// `--resume`/`--continue`; turns are appended and persisted, and each turn is
/// checkpointed for `/rewind`. A loaded session seeds the next run with its
/// real prior messages (full fidelity); files saved before transcripts existed
/// fall back to the markdown digest. The system prompt is rebuilt fresh by the
/// caller, never persisted.
pub fn spawn_core(
    orchestrator: Orchestrator,
    mut session: SessionRecord,
    mut action_rx: ActionRx,
    cancel: CancellationToken,
) -> EventRx {
    let (tx, rx) = mpsc::channel::<AppEvent>(128);
    let store = SessionStore::new(&orchestrator.project_root);
    let snapshotter = Snapshotter::new(orchestrator.project_root.clone());
    // Checkpoints belong to the session whose turns produced them, but the
    // store is project-global — a fresh session, a crash leftover, or a resume
    // into a DIFFERENT session (`--resume <other>`, `--fork`) must not inherit
    // another timeline's `turn-N` trees, or `/rewind`/`/undo` would restore an
    // unrelated tree and delete the current work. Resuming the owning session
    // (-c / --resume of the last run) keeps its checkpoints.
    let _ = snapshotter.reconcile_owner(&session.id);
    // The redo stack is in-memory only, so any `redo-<n>` dir left on disk (a crash
    // between /undo and /redo) is unreferenced — GC it. A resumed session keeps its
    // `turn-<N>` checkpoints but never a live redo, so this is always safe.
    snapshotter.clear_redo_snapshots();
    let mut orchestrator = orchestrator;
    // The base context WITHOUT any resume digest — restored on /clear and /compact
    // so an old-format (digest-only) resume's prior content doesn't leak into the
    // system prompt of the supposedly-fresh session for the rest of the process.
    let base_context_original = orchestrator.base_context.clone();
    if session.has_messages() {
        orchestrator.resume_seed = session.seed_messages();
    } else {
        let digest = session.resume_context();
        if !digest.is_empty() {
            orchestrator.base_context =
                format!("{}\n\n{}", orchestrator.base_context, digest);
        }
    }
    tokio::spawn(async move {
        let approver = Arc::new(ChannelApprover {
            event_tx: tx.clone(),
            rules: orchestrator.rules.clone(),
            project_root: orchestrator.project_root.clone(),
            home: orchestrator.home.clone(),
        });
        let mut turn_id: u64 = session.turns.len() as u64;
        // `/undo` pushes a forward snapshot here so `/redo` can move back forward;
        // any new turn (or /rewind, /resume, /clear, /compact) invalidates the
        // stack. `redo_counter` keeps the `redo-<n>` ids unique even after a
        // /compact resets `turn_id`.
        let mut redo_stack: Vec<RedoEntry> = Vec::new();
        let mut redo_counter: u64 = 0;
        // Session-level usage/cost, accumulated from each completed turn for
        // the `/cost` built-in.
        let mut cost = builtins::SessionCost::default();
        // Actions deferred from mid-turn (SlashCommand/Rewind forwarded while a turn
        // ran) are replayed here after the turn, ahead of new channel actions.
        let mut pending: std::collections::VecDeque<Action> = std::collections::VecDeque::new();
        // Live `!cmd &` background processes: id → kill token (for the shell view).
        let mut procs: std::collections::HashMap<u64, CancellationToken> =
            std::collections::HashMap::new();
        let mut next_proc_id: u64 = 0;
        // Images pasted (Ctrl+V) since the last prompt — attached to the next turn.
        let mut pending_images: Vec<(String, String)> = Vec::new();

        loop {
            let action = match pending.pop_front() {
                Some(a) => a,
                None => match action_rx.recv().await {
                    Some(a) => a,
                    None => break,
                },
            };
            match action {
                Action::Quit => break,
                Action::SubmitInput(prompt) => {
                    // `#<agent> ...` routes this turn to a named sub-agent (its own
                    // model/tools/role) by transiently replacing the pipeline with
                    // that one agent. `# heading`-style text (a space after `#`) and
                    // unknown names fall through to a normal turn. The original
                    // `#<agent> …` text is still recorded as the user message.
                    let route = prompt
                        .strip_prefix('#')
                        .filter(|rest| !rest.starts_with(char::is_whitespace))
                        .and_then(|rest| {
                            let mut it = rest.splitn(2, char::is_whitespace);
                            let name = it.next().unwrap_or("");
                            let agent_prompt = it.next().unwrap_or("").trim().to_string();
                            orchestrator
                                .agents
                                .iter()
                                .find(|a| a.name == name)
                                .cloned()
                                .map(|a| (a, agent_prompt))
                        });
                    let (run_prompt, restore_steps) = match route {
                        Some((agent, agent_prompt)) if !agent_prompt.is_empty() => {
                            let default_model = orchestrator
                                .steps
                                .first()
                                .map(|s| s.model_ref.clone())
                                .unwrap_or_default();
                            let saved = std::mem::replace(
                                &mut orchestrator.steps,
                                vec![crate::setup::agent_step(&agent, &default_model)],
                            );
                            (agent_prompt, Some(saved))
                        }
                        _ => (prompt.clone(), None),
                    };
                    // The model that will actually run this turn (the swapped
                    // `#agent` step, or the primary step otherwise) — captured
                    // before `restore_steps` puts the pipeline back, for stats.
                    let turn_model_ref = orchestrator
                        .steps
                        .first()
                        .map(|s| s.model_ref.clone())
                        .unwrap_or_default();
                    turn_id += 1;
                    // A new turn forks the timeline — any pending /redo is now stale.
                    clear_redo(&mut redo_stack, &snapshotter);
                    let _ = tx.send(AppEvent::TurnStarted { turn_id }).await;
                    checkpoint_turn(&snapshotter, turn_id, session.turns.len(), &tx).await;
                    // Attach (and clear) any images pasted since the last prompt.
                    let images = std::mem::take(&mut pending_images);
                    // A per-turn child token lets `Interrupt` cancel just this turn
                    // (not the whole app); cancelling it aborts the stream/tools.
                    let turn_cancel = cancel.child_token();
                    let mut result = None;
                    let mut deferred = Vec::new();
                    let turn_timeout = orchestrator.limits.turn_timeout;
                    let control = run_watched(
                        async {
                            result = Some(
                                orchestrator
                                    .run_turn(run_prompt, images.clone(), &tx, approver.clone(), turn_cancel.clone())
                                    .await,
                            );
                        },
                        &turn_cancel,
                        &mut action_rx,
                        &mut deferred,
                        turn_timeout,
                        &tx,
                    )
                    .await;
                    // Restore the pipeline after a `#<agent>` turn.
                    if let Some(saved) = restore_steps {
                        orchestrator.steps = saved;
                    }
                    match result {
                        Some(Ok(output)) => {
                            cost.record_turn(output.usage, output.cost_usd);
                            session.turns.push(TurnRecord {
                                user: prompt,
                                summaries: output.summaries,
                                messages: output.messages,
                                usage: output.usage,
                                cost_usd: output.cost_usd,
                                model_ref: turn_model_ref,
                                ended_at: now_unix_secs(),
                            });
                            let _ = store.save(&session);
                            // Carry the conversation into the live context so the
                            // NEXT turn remembers it. Without this, resume_seed only
                            // ever held the `--resume` history, so a live session
                            // forgot everything between turns.
                            orchestrator.resume_seed = session.seed_messages();
                            // `seed_messages()` now synthesizes digest pairs for any
                            // old-format turns, so the digest folded into base_context
                            // at launch is redundant — drop it, or those turns show up
                            // BOTH in the system prompt and in resume_seed every turn.
                            orchestrator.base_context = base_context_original.clone();
                        }
                        Some(Err(e)) if !matches!(e, CoreError::Cancelled) => {
                            let _ = tx.send(AppEvent::Error(e.to_string())).await;
                        }
                        _ => {}
                    }
                    let _ = tx.send(AppEvent::TurnComplete { turn_id }).await;
                    pending.extend(deferred);
                    if matches!(control, Control::Quit) {
                        break;
                    }
                }
                Action::Rewind { checkpoint_id, scope } => {
                    use stepper_protocol::RewindScope;
                    // Jumping to an arbitrary checkpoint forks the timeline — any
                    // pending /redo forward snapshots are now unreachable.
                    clear_redo(&mut redo_stack, &snapshotter);
                    // A `turn-N` checkpoint is the tree *before* turn N — the turn
                    // count to keep when rewinding the conversation. Prefer the count
                    // recorded with the checkpoint (robust to a drifted turn-id after
                    // failed turns or /compact); fall back to parsing N-1 from the id.
                    let keep = snapshotter
                        .checkpoint_turns(&checkpoint_id)
                        .or_else(|| {
                            checkpoint_id
                                .strip_prefix("turn-")
                                .and_then(|s| s.parse::<usize>().ok())
                                .map(|n| n.saturating_sub(1))
                        })
                        // Clamp to the live turn count: a stale checkpoint (left by
                        // an earlier rewind) records more turns than the session now
                        // has, which would set turn_id past the real count.
                        .map(|k| k.min(session.turns.len()));
                    let restore_files = !matches!(scope, RewindScope::ConversationOnly);
                    let restore_convo = !matches!(scope, RewindScope::CodeOnly);
                    // Restore the working tree first (skipped for conversation-only).
                    let file_err = if restore_files {
                        snapshotter.restore(&checkpoint_id).err()
                    } else {
                        None
                    };
                    // Truncate the conversation (skipped for code-only) — but not if
                    // the file restore failed, so the two never drift out of sync.
                    if restore_convo && file_err.is_none() && let Some(keep) = keep {
                        session.turns.truncate(keep);
                        turn_id = keep as u64;
                        let _ = store.save(&session);
                        // Drop the now-abandoned "future" checkpoints so they can't
                        // be re-selected later and restore a tree newer than the
                        // (now shorter) conversation.
                        snapshotter.prune_forward(keep);
                        // Reseed the live conversation to the rewound point so the
                        // next turn's context matches the tree.
                        orchestrator.resume_seed = session.seed_messages();
                    }
                    let (level, text) = match file_err {
                        Some(e) => (NoticeLevel::Warn, format!("rewind failed: {e}")),
                        None => {
                            let what = match scope {
                                RewindScope::Both => "files + conversation",
                                RewindScope::CodeOnly => "files",
                                RewindScope::ConversationOnly => "conversation",
                            };
                            (NoticeLevel::Info, format!("rewound {what} to {checkpoint_id}"))
                        }
                    };
                    let _ = tx.send(AppEvent::Notice { level, text }).await;
                }
                Action::Resume { session_id } => {
                    // In-session resume: replace the live session with the chosen
                    // one and reseed the conversation at full fidelity (old-format
                    // records synthesize digest pairs inside seed_messages).
                    clear_redo(&mut redo_stack, &snapshotter);
                    match store.load(&session_id) {
                        Some(loaded) => {
                            session = loaded;
                            turn_id = session.turns.len() as u64;
                            // Switching sessions orphans the store's checkpoints
                            // (they trace the PREVIOUS session's file timeline);
                            // clear them so `/rewind` can't offer another
                            // session's tree. Resuming into the owning session
                            // keeps them.
                            let _ = snapshotter.reconcile_owner(&session.id);
                            orchestrator.resume_seed = session.seed_messages();
                            // The loaded session's own history now lives in
                            // resume_seed; drop any launch digest still folded into
                            // base_context so it doesn't leak across the switch.
                            orchestrator.base_context = base_context_original.clone();
                            let _ = tx
                                .send(AppEvent::SessionResumed {
                                    id: session.id.clone(),
                                    name: session.name.clone(),
                                    turns: turn_id,
                                })
                                .await;
                        }
                        None => {
                            let _ = tx
                                .send(AppEvent::Notice {
                                    level: NoticeLevel::Warn,
                                    text: format!("no session '{session_id}' found"),
                                })
                                .await;
                        }
                    }
                }
                Action::SlashCommand { name, args } => {
                    let project_root = orchestrator.project_root.clone();
                    let home = orchestrator.home.clone();
                    let cwd = orchestrator.cwd.clone();
                    // A user command file (`.stepper/commands/<name>.md`) overrides a
                    // same-named built-in: the project owner controls `.stepper/`
                    // (writes there already require explicit approval), so `/init`
                    // etc. can be customized. Built-ins are consulted only when no
                    // command file shadows the name.
                    let cmd_def = commands::find_command(&project_root, home.as_deref(), &name);
                    // `/undo` and `/redo` act immediately on the snapshotter (which
                    // `builtins::handle` doesn't receive — same reason /rewind lives
                    // here), so intercept them unless a user command file shadows the
                    // name. They are in the COMMANDS table for the palette/help.
                    if cmd_def.is_none() && (name == "undo" || name == "redo") {
                        handle_undo_redo(
                            &name,
                            &snapshotter,
                            &store,
                            &mut session,
                            &mut orchestrator,
                            &mut turn_id,
                            &mut redo_stack,
                            &mut redo_counter,
                            &tx,
                        )
                        .await;
                        continue;
                    }
                    // `/rename <name>` and `/export [path]` act on the LIVE session
                    // (which `builtins::handle` can mutate, but these also touch the
                    // store / write a file), so handle them here, before builtins.
                    if cmd_def.is_none() && (name == "rename" || name == "export") {
                        let (level, text) = handle_session_meta(
                            &name,
                            &args,
                            &mut session,
                            &store,
                            &orchestrator.project_root,
                        );
                        let _ = tx.send(AppEvent::Notice { level, text }).await;
                        continue;
                    }
                    if cmd_def.is_none()
                        && builtins::handle(
                            &name,
                            &args,
                            &mut orchestrator,
                            &mut session,
                            &mut turn_id,
                            &store,
                            &cost,
                            &tx,
                        )
                        .await
                    {
                        // /clear begins a fresh session and /compact collapses the
                        // turns to one synthetic turn (resetting turn_id) — in both
                        // cases the prior `turn-N` checkpoints are no longer
                        // reachable by their old count, so leaving them on disk lets
                        // /rewind restore a stale tree while the session/turn_id have
                        // moved on (a desync). Drop the store in both.
                        if name == "clear" || name == "compact" {
                            // Drop the now-unreachable checkpoints AND undo any
                            // old-format resume digest folded into base_context, so a
                            // /clear is a true clean break (new-format history lives
                            // in resume_seed, which builtins already cleared).
                            orchestrator.base_context = base_context_original.clone();
                            // snapshotter.clear() below removes the redo-* dirs too;
                            // just drop the stale stack entries.
                            redo_stack.clear();
                            match snapshotter.clear() {
                                // Re-stamp ownership: /clear swapped in a fresh
                                // session id, /compact keeps the id — either way
                                // the emptied store belongs to the live session.
                                Ok(()) => snapshotter.stamp_owner(&session.id),
                                Err(e) => {
                                    let _ = tx
                                        .send(AppEvent::Notice {
                                            level: NoticeLevel::Warn,
                                            text: format!("checkpoint clear failed: {e}"),
                                        })
                                        .await;
                                }
                            }
                        }
                        continue;
                    }
                    let rules = orchestrator.rules_snapshot();
                    let mode = orchestrator.mode_snapshot();
                    // A command's `model:` frontmatter overrides the primary layer for
                    // THIS turn only (applied below, restored after the turn).
                    let model_override = cmd_def.as_ref().and_then(|d| d.model.clone());
                    let cmd_args = args.clone();
                    // Substitution is permission-gated (fail-closed); run it off-thread
                    // since `!`shell`` may block. `None` here = no command file (and not
                    // a built-in) → reported as an unknown command below.
                    let expanded = match cmd_def {
                        Some(def) => tokio::task::spawn_blocking(move || {
                            commands::expand_with(def, project_root, home, cwd, rules, mode, cmd_args)
                        })
                        .await
                        .ok()
                        .flatten(),
                        // `/code-review` is the one built-in that RUNS a turn (the
                        // `builtins::handle` entries never do), so it expands here
                        // and flows through the command-turn path below. A user
                        // command file with the same name still shadows it.
                        None if name == "code-review" => {
                            match tokio::task::spawn_blocking(move || {
                                review::code_review_prompt(&cmd_args, &cwd)
                            })
                            .await
                            .ok()
                            {
                                Some(Ok(prompt)) => Some(prompt),
                                Some(Err(msg)) => {
                                    let _ = tx
                                        .send(AppEvent::Notice {
                                            level: NoticeLevel::Warn,
                                            text: msg,
                                        })
                                        .await;
                                    continue;
                                }
                                None => None,
                            }
                        }
                        None => None,
                    };

                    match expanded {
                        Some(prompt) => {
                            turn_id += 1;
                            clear_redo(&mut redo_stack, &snapshotter);
                            let _ = tx.send(AppEvent::TurnStarted { turn_id }).await;
                            checkpoint_turn(&snapshotter, turn_id, session.turns.len(), &tx).await;
                            let images = std::mem::take(&mut pending_images);
                            let turn_cancel = cancel.child_token();
                            // Apply the per-command `model:` override (transient): swap
                            // the primary layer's model, remembering the old one to put
                            // back after the turn. An unresolvable model is skipped with
                            // a warning rather than failing the command.
                            let restore_model = if let Some(m) = model_override.as_deref() {
                                if orchestrator.resolver.resolve(m).is_ok() {
                                    orchestrator
                                        .steps
                                        .first_mut()
                                        .map(|s| std::mem::replace(&mut s.model_ref, m.to_string()))
                                } else {
                                    let _ = tx
                                        .send(AppEvent::Notice {
                                            level: NoticeLevel::Warn,
                                            text: format!(
                                                "command model '{m}' could not be resolved — using the current model"
                                            ),
                                        })
                                        .await;
                                    None
                                }
                            } else {
                                None
                            };
                            // The running model (after any per-command override) for stats.
                            let turn_model_ref = orchestrator
                                .steps
                                .first()
                                .map(|s| s.model_ref.clone())
                                .unwrap_or_default();
                            let mut result = None;
                            let mut deferred = Vec::new();
                            let turn_timeout = orchestrator.limits.turn_timeout;
                            let control = run_watched(
                                async {
                                    result = Some(
                                        orchestrator
                                            .run_turn(prompt, images, &tx, approver.clone(), turn_cancel.clone())
                                            .await,
                                    );
                                },
                                &turn_cancel,
                                &mut action_rx,
                                &mut deferred,
                                turn_timeout,
                                &tx,
                            )
                            .await;
                            // Restore the primary layer's model after the command turn.
                            if let Some(old) = restore_model
                                && let Some(s) = orchestrator.steps.first_mut()
                            {
                                s.model_ref = old;
                            }
                            if let Some(Ok(output)) = result {
                                cost.record_turn(output.usage, output.cost_usd);
                                session.turns.push(TurnRecord {
                                    user: format!("/{name} {args}").trim().to_string(),
                                    summaries: output.summaries,
                                    messages: output.messages,
                                    usage: output.usage,
                                    cost_usd: output.cost_usd,
                                    model_ref: turn_model_ref,
                                    ended_at: now_unix_secs(),
                                });
                                let _ = store.save(&session);
                                // Same as the chat path: carry the conversation
                                // forward so the next turn remembers this one, and
                                // drop the now-redundant launch digest from
                                // base_context (else old turns double-expose).
                                orchestrator.resume_seed = session.seed_messages();
                                orchestrator.base_context = base_context_original.clone();
                            }
                            let _ = tx.send(AppEvent::TurnComplete { turn_id }).await;
                            pending.extend(deferred);
                            if matches!(control, Control::Quit) {
                                break;
                            }
                        }
                        None => {
                            let _ = tx
                                .send(AppEvent::Notice {
                                    level: NoticeLevel::Warn,
                                    text: format!("unknown command: /{name}"),
                                })
                                .await;
                        }
                    }
                }
                Action::SetApiKey { provider, key } => {
                    // Persist to the OS keyring; the next provider resolve picks it
                    // up via the explicit>env>keyring precedence (no restart).
                    let (level, text) = match stepper_providers::store_key_in_keyring(&provider, &key)
                    {
                        Ok(()) => {
                            let mut text =
                                format!("saved API key for '{provider}' — pick the model again");
                            // The keyring is the lowest-precedence source; warn if the
                            // config already pins an explicit key that will shadow it.
                            if orchestrator.resolver.provider_has_explicit_key(&provider) {
                                text.push_str(
                                    " (note: this provider has an explicit apiKey in setting.json \
                                     or a set env var that takes precedence — remove it to use this key)",
                                );
                            }
                            (NoticeLevel::Info, text)
                        }
                        Err(e) => (
                            NoticeLevel::Warn,
                            format!(
                                "failed to save the API key for '{provider}' to the OS keyring ({e}) — it will not persist and will be requested again"
                            ),
                        ),
                    };
                    let _ = tx.send(AppEvent::Notice { level, text }).await;
                }
                Action::SetTheme { preset, colors } => {
                    // The TUI already applied the theme live; persist it so the next
                    // session loads it. Project `.stepper/` wins, else user `~/.stepper/`.
                    let project = orchestrator.project_root.join(".stepper");
                    let dir = if project.is_dir() {
                        Some(project)
                    } else {
                        orchestrator.home.as_ref().map(|h| h.join(".stepper"))
                    };
                    if let Some(dir) = dir {
                        let result = stepper_config::scaffold::update_settings(&dir, |obj| {
                            let mut theme = serde_json::Map::new();
                            if let Some(p) = &preset {
                                theme.insert("preset".into(), serde_json::Value::String(p.clone()));
                            }
                            if !colors.is_empty() {
                                let map: serde_json::Map<String, serde_json::Value> = colors
                                    .iter()
                                    .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
                                    .collect();
                                theme.insert("colors".into(), serde_json::Value::Object(map));
                            }
                            obj.insert("theme".into(), serde_json::Value::Object(theme));
                        });
                        if let Err(e) = result {
                            let _ = tx
                                .send(AppEvent::Notice {
                                    level: NoticeLevel::Warn,
                                    text: format!("theme not persisted: {e}"),
                                })
                                .await;
                        }
                    }
                }
                Action::RunShell(command) => {
                    // A `!cmd` (foreground or detached `&`) runs arbitrary shell in
                    // the project tree — it forks the timeline just like a turn, so
                    // any pending /redo forward snapshot is now stale. Invalidate it
                    // before the command can touch a file (covers both branches).
                    clear_redo(&mut redo_stack, &snapshotter);
                    let (inner, background) = proc::parse_background(&command);
                    if background {
                        // `!cmd &` — spawn detached (no turn, no 120s timeout) and
                        // track it for the shell view; output streams in as events.
                        let id = next_proc_id;
                        next_proc_id += 1;
                        let token = cancel.child_token();
                        procs.insert(id, token.clone());
                        proc::spawn_background(
                            id,
                            inner,
                            orchestrator.cwd.clone(),
                            orchestrator.home.clone(),
                            tx.clone(),
                            token,
                        );
                        continue;
                    }
                    turn_id += 1;
                    let _ = tx.send(AppEvent::TurnStarted { turn_id }).await;
                    let turn_cancel = cancel.child_token();
                    let mut deferred = Vec::new();
                    let control = run_watched(
                        run_shell(&orchestrator, &command, &tx, approver.clone(), turn_cancel.clone()),
                        &turn_cancel,
                        &mut action_rx,
                        &mut deferred,
                        // Interactive `!cmd` is not an agent turn — no time limit.
                        None,
                        &tx,
                    )
                    .await;
                    let _ = tx.send(AppEvent::TurnComplete { turn_id }).await;
                    pending.extend(deferred);
                    if matches!(control, Control::Quit) {
                        break;
                    }
                }
                Action::KillProcess(id) => {
                    if let Some(token) = procs.remove(&id) {
                        token.cancel();
                    }
                }
                Action::AttachImage { media_type, data } => {
                    pending_images.push((media_type, data));
                }
                // Shift+Tab / mode switches must reach the permission engine —
                // without this the orchestrator keeps its startup mode forever and
                // switching into Auto (etc.) interactively has no effect.
                Action::SetMode(m) => {
                    *orchestrator.mode.write().unwrap() = permission_mode(m);
                }
                _ => {}
            }
        }
        // The action loop ended (Quit or the action channel closed) — fire
        // SessionEnd once, distinct from the per-turn Stop, with a fresh token.
        let _ = orchestrator
            .hooks
            .run("SessionEnd", None, &serde_json::json!({}), &CancellationToken::new())
            .await;
    });
    rx
}

/// Map the protocol UI `Mode` (what Shift+Tab cycles) to the permission engine's
/// `PermissionMode`. The two enums mirror each other 1:1.
fn permission_mode(m: stepper_protocol::Mode) -> stepper_permission::PermissionMode {
    use stepper_permission::PermissionMode as P;
    use stepper_protocol::Mode as M;
    match m {
        M::Auto => P::Auto,
        M::Plan => P::Plan,
        M::AcceptEdits => P::AcceptEdits,
        M::Default => P::Default,
        M::DontAsk => P::DontAsk,
        M::Bypass => P::Bypass,
    }
}

/// Outcome of a watched turn: whether the outer loop should keep going or quit.
enum Control {
    Continue,
    Quit,
}

/// Notice text prefix emitted when a turn is stopped by its wall-clock timeout.
/// Shared so a headless (`-p`) caller can recognize the timeout and exit non-zero
/// (the turn itself unwinds as a silent `Cancelled`, like an `Esc`).
pub const TURN_TIMEOUT_NOTICE: &str = "stopped: turn time limit reached";

/// Drive `work` (a running turn) to completion while concurrently watching the
/// action channel, so a mid-turn `Interrupt` can cancel it. Without this, the
/// action loop is blocked awaiting the turn and `Esc` would sit unread until the
/// turn finished (and the TUI's queue would wedge forever). On `Interrupt` (or a
/// closed channel) the per-turn token is cancelled — the turn then unwinds via
/// `CoreError::Cancelled` and the caller still emits `TurnComplete`.
///
/// Chat/shell submissions can't arrive here (the TUI queues them while a turn is
/// active) and `Approve` is TUI-local, but `SlashCommand`/`Rewind`/`Resume` ARE
/// forwarded mid-turn — those are collected into `deferred` and replayed by the
/// caller after the turn (rather than silently dropped).
async fn run_watched(
    work: impl std::future::Future<Output = ()>,
    turn_cancel: &CancellationToken,
    action_rx: &mut ActionRx,
    deferred: &mut Vec<Action>,
    timeout: Option<std::time::Duration>,
    tx: &mpsc::Sender<AppEvent>,
) -> Control {
    tokio::pin!(work);
    // A `None` timeout never fires (pends forever); a `Some` one fires once and
    // cancels the turn, which then unwinds via `CoreError::Cancelled`.
    let timer = async {
        match timeout {
            Some(d) => tokio::time::sleep(d).await,
            None => std::future::pending::<()>().await,
        }
    };
    tokio::pin!(timer);
    let mut timed_out = false;
    // Once the action channel closes (sender dropped — app shutdown/panic), `recv`
    // returns `None` immediately and forever. Without disabling the branch the
    // `select!` would busy-spin at 100% CPU until `work` finishes; the flag parks
    // it so only `work`/`timer` are awaited after the channel closes.
    let mut rx_closed = false;
    loop {
        tokio::select! {
            _ = &mut work => return Control::Continue,
            _ = &mut timer, if !timed_out => {
                timed_out = true;
                let _ = tx
                    .send(AppEvent::Notice {
                        level: NoticeLevel::Warn,
                        text: format!(
                            "{TURN_TIMEOUT_NOTICE} ({})",
                            fmt_secs(timeout.unwrap_or_default().as_secs())
                        ),
                    })
                    .await;
                turn_cancel.cancel();
            }
            action = action_rx.recv(), if !rx_closed => match action {
                Some(Action::Interrupt) => turn_cancel.cancel(),
                None => {
                    turn_cancel.cancel();
                    rx_closed = true;
                }
                Some(Action::Quit) => {
                    turn_cancel.cancel();
                    return Control::Quit;
                }
                Some(other) => deferred.push(other),
            }
        }
    }
}

/// Render a whole-second duration as `Nm Ns` / `Nm` / `Ns` for a notice.
fn fmt_secs(secs: u64) -> String {
    match (secs / 60, secs % 60) {
        (0, s) => format!("{s}s"),
        (m, 0) => format!("{m}m"),
        (m, s) => format!("{m}m {s}s"),
    }
}

/// Wall-clock unix-epoch seconds for a turn's `ended_at`, or `None` if the system
/// clock is before the epoch (never, in practice).
pub(crate) fn now_unix_secs() -> Option<u64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

/// Snapshot the working tree for `/rewind`, surfacing a warning if it fails
/// (so the user knows the checkpoint is unavailable rather than discovering it
/// only on a failed rewind).
async fn checkpoint_turn(
    snapshotter: &Snapshotter,
    turn_id: u64,
    turns_completed: usize,
    tx: &mpsc::Sender<AppEvent>,
) {
    let id = format!("turn-{turn_id}");
    // A `/undo` followed by this new turn abandons the undone turns' future
    // checkpoints (turn-N taken when MORE than `turns_completed` turns were
    // done). They are unreachable now — drop them so the rewind picker can't
    // offer a stale tree that would overwrite this turn's work.
    snapshotter.prune_forward(turns_completed);
    if let Err(e) = snapshotter.snapshot(&id) {
        let _ = tx
            .send(AppEvent::Notice {
                level: NoticeLevel::Warn,
                text: format!("checkpoint failed (rewind unavailable): {e}"),
            })
            .await;
    } else {
        // Record how many session turns were complete at snapshot time so a later
        // `/rewind` truncates by this count rather than parsing N from the id —
        // the turn-id counter can drift past the real turn count (failed turns,
        // `/compact`), which would desync the session from the restored tree.
        snapshotter.record_turns(&id, turns_completed);
    }
    // Bound the store: every turn full-copies the tree, so cap retained
    // snapshots. A prune failure must not fail the turn.
    if let Err(e) = snapshotter.prune(checkpoint::RETAIN) {
        let _ = tx
            .send(AppEvent::Notice {
                level: NoticeLevel::Warn,
                text: format!("checkpoint prune failed: {e}"),
            })
            .await;
    }
}

/// A `/undo` step: the working-tree snapshot taken just before the undo (so
/// `/redo` can move forward again), the session turns the undo dropped, and the
/// `turn_id` to restore on redo. The `redo-<n>` snapshot id is outside the
/// `turn-<N>` namespace, so `prune`/`rewind` never touch it.
struct RedoEntry {
    snapshot_id: String,
    turns: Vec<TurnRecord>,
    turn_id: u64,
}

/// Drop every pending `/redo` step and delete its forward snapshot from disk.
/// Called whenever the timeline forks (a new turn, `/rewind`, `/resume`), so a
/// later `/redo` can never restore a stale tree over newer work.
fn clear_redo(stack: &mut Vec<RedoEntry>, snapshotter: &Snapshotter) {
    for entry in stack.drain(..) {
        snapshotter.remove(&entry.snapshot_id);
    }
}

/// `/rename <name>` sets the live session's name and persists it; `/export
/// [path]` writes the session's Markdown transcript to `path` (or a default
/// `.stepper/exports/<id>.md` under the project root). Returns a status notice.
fn handle_session_meta(
    name: &str,
    args: &str,
    session: &mut SessionRecord,
    store: &SessionStore,
    project_root: &std::path::Path,
) -> (NoticeLevel, String) {
    if name == "rename" {
        let new_name = args.trim();
        if new_name.is_empty() {
            return (NoticeLevel::Warn, "usage: /rename <name>".into());
        }
        session.name = Some(new_name.to_string());
        return match store.save(session) {
            Ok(()) => (NoticeLevel::Info, format!("renamed session to '{new_name}'")),
            Err(e) => (NoticeLevel::Warn, format!("rename failed to persist: {e}")),
        };
    }
    // /export
    if session.turns.is_empty() {
        return (NoticeLevel::Warn, "nothing to export yet (no turns)".into());
    }
    let dest = if args.trim().is_empty() {
        let dir = project_root.join(".stepper").join("exports");
        if let Err(e) = std::fs::create_dir_all(&dir) {
            return (NoticeLevel::Warn, format!("export failed: {e}"));
        }
        dir.join(format!("{}.md", session.id))
    } else {
        let p = std::path::PathBuf::from(args.trim());
        if p.is_absolute() { p } else { project_root.join(p) }
    };
    // Create the destination's parent (mirrors the default branch, which makes
    // `.stepper/exports`) so a user path with a not-yet-existing subdirectory
    // — `/export reports/out.md` — writes instead of failing with ENOENT.
    if let Some(parent) = dest.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        return (NoticeLevel::Warn, format!("export failed: {e}"));
    }
    match std::fs::write(&dest, session.to_transcript_md()) {
        Ok(()) => (NoticeLevel::Info, format!("exported transcript to {}", dest.display())),
        Err(e) => (NoticeLevel::Warn, format!("export failed: {e}")),
    }
}

/// `/undo` reverts the last turn: it snapshots the current tree (for `/redo`),
/// restores the `turn-<turn_id>` checkpoint (the tree BEFORE that turn), drops
/// the turn(s) from the session, and pushes a redo step. `/redo` pops the most
/// recent step, restores its forward snapshot, re-appends the dropped turns, and
/// deletes the consumed snapshot. The whole stack is invalidated by any new turn
/// (the caller calls `clear_redo`).
#[allow(clippy::too_many_arguments)]
async fn handle_undo_redo(
    name: &str,
    snapshotter: &Snapshotter,
    store: &SessionStore,
    session: &mut SessionRecord,
    orchestrator: &mut Orchestrator,
    turn_id: &mut u64,
    redo_stack: &mut Vec<RedoEntry>,
    redo_counter: &mut u64,
    tx: &mpsc::Sender<AppEvent>,
) {
    if name == "undo" {
        if *turn_id == 0 {
            let _ = tx
                .send(AppEvent::Notice { level: NoticeLevel::Info, text: "nothing to undo".into() })
                .await;
            return;
        }
        let checkpoint_id = format!("turn-{turn_id}");
        // The session turns to keep = those completed BEFORE the undone turn. Prefer
        // the count recorded with the checkpoint (robust to a turn_id drifted past
        // the real turn count by failed turns / `/compact`); clamp to be safe.
        let keep = snapshotter
            .checkpoint_turns(&checkpoint_id)
            .or_else(|| (*turn_id as usize).checked_sub(1))
            .unwrap_or(0)
            .min(session.turns.len());
        // Snapshot the CURRENT tree FIRST so `/redo` can move forward again.
        let redo_id = format!("redo-{redo_counter}");
        *redo_counter += 1;
        if let Err(e) = snapshotter.snapshot(&redo_id) {
            let _ = tx
                .send(AppEvent::Notice { level: NoticeLevel::Warn, text: format!("undo failed (snapshot): {e}") })
                .await;
            return;
        }
        if let Err(e) = snapshotter.restore(&checkpoint_id) {
            snapshotter.remove(&redo_id);
            let _ = tx
                .send(AppEvent::Notice { level: NoticeLevel::Warn, text: format!("undo failed: {e}") })
                .await;
            return;
        }
        let dropped = session.turns.split_off(keep);
        redo_stack.push(RedoEntry { snapshot_id: redo_id, turns: dropped, turn_id: *turn_id });
        *turn_id = keep as u64;
        let _ = store.save(session);
        orchestrator.resume_seed = session.seed_messages();
        let _ = tx
            .send(AppEvent::Notice {
                level: NoticeLevel::Info,
                text: "undid the last turn (/redo to restore)".into(),
            })
            .await;
    } else {
        let Some(entry) = redo_stack.pop() else {
            let _ = tx
                .send(AppEvent::Notice { level: NoticeLevel::Info, text: "nothing to redo".into() })
                .await;
            return;
        };
        if let Err(e) = snapshotter.restore(&entry.snapshot_id) {
            snapshotter.remove(&entry.snapshot_id);
            let _ = tx
                .send(AppEvent::Notice { level: NoticeLevel::Warn, text: format!("redo failed: {e}") })
                .await;
            return;
        }
        session.turns.extend(entry.turns);
        *turn_id = entry.turn_id;
        snapshotter.remove(&entry.snapshot_id);
        let _ = store.save(session);
        orchestrator.resume_seed = session.seed_messages();
        let _ = tx
            .send(AppEvent::Notice { level: NoticeLevel::Info, text: "redid".into() })
            .await;
    }
}

/// `!`-prefixed direct shell: run the bash tool once through the permission gate.
async fn run_shell(
    orchestrator: &Orchestrator,
    command: &str,
    tx: &mpsc::Sender<AppEvent>,
    approver: Arc<dyn stepper_tools::Approver>,
    cancel: CancellationToken,
) {
    use stepper_tools::ToolCx;
    let Some(bash) = orchestrator.base_tools.get("bash") else {
        return;
    };
    let cx = ToolCx {
        cwd: orchestrator.cwd.clone(),
        project_root: orchestrator.project_root.clone(),
        home: orchestrator.home.clone(),
        // A hand-typed `!cmd` is user-initiated, not model-initiated, so it runs
        // permissionless (no approval prompt) under Bypass. The guards that still
        // apply: the bash tool's secret-file screen (runs before the gate, so
        // `!cat ~/.ssh/id_rsa` is still refused) and explicit `deny` rules (deny
        // wins in every mode). This does NOT touch the `.stepper/commands` shell
        // gate, which stays rule-only fail-closed (model-plantable, see commands.rs).
        mode: stepper_permission::PermissionMode::Bypass,
        // Hard-coded Bypass — no live-mode handle (an interactive !cmd is never
        // subject to the model's plan-mode flips).
        live_mode: None,
        rules: orchestrator.rules_snapshot(),
        approver,
        cancel,
        // The OS sandbox still applies under Bypass: a hand-typed `!cmd` skips the
        // approval prompt but not the defense-in-depth write confinement.
        sandbox_writable_roots: orchestrator.sandbox_writable_roots.clone(),
    };
    match bash
        .call(serde_json::json!({ "command": command }), &cx)
        .await
    {
        Ok(result) => {
            let _ = tx
                .send(AppEvent::AssistantTokenDelta(format!(
                    "```sh\n$ {command}\n{}\n```\n",
                    result.content_text()
                )))
                .await;
        }
        Err(e) => {
            let _ = tx
                .send(AppEvent::Notice {
                    level: NoticeLevel::Warn,
                    text: format!("shell: {e}"),
                })
                .await;
        }
    }
}

#[cfg(test)]
mod session_meta_tests {
    use super::*;
    use crate::session::{SessionStore, TurnRecord};

    #[test]
    fn export_to_a_not_yet_existing_subdirectory_creates_it() {
        // `/export reports/out.md` must create `reports/` (mirroring the default
        // branch that makes `.stepper/exports`) instead of failing with ENOENT.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let store = SessionStore::new(root);
        let mut session = SessionRecord::fresh();
        session.turns.push(TurnRecord { user: "hi".into(), ..Default::default() });

        let (level, msg) =
            handle_session_meta("export", "reports/out.md", &mut session, &store, root);
        assert!(matches!(level, NoticeLevel::Info), "export succeeded: {msg}");
        let written = std::fs::read_to_string(root.join("reports/out.md")).unwrap();
        assert!(written.contains("hi"), "transcript body present");
    }
}
