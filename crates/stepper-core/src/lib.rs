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
pub mod fanout;
pub mod hooks;
pub mod layer;
pub mod model;
pub mod orchestrator;
pub mod ports;
pub mod proc;
pub mod resolver;
pub mod session;
pub mod setup;
pub mod skills;
pub mod tasks;

pub use agent::{AgentLoop, LayerOutcome};
pub use approver::ChannelApprover;
pub use builtins::names as builtin_command_names;
pub use checkpoint::Snapshotter;
pub use dispatch::{
    DispatchRequest, DispatchResult, DispatchTool, Dispatcher, OrchestratorDispatcher,
};
pub use error::CoreError;
pub use fanout::{run_parallel, FanoutTask};
pub use hooks::{HookDecision, HookHost};
pub use layer::{FailurePolicy, Handoff, StepDef, SubTask};
pub use model::{ModelInfo, ModelRegistry};
pub use orchestrator::{Orchestrator, SessionLimits, TurnOutput};
pub use ports::ProviderResolver;
pub use resolver::ConfigProviderResolver;
pub use session::{SessionRecord, SessionStore, TurnRecord};
pub use setup::{build_steps, compose_system, load_base_context, AGENT_DIRECTIVES, DEFAULT_SYSTEM_PROMPT};
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
    let mut orchestrator = orchestrator;
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
        });
        let mut turn_id: u64 = session.turns.len() as u64;
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
                    turn_id += 1;
                    let _ = tx.send(AppEvent::TurnStarted { turn_id }).await;
                    checkpoint_turn(&snapshotter, turn_id, &tx).await;
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
                                    .run_turn(prompt.clone(), images.clone(), &tx, approver.clone(), turn_cancel.clone())
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
                    match result {
                        Some(Ok(output)) => {
                            cost.record_turn(output.usage, output.cost_usd);
                            session.turns.push(TurnRecord {
                                user: prompt,
                                summaries: output.summaries,
                                messages: output.messages,
                            });
                            let _ = store.save(&session);
                            // Carry the conversation into the live context so the
                            // NEXT turn remembers it. Without this, resume_seed only
                            // ever held the `--resume` history, so a live session
                            // forgot everything between turns.
                            orchestrator.resume_seed = session.seed_messages();
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
                Action::Rewind { checkpoint_id } => {
                    let (level, text) = match snapshotter.restore(&checkpoint_id) {
                        Ok(()) => {
                            // A `turn-N` checkpoint is the tree *before* turn N —
                            // drop turn N onward from the session and reset the
                            // counter so later turns/checkpoints stay consistent.
                            if let Some(n) = checkpoint_id
                                .strip_prefix("turn-")
                                .and_then(|s| s.parse::<usize>().ok())
                            {
                                let keep = n.saturating_sub(1);
                                session.turns.truncate(keep);
                                turn_id = keep as u64;
                                let _ = store.save(&session);
                            }
                            (NoticeLevel::Info, format!("rewound to {checkpoint_id}"))
                        }
                        Err(e) => (NoticeLevel::Warn, format!("rewind failed: {e}")),
                    };
                    let _ = tx.send(AppEvent::Notice { level, text }).await;
                }
                Action::Resume { session_id } => {
                    // In-session resume: replace the live session with the chosen
                    // one and reseed the conversation at full fidelity (old-format
                    // records synthesize digest pairs inside seed_messages).
                    match store.load(&session_id) {
                        Some(loaded) => {
                            session = loaded;
                            turn_id = session.turns.len() as u64;
                            orchestrator.resume_seed = session.seed_messages();
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
                    // Built-ins (/help, /clear, /compact, /context, /cost, /model,
                    // /permissions, /resume, /rewind) are handled first and don't
                    // run a turn; everything else expands a user command.
                    if builtins::handle(
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
                        // /clear begins a fresh session, so its checkpoints are
                        // no longer reachable — drop the store too.
                        if name == "clear"
                            && let Err(e) = snapshotter.clear()
                        {
                            let _ = tx
                                .send(AppEvent::Notice {
                                    level: NoticeLevel::Warn,
                                    text: format!("checkpoint clear failed: {e}"),
                                })
                                .await;
                        }
                        continue;
                    }
                    let project_root = orchestrator.project_root.clone();
                    let home = orchestrator.home.clone();
                    let cwd = orchestrator.cwd.clone();
                    let rules = orchestrator.rules.clone();
                    let mode = orchestrator.mode;
                    let (cmd_name, cmd_args) = (name.clone(), args.clone());
                    // Substitution is permission-gated (fail-closed); run it off-thread
                    // since `!`shell`` may block.
                    let expanded = tokio::task::spawn_blocking(move || {
                        commands::expand(project_root, home, cwd, rules, mode, cmd_name, cmd_args)
                    })
                    .await
                    .ok()
                    .flatten();

                    match expanded {
                        Some(prompt) => {
                            turn_id += 1;
                            let _ = tx.send(AppEvent::TurnStarted { turn_id }).await;
                            checkpoint_turn(&snapshotter, turn_id, &tx).await;
                            let images = std::mem::take(&mut pending_images);
                            let turn_cancel = cancel.child_token();
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
                            if let Some(Ok(output)) = result {
                                cost.record_turn(output.usage, output.cost_usd);
                                session.turns.push(TurnRecord {
                                    user: format!("/{name} {args}").trim().to_string(),
                                    summaries: output.summaries,
                                    messages: output.messages,
                                });
                                let _ = store.save(&session);
                                // Same as the chat path: carry the conversation
                                // forward so the next turn remembers this one.
                                orchestrator.resume_seed = session.seed_messages();
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
                        Ok(()) => (
                            NoticeLevel::Info,
                            format!("saved API key for '{provider}' — pick the model again"),
                        ),
                        Err(e) => (
                            NoticeLevel::Warn,
                            format!(
                                "failed to save the API key for '{provider}' to the OS keyring ({e}) — it will not persist and will be requested again"
                            ),
                        ),
                    };
                    let _ = tx.send(AppEvent::Notice { level, text }).await;
                }
                Action::RunShell(command) => {
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
                    orchestrator.mode = permission_mode(m);
                }
                _ => {}
            }
        }
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
            action = action_rx.recv() => match action {
                Some(Action::Interrupt) | None => turn_cancel.cancel(),
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

/// Snapshot the working tree for `/rewind`, surfacing a warning if it fails
/// (so the user knows the checkpoint is unavailable rather than discovering it
/// only on a failed rewind).
async fn checkpoint_turn(
    snapshotter: &Snapshotter,
    turn_id: u64,
    tx: &mpsc::Sender<AppEvent>,
) {
    if let Err(e) = snapshotter.snapshot(&format!("turn-{turn_id}")) {
        let _ = tx
            .send(AppEvent::Notice {
                level: NoticeLevel::Warn,
                text: format!("checkpoint failed (rewind unavailable): {e}"),
            })
            .await;
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
        rules: orchestrator.rules.clone(),
        approver,
        cancel,
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
