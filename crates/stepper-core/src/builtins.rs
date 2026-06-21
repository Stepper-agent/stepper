//! Built-in slash commands (`/help`, `/clear`, `/compact`, `/context`, `/cost`,
//! `/model`, `/permissions`, `/resume`, `/rewind`). These are handled inside the
//! core action loop *before* user-authored `.stepper/commands` files, mirroring
//! Claude Code's built-in command surface. They emit their own events (a
//! `Notice`, `ModelChanged`, `ContextBreakdown`, `PermissionsSnapshot`,
//! `CheckpointList`, `SessionList`, or `CompactionStarted`/`Done`) and never run
//! an agent turn.

use crate::compaction::{estimate_tokens, Compactor};
use crate::error::CoreError;
use crate::orchestrator::Orchestrator;
use crate::ports::ConnectedProvider;
use crate::session::{SessionRecord, SessionStore, TurnRecord};
use std::path::Path;
use stepper_permission::{PermissionMode, Rule};
use stepper_protocol::{
    AppEvent, ApprovalRuleView, CheckpointView, ContextBreakdownView, EventTx, ModelView,
    NoticeLevel, PermissionRuleView, PermissionsSnapshotView, SessionView, SettingsRowView,
    SettingsSnapshotView, SettingsTabView,
};
use stepper_provider::Usage;

/// The built-in commands as `(name, args, description)` — the single source for
/// the `/` palette, the `builtin_command_names` export, and `/help`, so the three
/// can never drift.
const COMMANDS: &[(&str, &str, &str)] = &[
    ("help", "", "show this help"),
    ("clear", "", "reset conversation"),
    ("compact", "[instructions]", "compact the conversation now"),
    ("context", "", "window breakdown"),
    ("cost", "", "session usage & USD"),
    ("init", "", "create the .stepper/ skeleton"),
    ("scaffold-layer", "", "write a default plan→implement→review pipeline"),
    ("layer", "<name>", "new layer"),
    ("command", "<name>", "new slash command"),
    ("import", "[claude|codex|cursor|gemini|all] [apply]", "migrate another agent's config"),
    ("connect", "", "add a provider from models.dev"),
    ("login", "[provider]", "set an API key"),
    ("model", "[provider/model]", "show or switch"),
    ("models", "", "pick from fetched models"),
    ("theme", "", "edit the TUI color theme"),
    ("editor", "[text]", "compose the prompt in $EDITOR"),
    ("effort", "[off|low|medium|high|xhigh|max]", "reasoning effort"),
    ("permissions", "", "rules & approvals"),
    ("settings", "", "all settings (tabbed overview)"),
    ("allow", "<spec>", "add an allow rule (e.g. Bash(cargo *))"),
    ("ask", "<spec>", "add an ask rule"),
    ("deny", "<spec>", "add a deny rule"),
    ("resume", "", "pick a session"),
    ("rename", "<name>", "rename this session"),
    ("export", "[path]", "write the session transcript to a file"),
    ("rewind", "[code|conversation]", "pick a checkpoint, also Esc-Esc"),
    ("undo", "", "revert the last turn (files + message)"),
    ("redo", "", "re-apply an undone turn"),
];

/// Names of the built-in commands, for the `/` palette (merged with the user's).
pub fn names() -> Vec<String> {
    COMMANDS.iter().map(|(n, _, _)| n.to_string()).collect()
}

/// `(name, description)` for the `/` palette — the arg hints stay in `/help`.
pub fn descriptions() -> Vec<(&'static str, &'static str)> {
    COMMANDS.iter().map(|(n, _, d)| (*n, *d)).collect()
}

/// Session-level usage accounting for `/cost`, accumulated by `spawn_core` from
/// each completed turn's `TurnOutput` (cancelled/failed turns are not counted).
#[derive(Debug, Clone, Copy, Default)]
pub struct SessionCost {
    pub usage: Usage,
    pub cost_usd: f64,
    pub turn_usage: Usage,
    pub turn_cost_usd: f64,
}

impl SessionCost {
    pub fn record_turn(&mut self, usage: Usage, cost_usd: f64) {
        self.usage.add(&usage);
        self.cost_usd += cost_usd;
        self.turn_usage = usage;
        self.turn_cost_usd = cost_usd;
    }
}

/// Handle a built-in command. Returns `true` if `name` was a built-in (already
/// handled), `false` to fall through to user-command expansion.
#[allow(clippy::too_many_arguments)]
pub async fn handle(
    name: &str,
    args: &str,
    orchestrator: &mut Orchestrator,
    session: &mut SessionRecord,
    turn_id: &mut u64,
    store: &SessionStore,
    cost: &SessionCost,
    tx: &EventTx,
) -> bool {
    match name {
        "help" => {
            notice(tx, NoticeLevel::Info, help_text()).await;
            true
        }
        "clear" => {
            // A fresh session (new id) — a clean break, not just emptied turns.
            // The old session stays on disk (resumable via /resume).
            *session = SessionRecord::fresh();
            orchestrator.resume_seed.clear();
            *turn_id = 0;
            let _ = store.save(session);
            // `Cleared` (not a Notice) so the TUI also purges the terminal
            // scrollback — the previous conversation disappears like `clear`.
            let _ = tx.send(AppEvent::Cleared).await;
            true
        }
        "compact" => {
            handle_compact(args, orchestrator, session, turn_id, store, tx).await;
            true
        }
        "context" => {
            handle_context(orchestrator, session, tx).await;
            true
        }
        "cost" => {
            notice(tx, NoticeLevel::Info, cost_text(cost)).await;
            true
        }
        "init" => {
            handle_init(&orchestrator.project_root, tx).await;
            true
        }
        "scaffold-layer" => {
            handle_scaffold_layer(&orchestrator.project_root, tx).await;
            true
        }
        "layer" => {
            handle_new_layer(args.trim(), &orchestrator.project_root, tx).await;
            true
        }
        "command" => {
            handle_new_command(args.trim(), &orchestrator.project_root, tx).await;
            true
        }
        "import" => {
            handle_import(args.trim(), orchestrator.home.as_deref(), tx).await;
            true
        }
        "connect" => {
            handle_connect(args.trim(), orchestrator, tx).await;
            true
        }
        "login" => {
            handle_login(args.trim(), orchestrator, tx).await;
            true
        }
        "model" => {
            handle_model(args.trim(), orchestrator, tx).await;
            true
        }
        "models" => {
            handle_models(orchestrator, tx).await;
            true
        }
        "theme" => {
            // Colors live TUI-side, so just ask the TUI to open its editor; the
            // chosen theme comes back as `Action::SetTheme` for persistence.
            let _ = tx.send(AppEvent::OpenThemeEditor).await;
            true
        }
        "editor" => {
            // The editor spawn (terminal handoff) is TUI-only; pass the slash
            // argument through as the seed (`/editor foo` starts the buffer at
            // "foo"). The edited result replaces the TUI input box.
            let _ = tx.send(AppEvent::OpenEditor { seed: args.trim().to_string() }).await;
            true
        }
        "effort" => {
            handle_effort(args.trim(), orchestrator, tx).await;
            true
        }
        "permissions" => {
            handle_permissions(orchestrator, tx).await;
            true
        }
        "settings" => {
            handle_settings(orchestrator, tx).await;
            true
        }
        verdict @ ("allow" | "ask" | "deny") => {
            handle_permission_rule(verdict, args.trim(), orchestrator, tx).await;
            true
        }
        "resume" => {
            handle_resume(store, tx).await;
            true
        }
        "rewind" => {
            // `/rewind code` restores files only, `/rewind conversation` the
            // transcript only; bare `/rewind` (and Esc-Esc) restores both.
            let scope = match args.trim().to_ascii_lowercase().as_str() {
                "code" | "files" => stepper_protocol::RewindScope::CodeOnly,
                "conversation" | "convo" | "chat" => stepper_protocol::RewindScope::ConversationOnly,
                _ => stepper_protocol::RewindScope::Both,
            };
            handle_rewind(&orchestrator.project_root, scope, tx).await;
            true
        }
        _ => false,
    }
}

fn help_text() -> String {
    let parts: Vec<String> = COMMANDS
        .iter()
        .map(|(name, args, desc)| {
            let mut s = format!("/{name}");
            if !args.is_empty() {
                s.push(' ');
                s.push_str(args);
            }
            if !desc.is_empty() {
                s.push_str(&format!(" ({desc})"));
            }
            s
        })
        .collect();
    format!("commands: {} · plus any .stepper/commands/*.md", parts.join(" · "))
}

/// `/init` — ensure the `.stepper/` directory skeleton exists.
async fn handle_init(project_root: &Path, tx: &EventTx) {
    match stepper_config::scaffold::ensure_skeleton(project_root) {
        Ok(()) => {
            notice(
                tx,
                NoticeLevel::Info,
                "ensured .stepper/ skeleton (layer/ commands/ skills/ output-styles/)".into(),
            )
            .await
        }
        Err(e) => notice(tx, NoticeLevel::Warn, format!("init failed: {e}")).await,
    }
}

/// `/scaffold-layer` — write a default plan→implement→review pipeline and, if no
/// pipeline is configured yet, wire it into `setting.json` `step`.
async fn handle_scaffold_layer(project_root: &Path, tx: &EventTx) {
    use stepper_config::scaffold;
    match scaffold::scaffold_default_pipeline(project_root) {
        Ok(created) => {
            let set = scaffold::set_pipeline_steps_if_empty(project_root, &scaffold::pipeline_step_names())
                .unwrap_or(false);
            let made = if created.is_empty() {
                "pipeline layers already existed".to_string()
            } else {
                format!("wrote {} layer file(s): plan → implement → review", created.len())
            };
            let step = if set {
                " and set step:[plan,implement,review]"
            } else {
                " (kept your existing step pipeline)"
            };
            notice(
                tx,
                NoticeLevel::Info,
                format!("{made}{step} — restart stepper to run the pipeline"),
            )
            .await;
        }
        Err(e) => notice(tx, NoticeLevel::Warn, format!("scaffold-layer failed: {e}")).await,
    }
}

/// `/layer <name>` — scaffold a single custom layer.
async fn handle_new_layer(name: &str, project_root: &Path, tx: &EventTx) {
    use stepper_config::scaffold;
    if !scaffold::is_safe_name(name) {
        notice(
            tx,
            NoticeLevel::Warn,
            "usage: /layer <name> — letters, digits, '-' and '_' only".into(),
        )
        .await;
        return;
    }
    match scaffold::scaffold_layer(project_root, name, &format!("The {name} layer.")) {
        Ok(Some(path)) => {
            notice(
                tx,
                NoticeLevel::Info,
                format!("created {} — add \"{name}\" to setting.json step and restart", path.display()),
            )
            .await
        }
        Ok(None) => {
            notice(tx, NoticeLevel::Warn, format!("layer/{name}/index.md already exists")).await
        }
        Err(e) => notice(tx, NoticeLevel::Warn, format!("creating layer failed: {e}")).await,
    }
}

/// `/command <name>` — scaffold a single user slash command.
async fn handle_new_command(name: &str, project_root: &Path, tx: &EventTx) {
    use stepper_config::scaffold;
    if !scaffold::is_safe_name(name) {
        notice(
            tx,
            NoticeLevel::Warn,
            "usage: /command <name> — letters, digits, '-' and '_' only".into(),
        )
        .await;
        return;
    }
    match scaffold::scaffold_command(project_root, name) {
        Ok(Some(path)) => {
            notice(
                tx,
                NoticeLevel::Info,
                format!("created {} — restart to use /{name}", path.display()),
            )
            .await
        }
        Ok(None) => {
            notice(tx, NoticeLevel::Warn, format!("commands/{name}.md already exists")).await
        }
        Err(e) => notice(tx, NoticeLevel::Warn, format!("creating command failed: {e}")).await,
    }
}

/// `/import [claude|codex|cursor|gemini|all] [apply]` — preview (default) or apply a migration
/// of another agent's global config into `~/.stepper/`. Without `apply` it only
/// shows the plan; `/import apply` commits it. The migration is non-destructive
/// and idempotent, so previewing then applying is safe.
async fn handle_import(args: &str, home: Option<&Path>, tx: &EventTx) {
    let Some(home) = home else {
        notice(tx, NoticeLevel::Warn, "cannot locate your home directory — set HOME".into()).await;
        return;
    };
    let mut from = stepper_config::ImportFrom::All;
    let mut apply = false;
    for token in args.split_whitespace() {
        match token.to_ascii_lowercase().as_str() {
            "apply" => apply = true,
            "claude" => from = stepper_config::ImportFrom::Claude,
            "codex" => from = stepper_config::ImportFrom::Codex,
            "cursor" => from = stepper_config::ImportFrom::Cursor,
            "gemini" => from = stepper_config::ImportFrom::Gemini,
            "all" => from = stepper_config::ImportFrom::All,
            _ => {
                notice(
                    tx,
                    NoticeLevel::Warn,
                    format!("usage: /import [claude|codex|cursor|gemini|all] [apply] (got '{token}')"),
                )
                .await;
                return;
            }
        }
    }
    let plan = match stepper_config::build_plan(home, from) {
        Ok(plan) => plan,
        Err(e) => {
            notice(tx, NoticeLevel::Warn, format!("import failed: {e}")).await;
            return;
        }
    };
    let preview = stepper_config::render_preview(&plan);
    if !apply || plan.is_empty() {
        let hint = if plan.is_empty() {
            String::new()
        } else {
            // Echo the previewed source so following the hint applies the same scope.
            let apply_cmd = match from {
                stepper_config::ImportFrom::Claude => "/import claude apply",
                stepper_config::ImportFrom::Codex => "/import codex apply",
                stepper_config::ImportFrom::Cursor => "/import cursor apply",
                stepper_config::ImportFrom::Gemini => "/import gemini apply",
                stepper_config::ImportFrom::All => "/import apply",
            };
            format!("\nrun `{apply_cmd}` to write it now (no further prompt; the migration is non-destructive)")
        };
        notice(tx, NoticeLevel::Info, format!("{preview}{hint}")).await;
        return;
    }
    match stepper_config::apply_plan(&plan) {
        Ok(summary) => {
            notice(
                tx,
                NoticeLevel::Info,
                format!(
                    "{preview}\nimported: {} stepper.md section(s), {} setting.json, {} file(s) — restart to pick up changes",
                    summary.sections_appended,
                    if summary.settings_written { "wrote" } else { "no change to" },
                    summary.files_copied,
                ),
            )
            .await;
        }
        Err(e) => notice(tx, NoticeLevel::Warn, format!("import apply failed: {e}")).await,
    }
}

fn fmt_usage(u: &Usage) -> String {
    format!(
        "{} in · {} out · {} cache-read · {} cache-write",
        u.input, u.output, u.cache_read, u.cache_write
    )
}

fn cost_text(cost: &SessionCost) -> String {
    format!(
        "cost: session {} · ${:.4} | last turn {} · ${:.4}",
        fmt_usage(&cost.usage),
        cost.cost_usd,
        fmt_usage(&cost.turn_usage),
        cost.turn_cost_usd
    )
}

/// `/compact [instructions]`: fold the persisted conversation now (same
/// cut-point rules as auto-compaction: the recent tail is kept and a tool call
/// is never split from its results), summarize the folded prefix — steered by
/// the optional instructions — and reseed/persist the compacted history. The
/// freed figure is the honest before/after estimate delta.
async fn handle_compact(
    args: &str,
    orchestrator: &mut Orchestrator,
    session: &mut SessionRecord,
    turn_id: &mut u64,
    store: &SessionStore,
    tx: &EventTx,
) {
    let mut messages = session.seed_messages();
    if messages.is_empty() {
        notice(tx, NoticeLevel::Warn, "nothing to compact (no conversation yet)".into()).await;
        return;
    }
    let context_window = orchestrator
        .steps
        .first()
        .map(|s| orchestrator.resolver.model_info(&s.model_ref).context_window)
        .unwrap_or(0);
    let compactor = Compactor::new(context_window.max(1));
    // u64::MAX forces the fold regardless of the soft threshold; `plan` still
    // refuses when the history fits the keep-recent tail or has no safe cut.
    let Some(cut) = compactor.plan(&messages, u64::MAX) else {
        notice(
            tx,
            NoticeLevel::Info,
            "nothing to compact (conversation already fits the recent tail)".into(),
        )
        .await;
        return;
    };
    let _ = tx.send(AppEvent::CompactionStarted).await;
    let before = estimate_tokens(&messages);
    let dropped: Vec<stepper_provider::Message> = messages.drain(0..cut).collect();
    let trimmed = args.trim();
    let instructions = (!trimmed.is_empty()).then_some(trimmed);
    let summarizer = orchestrator
        .compaction_model
        .as_ref()
        .and_then(|m| orchestrator.resolver.resolve(m).ok());
    let summary = match summarizer {
        // Manual `/compact` runs outside an active turn, so there is no turn-cancel
        // to thread; a fresh (never-cancelled) token keeps the prior behavior.
        Some(p) => crate::compaction::summarize_with_model(
            p.as_ref(),
            &dropped,
            instructions,
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap_or_else(|| crate::compaction::heuristic_summary(&dropped)),
        None => crate::compaction::heuristic_summary(&dropped),
    };
    messages.insert(0, crate::compaction::marker(&summary));
    let freed_tokens = before.saturating_sub(estimate_tokens(&messages));

    // The compacted history replaces the persisted turns as one synthetic turn,
    // and reseeds the live conversation for the next turn. Roll the prior turns'
    // usage/cost into it so cross-session `stepper stats` totals survive a compaction
    // instead of being silently zeroed.
    let prior_usage = session.turns.iter().fold(Usage::default(), |mut acc, t| {
        acc.add(&t.usage);
        acc
    });
    let prior_cost = session.turns.iter().map(|t| t.cost_usd).sum();
    session.turns = vec![TurnRecord {
        user: "/compact".into(),
        summaries: vec![("compact".into(), summary)],
        messages: messages.clone(),
        usage: prior_usage,
        cost_usd: prior_cost,
        // Stamp the compaction moment so the rolled-up usage still falls inside a
        // `stats --days` window (an absent timestamp would drop it from every
        // windowed view — the opposite of preserving it).
        ended_at: crate::now_unix_secs(),
        ..Default::default()
    }];
    *turn_id = session.turns.len() as u64;
    let _ = store.save(session);
    orchestrator.resume_seed = messages;
    let _ = tx.send(AppEvent::CompactionDone { freed_tokens }).await;
}

/// Write `model` as `defaultModel` into the project `.stepper/setting.json` (if a
/// project config dir exists), else the user's `~/.stepper/setting.json`. `Ok(true)`
/// when written. Errors (e.g. an unparseable destination) are returned, not fatal.
fn persist_default_model(orchestrator: &Orchestrator, model: &str) -> std::io::Result<bool> {
    let project = orchestrator.project_root.join(".stepper");
    let dir = if project.is_dir() {
        project
    } else if let Some(home) = orchestrator.home.as_ref() {
        home.join(".stepper")
    } else {
        return Ok(false);
    };
    stepper_config::scaffold::update_settings(&dir, |obj| {
        obj.insert("defaultModel".into(), serde_json::Value::String(model.to_string()));
    })?;
    Ok(true)
}

/// `/effort [off|low|medium|high|xhigh|max]` — show or set the session reasoning
/// effort. With no argument it opens the TUI picker (current level highlighted).
/// An explicit set applies to every step (overriding per-layer frontmatter for
/// this session) and persists as the project default.
async fn handle_effort(arg: &str, orchestrator: &mut Orchestrator, tx: &EventTx) {
    if arg.is_empty() {
        let current = orchestrator
            .steps
            .first()
            .and_then(|s| s.reasoning_effort.clone())
            .unwrap_or_else(|| "off".into());
        let _ = tx.send(AppEvent::OpenEffortPicker { current }).await;
        return;
    }
    let level = arg.to_ascii_lowercase();
    if !matches!(level.as_str(), "off" | "low" | "medium" | "high" | "xhigh" | "max") {
        notice(
            tx,
            NoticeLevel::Warn,
            format!("unknown effort '{arg}' — use off | low | medium | high | xhigh | max"),
        )
        .await;
        return;
    }
    // An explicit /effort overrides every step's reasoning controls this session.
    let (re, tb) = crate::setup::effort_controls(&level);
    for step in &mut orchestrator.steps {
        step.reasoning_effort = re.clone();
        step.thinking_budget = tb;
    }
    let persisted = persist_effort(orchestrator, &level);
    let _ = tx
        .send(AppEvent::EffortChanged((level != "off").then(|| level.clone())))
        .await;
    let suffix = match persisted {
        Ok(true) => "",
        _ => " (not persisted — no .stepper/)",
    };
    notice(tx, NoticeLevel::Info, format!("reasoning effort → {level}{suffix}")).await;
}

fn persist_effort(orchestrator: &Orchestrator, level: &str) -> std::io::Result<bool> {
    let project = orchestrator.project_root.join(".stepper");
    let dir = if project.is_dir() {
        project
    } else if let Some(home) = orchestrator.home.as_ref() {
        home.join(".stepper")
    } else {
        return Ok(false);
    };
    stepper_config::scaffold::update_settings(&dir, |obj| {
        obj.insert("reasoningEffort".into(), serde_json::Value::String(level.to_string()));
    })?;
    Ok(true)
}

async fn handle_model(arg: &str, orchestrator: &mut Orchestrator, tx: &EventTx) {
    if arg.is_empty() {
        let current = orchestrator
            .steps
            .first()
            .map(|s| s.model_ref.clone())
            .unwrap_or_else(|| "<none>".into());
        notice(tx, NoticeLevel::Info, format!("current model: {current}")).await;
        return;
    }
    // Validate by resolving before committing the switch.
    match orchestrator.resolver.resolve(arg) {
        Ok(provider) => {
            let view = ModelView {
                provider: provider.provider().to_string(),
                model: provider.model().to_string(),
            };
            // Switch only the primary (first) layer — overwriting every step would
            // flatten an intentional per-layer model pipeline. This matches the
            // no-arg display and /context, which both read steps.first().
            if let Some(step) = orchestrator.steps.first_mut() {
                step.model_ref = arg.to_string();
            }
            let _ = tx.send(AppEvent::ModelChanged(view)).await;
            // Persist as `defaultModel` so the pick survives a restart (without it,
            // the switch evaporates and the next launch reverts to the configured/
            // built-in default). Project `.stepper` wins, else the user's.
            let persisted = persist_default_model(orchestrator, arg);
            notice(
                tx,
                NoticeLevel::Info,
                match persisted {
                    Ok(true) => format!("primary model switched to {arg} (saved as default)"),
                    _ => format!("primary model switched to {arg}"),
                },
            )
            .await;
        }
        Err(e) => {
            notice(
                tx,
                NoticeLevel::Warn,
                format!("cannot switch to '{arg}': {e}"),
            )
            .await;
            // No key for this provider → offer to set one right away.
            if matches!(e, CoreError::Provider(stepper_provider::ProviderError::Auth(_)))
                && let Some(provider) = arg.split('/').next().filter(|p| !p.is_empty())
            {
                let _ = tx
                    .send(AppEvent::ApiKeyPrompt { provider: provider.to_string() })
                    .await;
            }
        }
    }
}

/// `/login [provider]`: open the TUI's API-key entry overlay for `provider`
/// (defaulting to the current primary model's provider). `provider/model` is
/// accepted too — only the provider segment is used.
async fn handle_login(arg: &str, orchestrator: &Orchestrator, tx: &EventTx) {
    let provider = if arg.is_empty() {
        orchestrator
            .steps
            .first()
            .and_then(|s| s.model_ref.split('/').next())
            .unwrap_or("")
            .to_string()
    } else {
        arg.split('/').next().unwrap_or(arg).trim().to_string()
    };
    if provider.is_empty() {
        notice(tx, NoticeLevel::Warn, "usage: /login <provider>".into()).await;
        return;
    }
    let _ = tx.send(AppEvent::ApiKeyPrompt { provider }).await;
}

/// `/models`: fetch the selectable models (each configured provider's live list
/// merged with the models.dev catalog) and hand them to the TUI as a picker. The
/// picker's selection comes back as `/model <ref>`, reusing `handle_model`.
async fn handle_models(orchestrator: &Orchestrator, tx: &EventTx) {
    notice(tx, NoticeLevel::Info, "fetching models…".into()).await;
    let models = orchestrator.resolver.list_models().await;
    if models.is_empty() {
        notice(
            tx,
            NoticeLevel::Warn,
            "no models found (check provider keys / connectivity, or use /model provider/id)".into(),
        )
        .await;
        return;
    }
    let _ = tx.send(AppEvent::ModelList(models)).await;
}

/// `/connect [provider]`: with no arg, fetch the models.dev provider seed and
/// open the picker; with a provider id, register it (wire kind + base URL derived
/// from the catalog) into the live config *and* `setting.json`, then prompt for
/// its API key (which the existing `/login` overlay stores in the OS keyring).
async fn handle_connect(arg: &str, orchestrator: &Orchestrator, tx: &EventTx) {
    if arg.is_empty() {
        notice(tx, NoticeLevel::Info, "fetching providers…".into()).await;
        let providers = orchestrator.resolver.list_providers().await;
        if providers.is_empty() {
            notice(
                tx,
                NoticeLevel::Warn,
                "no providers found (models.dev unreachable?) — or use /login <provider>".into(),
            )
            .await;
            return;
        }
        let _ = tx.send(AppEvent::ProviderList(providers)).await;
        return;
    }
    match orchestrator.resolver.connect_provider(arg).await {
        Ok(connected) => {
            let suffix = match persist_provider(orchestrator, arg, &connected) {
                Ok(true) => "",
                _ => " (not persisted — no .stepper/)",
            };
            notice(
                tx,
                NoticeLevel::Info,
                format!("added provider '{arg}'{suffix} — enter its API key"),
            )
            .await;
            // Reuse the `/login` key overlay; the key lands in the OS keyring and
            // the next resolve picks it up (no restart).
            let _ = tx.send(AppEvent::ApiKeyPrompt { provider: arg.to_string() }).await;
        }
        Err(e) => notice(tx, NoticeLevel::Warn, format!("cannot connect '{arg}': {e}")).await,
    }
}

/// Merge a freshly connected provider into `setting.json` `providers` (project
/// `.stepper/` wins, else user `~/.stepper/`) without clobbering existing ones.
/// `Ok(false)` when there is nowhere to persist (no `.stepper/`, no home).
fn persist_provider(
    orchestrator: &Orchestrator,
    id: &str,
    connected: &ConnectedProvider,
) -> std::io::Result<bool> {
    let project = orchestrator.project_root.join(".stepper");
    let dir = if project.is_dir() {
        project
    } else if let Some(home) = orchestrator.home.as_ref() {
        home.join(".stepper")
    } else {
        return Ok(false);
    };
    stepper_config::scaffold::update_settings(&dir, |obj| {
        let providers = obj
            .entry("providers")
            .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
        if let Some(map) = providers.as_object_mut() {
            // Merge into the existing entry (if any): refresh the wire `kind` and
            // fill `baseUrl` only when absent, preserving any apiKey/defaultModel/
            // auth/contextWindow the user already had — never a whole-object replace.
            let entry = map
                .entry(id.to_string())
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
            if let Some(obj) = entry.as_object_mut() {
                // Fill kind/baseUrl only when absent — preserve a user's existing
                // dialect (openai-responses/codex) and base, like the live path.
                obj.entry("kind".to_string())
                    .or_insert_with(|| serde_json::Value::String(connected.kind.clone()));
                if let Some(base) = &connected.base_url {
                    obj.entry("baseUrl".to_string())
                        .or_insert_with(|| serde_json::Value::String(base.clone()));
                }
            }
        }
    })?;
    Ok(true)
}

/// chars/4, the same estimator the compactor uses for free text.
fn estimate_str(s: &str) -> u64 {
    (s.len() / 4) as u64
}

/// `/context`: estimate where the primary layer's window goes. Categories are
/// computed from the same inputs `run_turn` assembles — base context (memory),
/// the layer system prompt (with its skills advertisement split out), the
/// layer-filtered tool specs (built-in vs MCP), and the persisted conversation.
async fn handle_context(orchestrator: &Orchestrator, session: &SessionRecord, tx: &EventTx) {
    let Some(step) = orchestrator.steps.first() else {
        notice(tx, NoticeLevel::Warn, "no model configured".into()).await;
        return;
    };
    let info = orchestrator.resolver.model_info(&step.model_ref);
    let ads = crate::skills::advertise(&step.skills);
    let skills = estimate_str(&ads);
    // The real system message also carries the universal agentic directives that
    // compose_system() prepends to every layer — count them so the breakdown is honest.
    let system_prompt = (estimate_str(&step.system_prompt) + estimate_str(crate::AGENT_DIRECTIVES))
        .saturating_sub(skills);
    let memory = estimate_str(&orchestrator.base_context);
    let registry = orchestrator
        .base_tools
        .filtered(&step.tool_allow, &step.tool_deny)
        .filter_mcp(&step.mcp_allow, &orchestrator.always_load_mcp);
    let (mut tools, mut mcp_tools) = (0u64, 0u64);
    for spec in registry.specs() {
        let size = (spec.name.len()
            + spec.description.len()
            + spec.input_schema.to_string().len()) as u64
            / 4;
        if spec.name.starts_with("mcp__") {
            mcp_tools += size;
        } else {
            tools += size;
        }
    }
    let messages = estimate_tokens(&session.seed_messages());
    let used = system_prompt + tools + mcp_tools + skills + memory + messages;
    let breakdown = ContextBreakdownView {
        system_prompt,
        tools,
        mcp_tools,
        skills,
        memory,
        messages,
        free: info.context_window.saturating_sub(used),
        context_limit: info.context_window,
    };
    let _ = tx.send(AppEvent::ContextBreakdown(breakdown)).await;
}

/// The default rules `stepper init` scaffolds into a project `setting.json` —
/// shown with source `scaffold` so a user can tell them from hand-written rules.
const SCAFFOLD_RULES: &[(&str, &str)] = &[
    ("allow", "Read(/**)"),
    ("allow", "Bash(cargo *)"),
    ("ask", "Bash(git push:*)"),
    ("deny", "Read(//etc/**)"),
    ("deny", "Bash(rm -rf *)"),
];

fn mode_label(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Auto => "auto",
        PermissionMode::Plan => "plan",
        PermissionMode::AcceptEdits => "accept-edits",
        PermissionMode::Default => "default",
        PermissionMode::DontAsk => "dont-ask",
        PermissionMode::Bypass => "bypass",
    }
}

/// `/permissions`: a read-only snapshot of the rules and persisted approvals as
/// they stand in the settings files right now (user + project scopes read
/// separately so each rule names its source).
/// `/allow|/ask|/deny <spec>` — validate the spec, fold it into the live rules
/// (so it applies immediately), and persist it to setting.json `permissions`.
async fn handle_permission_rule(verdict: &str, spec: &str, orchestrator: &mut Orchestrator, tx: &EventTx) {
    if spec.is_empty() {
        notice(tx, NoticeLevel::Warn, format!("usage: /{verdict} <spec>  (e.g. /{verdict} Bash(cargo *))")).await;
        return;
    }
    if Rule::parse(spec).is_none() {
        notice(tx, NoticeLevel::Warn, format!("malformed permission spec: {spec}")).await;
        return;
    }
    // Live-fold into the matching bucket so it applies on the next tool call.
    let specs = [spec.to_string()];
    {
        let mut w = orchestrator.rules.write().unwrap();
        *w = match verdict {
            "allow" => w.extended(&specs, &[], &[]),
            "ask" => w.extended(&[], &specs, &[]),
            _ => w.extended(&[], &[], &specs),
        };
    }
    // Persist to setting.json permissions.{allow|ask|deny} (de-duped).
    let project = orchestrator.project_root.join(".stepper");
    let dir = if project.is_dir() {
        Some(project)
    } else {
        orchestrator.home.as_ref().map(|h| h.join(".stepper"))
    };
    if let Some(dir) = dir {
        let bucket = verdict.to_string();
        let spec_owned = spec.to_string();
        let result = stepper_config::scaffold::update_settings(&dir, |obj| {
            let perms = obj
                .entry("permissions")
                .or_insert_with(|| serde_json::json!({}));
            if let Some(perms) = perms.as_object_mut() {
                let arr = perms
                    .entry(bucket)
                    .or_insert_with(|| serde_json::Value::Array(Vec::new()));
                if let Some(arr) = arr.as_array_mut()
                    && !arr.iter().any(|v| v.as_str() == Some(spec_owned.as_str()))
                {
                    arr.push(serde_json::Value::String(spec_owned));
                }
            }
        });
        if let Err(e) = result {
            notice(tx, NoticeLevel::Warn, format!("rule applied but not persisted: {e}")).await;
            return;
        }
    }
    handle_permissions(orchestrator, tx).await;
}

async fn handle_permissions(orchestrator: &Orchestrator, tx: &EventTx) {
    let mut rules = Vec::new();
    let mut approvals = Vec::new();
    let user_dir = orchestrator.home.as_ref().map(|h| h.join(".stepper"));
    let project_dir = orchestrator.project_root.join(".stepper");
    for (dir, scope) in [(user_dir, "user"), (Some(project_dir), "project")] {
        let Some(settings) = dir.and_then(|d| read_settings(&d)) else {
            continue;
        };
        let lists = [
            ("allow", &settings.permissions.allow),
            ("ask", &settings.permissions.ask),
            ("deny", &settings.permissions.deny),
        ];
        for (verdict, list) in lists {
            for rule in list {
                let scaffolded = scope == "project"
                    && SCAFFOLD_RULES.contains(&(verdict, rule.as_str()));
                rules.push(PermissionRuleView {
                    verdict: verdict.into(),
                    rule: rule.clone(),
                    source: if scaffolded { "scaffold".into() } else { scope.into() },
                });
            }
        }
        for approval in &settings.approvals {
            approvals.push(ApprovalRuleView {
                rule: approval.rule.clone(),
                scope: approval.scope.clone(),
                granted_at: approval.granted_at.clone(),
            });
        }
    }
    let snapshot = PermissionsSnapshotView {
        mode: mode_label(orchestrator.mode_snapshot()).into(),
        rules,
        approvals,
    };
    let _ = tx.send(AppEvent::PermissionsSnapshot(snapshot)).await;
}

fn row(label: &str, value: impl Into<String>) -> SettingsRowView {
    SettingsRowView {
        label: label.into(),
        value: value.into(),
    }
}

/// `/settings` — a consolidated, read-only overview of the session's settings,
/// grouped into tabs. Live values (mode/effort/model) come from the orchestrator;
/// persisted ones (theme/mcp/notifications) are read from `.stepper/setting.json`
/// (project over user). Enter on a tab with a `jump` opens its dedicated editor.
async fn handle_settings(orchestrator: &Orchestrator, tx: &EventTx) {
    let user = orchestrator
        .home
        .as_ref()
        .map(|h| h.join(".stepper"))
        .and_then(|d| read_settings(&d));
    let project = read_settings(&orchestrator.project_root.join(".stepper"));
    let pick = |f: fn(&stepper_config::SettingsFile) -> Option<String>| {
        project.as_ref().and_then(f).or_else(|| user.as_ref().and_then(f))
    };

    let effort = orchestrator
        .steps
        .first()
        .and_then(|s| s.reasoning_effort.clone())
        .unwrap_or_else(|| "off".into());

    let general = SettingsTabView {
        title: "General".into(),
        rows: vec![
            row("mode", mode_label(orchestrator.mode_snapshot())),
            row("reasoning effort", effort),
            row("cwd", orchestrator.cwd.display().to_string()),
            row("project root", orchestrator.project_root.display().to_string()),
        ],
        jump: None,
    };

    let model = SettingsTabView {
        title: "Model".into(),
        rows: orchestrator
            .steps
            .iter()
            .map(|s| {
                let eff = s.reasoning_effort.as_deref().unwrap_or("off");
                row(&s.name, format!("{}  ·  effort {eff}", s.model_ref))
            })
            .collect(),
        jump: Some("model".into()),
    };

    let (allow, ask, deny) = {
        let r = orchestrator.rules.read().unwrap();
        (r.allow.len(), r.ask.len(), r.deny.len())
    };
    let permissions = SettingsTabView {
        title: "Permissions".into(),
        rows: vec![
            row("mode", mode_label(orchestrator.mode_snapshot())),
            row("allow rules", allow.to_string()),
            row("ask rules", ask.to_string()),
            row("deny rules", deny.to_string()),
        ],
        jump: Some("permissions".into()),
    };

    let theme = SettingsTabView {
        title: "Theme".into(),
        rows: vec![row(
            "preset",
            pick(|s| s.theme.as_ref().and_then(|t| t.preset.clone())).unwrap_or_else(|| "dark (default)".into()),
        )],
        jump: Some("theme".into()),
    };

    let mut mcp_servers: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    for src in [user.as_ref(), project.as_ref()].into_iter().flatten() {
        for (name, cfg) in &src.mcp_servers {
            let transport = cfg.transport.clone().unwrap_or_else(|| "stdio".into());
            mcp_servers.insert(name.clone(), transport);
        }
    }
    let mcp = SettingsTabView {
        title: "MCP".into(),
        rows: if mcp_servers.is_empty() {
            vec![row("servers", "none configured")]
        } else {
            mcp_servers.into_iter().map(|(name, transport)| row(&name, transport)).collect()
        },
        jump: None,
    };

    let notifications = SettingsTabView {
        title: "Notifications".into(),
        rows: vec![row(
            "bell",
            match project.as_ref().and_then(|s| s.notification.as_ref()).or_else(|| user.as_ref().and_then(|s| s.notification.as_ref())) {
                Some(n) => {
                    let (complete, approval, error) = n.resolve();
                    format!("complete:{complete} approval:{approval} error:{error}")
                }
                None => "off (default)".into(),
            },
        )],
        jump: None,
    };

    let snapshot = SettingsSnapshotView {
        tabs: vec![general, model, permissions, theme, mcp, notifications],
    };
    let _ = tx.send(AppEvent::SettingsSnapshot(snapshot)).await;
}

fn read_settings(dir: &Path) -> Option<stepper_config::SettingsFile> {
    let raw = std::fs::read_to_string(dir.join("setting.json")).ok()?;
    // JSONC, mirroring `Config::load`, so a commented `setting.json` shows real
    // values in `/settings` instead of being silently read as empty.
    let value = stepper_config::parse_setting_jsonc(&raw).ok()?;
    serde_json::from_value(value).ok()
}

/// `/rewind` (and the TUI's Esc-Esc): list the `turn-N` working-tree
/// checkpoints, newest first; the TUI picker sends the selection back as
/// `Action::Rewind` carrying `scope` (files / conversation / both).
async fn handle_rewind(project_root: &Path, scope: stepper_protocol::RewindScope, tx: &EventTx) {
    let dir = project_root.join(".stepper").join("checkpoints");
    let mut turns: Vec<u64> = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| {
            e.file_name()
                .to_str()
                .and_then(|name| name.strip_prefix("turn-"))
                .and_then(|n| n.parse().ok())
        })
        .collect();
    if turns.is_empty() {
        notice(tx, NoticeLevel::Warn, "no checkpoints to rewind to yet".into()).await;
        return;
    }
    turns.sort_unstable_by(|a, b| b.cmp(a));
    let checkpoints = turns
        .into_iter()
        .map(|turn| CheckpointView {
            id: format!("turn-{turn}"),
            turn,
        })
        .collect();
    let _ = tx.send(AppEvent::CheckpointList { checkpoints, scope }).await;
}

/// How many sessions the `/resume` picker lists.
const RESUME_PICKER_LIMIT: usize = 20;

/// `/resume`: list recent sessions (newest first); the TUI picker sends the
/// selection back as `Action::Resume`.
async fn handle_resume(store: &SessionStore, tx: &EventTx) {
    let recent = store.list_recent(RESUME_PICKER_LIMIT);
    if recent.is_empty() {
        notice(tx, NoticeLevel::Warn, "no saved sessions to resume".into()).await;
        return;
    }
    let now = std::time::SystemTime::now();
    let sessions = recent
        .into_iter()
        .map(|(record, modified)| SessionView {
            digest: record
                .turns
                .first()
                .and_then(|t| t.user.lines().next())
                .unwrap_or("(empty)")
                .to_string(),
            turns: record.turns.len(),
            age: age_label(now, modified),
            id: record.id,
            name: record.name,
        })
        .collect();
    let _ = tx.send(AppEvent::SessionList(sessions)).await;
}

/// A short human age (`just now` / `3m ago` / `2h ago` / `5d ago`) for a session
/// modified time — shared by the `/resume` picker and the `session list` CLI.
pub fn age_label(now: std::time::SystemTime, modified: std::time::SystemTime) -> String {
    let secs = now
        .duration_since(modified)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    match secs {
        0..60 => "just now".into(),
        60..3600 => format!("{}m ago", secs / 60),
        3600..86_400 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86_400),
    }
}

async fn notice(tx: &EventTx, level: NoticeLevel, text: String) {
    let _ = tx.send(AppEvent::Notice { level, text }).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptions_cover_every_name_and_are_non_empty() {
        let names = names();
        let descs = descriptions();
        assert_eq!(descs.len(), names.len(), "one description per command");
        for (name, desc) in &descs {
            assert!(names.contains(&name.to_string()), "{name} is a known command");
            assert!(!desc.is_empty(), "{name} has a description");
        }
    }

    #[test]
    fn help_text_lists_every_command_with_its_args() {
        let help = help_text();
        assert!(help.contains("/import [claude|codex|cursor|gemini|all] [apply]"), "arg hints kept: {help}");
        assert!(help.contains("/compact [instructions]"));
        for (name, _, _) in COMMANDS {
            assert!(help.contains(&format!("/{name}")), "/{name} listed in help");
        }
    }
}
