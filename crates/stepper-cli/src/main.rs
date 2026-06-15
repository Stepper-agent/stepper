mod cli;
mod core_setup;
mod onboarding;

use clap::Parser;
use cli::{AuthCmd, Cli, Command, GlobalArgs};
use core_setup::{build_orchestrator_with_fallback, DEFAULT_MODEL};
use std::io::Write;
use stepper_core::{spawn_core, SessionLimits, SessionRecord, SessionStore};
use stepper_permission::{PermissionMode, RuleSet};
use stepper_protocol::{
    Action, AppEvent, ApprovalDecision, ApprovalKind, Mode, ModelView,
};
use stepper_providers::codex::{oauth, CodexTokenStore};
use stepper_providers::ProviderFactory;
use stepper_tui::{run_tui, TuiInit};
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        None | Some(Command::Run) => launch(cli.global).await,
        Some(Command::Auth(args)) => match args.cmd {
            AuthCmd::Login(login) => auth_login(login.codex).await,
            AuthCmd::SetKey { provider } => set_key(&provider),
            AuthCmd::DeleteKey { provider } => delete_key(&provider),
        },
        Some(Command::Config(args)) => config_cmd(args, cli.global),
        Some(Command::Init) => init_cmd(cli.global),
        Some(Command::Layer { name }) => scaffold_layer_cmd(&name, cli.global),
        Some(Command::Cmd { name }) => scaffold_command_cmd(&name, cli.global),
        Some(Command::ScaffoldLayer) => scaffold_pipeline_cmd(cli.global),
    }
}

fn global_cwd(global: &GlobalArgs) -> anyhow::Result<std::path::PathBuf> {
    match &global.cwd {
        Some(p) => Ok(p.clone()),
        None => Ok(std::env::current_dir()?),
    }
}

fn scaffold_layer_cmd(name: &str, global: GlobalArgs) -> anyhow::Result<()> {
    let cwd = global_cwd(&global)?;
    if !stepper_config::scaffold::is_safe_name(name) {
        anyhow::bail!("invalid layer name '{name}' — letters, digits, '-' and '_' only");
    }
    match stepper_config::scaffold::scaffold_layer(&cwd, name, &format!("The {name} layer."))? {
        Some(path) => println!("wrote {} — add \"{name}\" to setting.json step", path.display()),
        None => println!("layer/{name}/index.md already exists"),
    }
    Ok(())
}

fn scaffold_command_cmd(name: &str, global: GlobalArgs) -> anyhow::Result<()> {
    let cwd = global_cwd(&global)?;
    if !stepper_config::scaffold::is_safe_name(name) {
        anyhow::bail!("invalid command name '{name}' — letters, digits, '-' and '_' only");
    }
    match stepper_config::scaffold::scaffold_command(&cwd, name)? {
        Some(path) => println!("wrote {} — use /{name} in the TUI", path.display()),
        None => println!("commands/{name}.md already exists"),
    }
    Ok(())
}

fn scaffold_pipeline_cmd(global: GlobalArgs) -> anyhow::Result<()> {
    use stepper_config::scaffold;
    let cwd = global_cwd(&global)?;
    scaffold::ensure_skeleton(&cwd)?;
    let created = scaffold::scaffold_default_pipeline(&cwd)?;
    for path in &created {
        println!("wrote {}", path.display());
    }
    if scaffold::set_pipeline_steps_if_empty(&cwd, &scaffold::pipeline_step_names())? {
        println!("set step:[plan, implement, review] in setting.json");
    } else if created.is_empty() {
        println!("the default pipeline already exists");
    } else {
        println!("kept your existing step pipeline (add the new layers to it manually)");
    }
    Ok(())
}

fn config_cmd(args: cli::ConfigArgs, global: GlobalArgs) -> anyhow::Result<()> {
    let cwd = global
        .cwd
        .clone()
        .map(Ok)
        .unwrap_or_else(std::env::current_dir)?;

    if args.schema {
        println!(
            "{}",
            serde_json::to_string_pretty(&stepper_config::settings_schema())?
        );
        return Ok(());
    }
    if args.validate {
        match stepper_config::Config::load(&cwd) {
            Ok(cfg) => {
                let mut problems = cfg.validate_values();
                let perms = &cfg.settings.permissions;
                match RuleSet::from_lists_checked(&perms.allow, &perms.ask, &perms.deny) {
                    Ok((_, dropped)) => problems.extend(dropped.iter().map(|spec| {
                        format!("permissions: malformed allow/ask rule (ignored at startup): {spec}")
                    })),
                    Err(e) => problems.push(format!("permissions: {e}")),
                }
                if !problems.is_empty() {
                    for problem in &problems {
                        eprintln!("error: {problem}");
                    }
                    anyhow::bail!("invalid config: {} problem(s) found", problems.len());
                }
                println!(
                    "setting.json is valid ({} step(s), {} provider(s))",
                    cfg.settings.step.len(),
                    cfg.settings.providers.len()
                );
                Ok(())
            }
            Err(e) => anyhow::bail!("invalid config: {e}"),
        }
    } else {
        println!("usage: stepper config --schema | --validate");
        Ok(())
    }
}

fn init_cmd(global: GlobalArgs) -> anyhow::Result<()> {
    let cwd = global
        .cwd
        .clone()
        .map(Ok)
        .unwrap_or_else(std::env::current_dir)?;
    let stepper_dir = cwd.join(".stepper");
    std::fs::create_dir_all(&stepper_dir)?;
    // Lay down the discoverable subdirectory skeleton (layer/ commands/ skills/
    // output-styles/) so the layout is obvious even before anything is authored.
    stepper_config::scaffold::ensure_skeleton(&cwd)?;

    let stepper_md = stepper_dir.join("stepper.md");
    if stepper_md.exists() {
        println!("{} already exists — leaving it untouched", stepper_md.display());
    } else {
        std::fs::write(&stepper_md, scaffold_stepper_md(&cwd))?;
        println!("wrote {}", stepper_md.display());
    }

    let setting = stepper_dir.join("setting.json");
    if !setting.exists() {
        std::fs::write(&setting, scaffold_setting_json(None, "accept-edits"))?;
        println!("wrote {}", setting.display());
    }
    Ok(())
}

pub(crate) fn scaffold_stepper_md(cwd: &std::path::Path) -> String {
    let name = cwd
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project");
    let stack = detect_stack(cwd);
    format!(
        "# {name}\n\n\
        > Base context for the stepper agent (like CLAUDE.md). Edit freely.\n\n\
        ## Stack\n\n{stack}\n\n\
        ## Conventions\n\n\
        - Keep changes minimal and idiomatic to the surrounding code.\n\
        - Run the build/tests after edits.\n\n\
        ## Notes\n\n_(add project-specific guidance here)_\n"
    )
}

fn detect_stack(cwd: &std::path::Path) -> String {
    let markers = [
        ("Cargo.toml", "Rust (cargo)"),
        ("package.json", "Node / JavaScript"),
        ("pyproject.toml", "Python"),
        ("go.mod", "Go"),
        ("pom.xml", "Java (maven)"),
    ];
    let found: Vec<&str> = markers
        .iter()
        .filter(|(file, _)| cwd.join(file).exists())
        .map(|(_, label)| *label)
        .collect();
    if found.is_empty() {
        "- _(stack not auto-detected)_".to_string()
    } else {
        found.iter().map(|l| format!("- {l}")).collect::<Vec<_>>().join("\n")
    }
}

/// The `.stepper/setting.json` scaffold. `default_model` adds a `defaultModel`
/// line when present (the first-run setup passes the chosen model); `mode` sets
/// the starting permission mode. With `None` + `"accept-edits"` the output is
/// the plain `stepper init` template.
pub(crate) fn scaffold_setting_json(default_model: Option<&str>, mode: &str) -> String {
    let default_model_line = match default_model {
        // Serialize the value through serde_json so any model string is escaped
        // into a valid JSON string (the scaffold is otherwise hand-formatted).
        Some(m) => format!(
            "  \"defaultModel\": {},\n",
            serde_json::to_string(m).unwrap_or_else(|_| "\"\"".into())
        ),
        None => String::new(),
    };
    format!(
        "{{\n  \"$schema\": \"stepper://setting.schema.json\",\n  \"step\": [],\n{default_model_line}  \"mode\": \"{mode}\",\n  \"providers\": {{}},\n  \"permissions\": {{\n    \"allow\": [\"Read(/**)\", \"Bash(cargo *)\"],\n    \"ask\": [\"Bash(git push:*)\"],\n    \"deny\": [\"Read(//etc/**)\", \"Bash(rm -rf *)\", \"Bash(rm -fr *)\", \"Bash(sudo *)\", \"Bash(git push --force *)\", \"Bash(git push -f *)\"]\n  }},\n  \"approvals\": [],\n  \"hooks\": {{}}\n}}\n"
    )
}

async fn auth_login(codex: bool) -> anyhow::Result<()> {
    if !codex {
        anyhow::bail!("only `--codex` login is supported right now");
    }
    let factory = ProviderFactory::new()?;
    let client = factory.client();
    let path = CodexTokenStore::default_path();
    let account = oauth::login(&client, &path).await?;
    println!("\nSigned in to ChatGPT. account_id={account}");
    println!("Credentials saved to {}", path.display());
    Ok(())
}

/// Store a provider's API key in the OS keyring (read from stdin so it never
/// lands in shell history).
fn set_key(provider: &str) -> anyhow::Result<()> {
    use std::io::Write;
    eprint!("Enter API key for '{provider}': ");
    std::io::stderr().flush().ok();
    let mut key = String::new();
    std::io::stdin().read_line(&mut key)?;
    let key = key.trim();
    if key.is_empty() {
        anyhow::bail!("no key provided");
    }
    stepper_providers::store_key_in_keyring(provider, key)?;
    eprintln!("stored key for '{provider}' in the OS keyring");
    Ok(())
}

fn delete_key(provider: &str) -> anyhow::Result<()> {
    stepper_providers::delete_key_from_keyring(provider)?;
    eprintln!("removed key for '{provider}' from the OS keyring");
    Ok(())
}

async fn launch(global: GlobalArgs) -> anyhow::Result<()> {
    let cwd = match &global.cwd {
        Some(p) => p.clone(),
        None => std::env::current_dir()?,
    };
    let cli_mode = resolve_mode(&global);
    let limits = SessionLimits::new(global.max_turns, global.max_budget_usd);

    if let Some(prompt) = global.print.clone() {
        return oneshot(&global, cli_mode, cwd, prompt, limits).await;
    }

    // First-run setup runs only on the interactive path (headless returned
    // above). When it writes a config its chosen model becomes this session's
    // model too, so the orchestrator and the footer agree.
    let onboarding_model = onboarding::maybe_first_run(&cwd, global.no_init)?;
    let effective_model = global.model.clone().or(onboarding_model);
    let (provider, model) = split_model(effective_model.as_deref());

    let (orchestrator, _mcp) = build_orchestrator_with_fallback(
        effective_model.as_deref(),
        global.fallback_model.as_deref(),
        cli_mode,
        cwd.clone(),
        limits,
    )
    .await?;
    let session = resume_or_fresh(
        &orchestrator.project_root,
        global.resume.as_deref(),
        global.continue_session,
        global.name.as_deref(),
    );
    // The orchestrator's mode is the resolved one (flag > setting.json > default).
    let resolved_mode = perm_to_mode(orchestrator.mode);
    // First-run / keyless start: if the active model's provider needs an API key
    // and none is resolvable (env / keyring / config), open the key overlay right
    // away by replaying a `/login <provider>` once the session is up.
    let model_ref = effective_model.clone().unwrap_or_else(|| DEFAULT_MODEL.to_string());
    let key_prompt = needs_api_key(&*orchestrator.resolver, &model_ref)
        .then(|| provider_of(&model_ref).to_string());
    let (action_tx, action_rx) = tokio::sync::mpsc::channel(64);
    let cancel = CancellationToken::new();
    let event_rx = spawn_core(orchestrator, session, action_rx, cancel.clone());
    if let Some(provider) = key_prompt {
        let _ = action_tx
            .send(Action::SlashCommand { name: "login".into(), args: provider })
            .await;
    }

    let mut commands = stepper_core::builtin_command_names();
    commands.extend(
        stepper_config::Config::load(&cwd)
            .map(|c| c.command_names())
            .unwrap_or_default(),
    );

    // `_mcp` keeps the MCP server connections open for the whole TUI session.
    let init = TuiInit {
        inline_height: 14,
        model: ModelView { provider, model },
        mode: resolved_mode,
        cwd,
        commands,
    };
    run_tui(event_rx, action_tx, init, cancel).await
}

/// Headless one-shot: drive the real agent and stream assistant text to stdout.
/// Approval prompts are DENIED by default (each denial is named on stderr);
/// `--dangerously-auto-approve` restores the old blind-approve behavior. A turn
/// that ends in an error (including a `--max-turns`/`--max-budget-usd` cap)
/// exits non-zero.
async fn oneshot(
    global: &GlobalArgs,
    cli_mode: Option<PermissionMode>,
    cwd: std::path::PathBuf,
    prompt: String,
    limits: SessionLimits,
) -> anyhow::Result<()> {
    let (orchestrator, _mcp) = build_orchestrator_with_fallback(
        global.model.as_deref(),
        global.fallback_model.as_deref(),
        cli_mode,
        cwd,
        limits,
    )
    .await?;
    let session = resume_or_fresh(
        &orchestrator.project_root,
        global.resume.as_deref(),
        global.continue_session,
        global.name.as_deref(),
    );
    let (action_tx, action_rx) = tokio::sync::mpsc::channel(64);
    let cancel = CancellationToken::new();
    let mut event_rx = spawn_core(orchestrator, session, action_rx, cancel);

    action_tx.send(Action::SubmitInput(prompt)).await?;
    let mut stdout = std::io::stdout();
    let mut turn_error: Option<String> = None;

    while let Some(event) = event_rx.recv().await {
        match event {
            AppEvent::AssistantTokenDelta(t) => {
                print!("{t}");
                stdout.flush().ok();
            }
            AppEvent::ApprovalRequested(req) => {
                if global.dangerously_auto_approve {
                    let _ = req.reply.send(ApprovalDecision::AllowOnce);
                } else {
                    eprintln!(
                        "\n[denied] {} (headless denies approval prompts; pass --dangerously-auto-approve to allow)",
                        describe_approval(&req.kind)
                    );
                    let _ = req.reply.send(ApprovalDecision::Deny);
                }
            }
            AppEvent::ToolCallStarted(view) => eprintln!("\n[tool] {}", view.summary),
            AppEvent::Error(e) => {
                eprintln!("\nerror: {e}");
                turn_error = Some(e);
            }
            AppEvent::TurnComplete { .. } => break,
            _ => {}
        }
    }
    println!();
    let _ = action_tx.send(Action::Quit).await;
    if let Some(e) = turn_error {
        anyhow::bail!("turn failed: {e}");
    }
    Ok(())
}

/// Name the action a headless run is denying, for the stderr note.
fn describe_approval(kind: &ApprovalKind) -> String {
    match kind {
        ApprovalKind::Command { cmd, .. } => format!("bash: {cmd}"),
        ApprovalKind::FileEdit(diff) => format!("edit: {}", diff.path.display()),
        ApprovalKind::OutsideProject { path, action } => {
            format!("{action}: {}", path.display())
        }
        ApprovalKind::Mcp { server, tool } => format!("mcp: {server}/{tool}"),
    }
}

/// The CLI-forced `PermissionMode`, or `None` when `setting.json` should decide
/// (in `build_orchestrator_with_fallback`). `--dangerously-skip-permissions` forces Bypass;
/// otherwise the `--mode` flag wins; a headless `-p` run without either falls
/// back to DontAsk (fail closed) unless blind approval was explicitly requested.
fn resolve_mode(global: &GlobalArgs) -> Option<PermissionMode> {
    if global.dangerously_skip_permissions {
        return Some(PermissionMode::Bypass);
    }
    if let Some(mode) = global.mode {
        return Some(mode_to_perm(Mode::from(mode)));
    }
    if global.print.is_some() && !global.dangerously_auto_approve {
        return Some(PermissionMode::DontAsk);
    }
    None
}

fn mode_to_perm(m: Mode) -> PermissionMode {
    match m {
        Mode::Auto => PermissionMode::Auto,
        Mode::Plan => PermissionMode::Plan,
        Mode::AcceptEdits => PermissionMode::AcceptEdits,
        Mode::Default => PermissionMode::Default,
        Mode::DontAsk => PermissionMode::DontAsk,
        Mode::Bypass => PermissionMode::Bypass,
    }
}

fn perm_to_mode(p: PermissionMode) -> Mode {
    match p {
        PermissionMode::Auto => Mode::Auto,
        PermissionMode::Plan => Mode::Plan,
        PermissionMode::AcceptEdits => Mode::AcceptEdits,
        PermissionMode::Default => Mode::Default,
        PermissionMode::DontAsk => Mode::DontAsk,
        PermissionMode::Bypass => Mode::Bypass,
    }
}

/// Load the session for `--resume <id>` or `--continue` (most recent by file
/// mtime), or start a fresh one. `--name` is stored on the record either way.
/// Seeding the prior conversation (real messages, or the digest for old-format
/// files) happens inside `spawn_core`, so headless and TUI behave alike.
fn resume_or_fresh(
    project_root: &std::path::Path,
    resume: Option<&str>,
    continue_latest: bool,
    name: Option<&str>,
) -> SessionRecord {
    let store = SessionStore::new(project_root);
    let mut session = match (resume, continue_latest) {
        (Some(id), _) => store.load(id).unwrap_or_else(|| {
            eprintln!("no session '{id}' found — starting fresh");
            SessionRecord::fresh()
        }),
        (None, true) => store.latest().unwrap_or_else(|| {
            eprintln!("no session to continue — starting fresh");
            SessionRecord::fresh()
        }),
        (None, false) => SessionRecord::fresh(),
    };
    if let Some(name) = name {
        session.name = Some(name.to_string());
    }
    session
}

/// The provider segment of a `provider/model-id` ref (or the whole string).
fn provider_of(model_ref: &str) -> &str {
    model_ref.split('/').next().unwrap_or(model_ref)
}

/// Whether the active model can't be built for lack of a key. A test-resolve that
/// fails with an auth error means the provider needs a key and none is reachable
/// (env / keyring / config); any other outcome (ok, or a non-auth error) does not
/// open the key overlay.
fn needs_api_key(resolver: &dyn stepper_core::ProviderResolver, model_ref: &str) -> bool {
    matches!(
        resolver.resolve(model_ref),
        Err(stepper_core::CoreError::Provider(
            stepper_providers::ProviderError::Auth(_)
        ))
    )
}

fn split_model(model: Option<&str>) -> (String, String) {
    match model.unwrap_or(DEFAULT_MODEL).split_once('/') {
        Some((p, m)) => (p.to_string(), m.to_string()),
        None => ("ollama-cloud".to_string(), "qwen3-coder".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeResolver {
        auth_err: bool,
    }
    impl stepper_core::ProviderResolver for FakeResolver {
        fn resolve(
            &self,
            _model_ref: &str,
        ) -> Result<Box<dyn stepper_providers::LlmProvider>, stepper_core::CoreError> {
            if self.auth_err {
                Err(stepper_core::CoreError::Provider(
                    stepper_providers::ProviderError::Auth("no key".into()),
                ))
            } else {
                Err(stepper_core::CoreError::NoModel("x".into()))
            }
        }
        fn model_info(&self, _model_ref: &str) -> stepper_core::ModelInfo {
            stepper_core::ModelInfo {
                context_window: 0,
                max_output_tokens: 0,
                input_per_mtok: 0.0,
                output_per_mtok: 0.0,
                cache_read_per_mtok: 0.0,
                cache_write_per_mtok: 0.0,
                estimated: true,
            }
        }
    }

    #[test]
    fn provider_of_takes_the_segment_before_the_slash() {
        assert_eq!(provider_of("anthropic/claude-opus-4-8"), "anthropic");
        assert_eq!(provider_of("noslash"), "noslash");
    }

    #[test]
    fn needs_api_key_only_on_an_auth_error() {
        assert!(needs_api_key(&FakeResolver { auth_err: true }, "anthropic/x"));
        assert!(
            !needs_api_key(&FakeResolver { auth_err: false }, "anthropic/x"),
            "a non-auth error must not pop the key overlay"
        );
    }

    #[test]
    fn scaffold_default_matches_plain_init_and_parses() {
        let s = scaffold_setting_json(None, "accept-edits");
        assert!(!s.contains("defaultModel"));
        let parsed: stepper_config::SettingsFile = serde_json::from_str(&s).unwrap();
        assert_eq!(parsed.mode.as_deref(), Some("accept-edits"));
        assert!(parsed.default_model.is_none());
        assert!(parsed.step.is_empty());
    }

    #[test]
    fn scaffold_embeds_chosen_model_and_mode() {
        let s = scaffold_setting_json(Some("anthropic/claude-opus-4-8"), "plan");
        let parsed: stepper_config::SettingsFile = serde_json::from_str(&s).unwrap();
        assert_eq!(parsed.default_model.as_deref(), Some("anthropic/claude-opus-4-8"));
        assert_eq!(parsed.mode.as_deref(), Some("plan"));
    }

    #[test]
    fn scaffold_escapes_model_so_json_stays_valid() {
        // Even a model string with a quote (rejected upstream, but the scaffold
        // must not be the thing that produces invalid JSON) round-trips safely.
        let s = scaffold_setting_json(Some("a\"b/c"), "accept-edits");
        let parsed: stepper_config::SettingsFile = serde_json::from_str(&s).unwrap();
        assert_eq!(parsed.default_model.as_deref(), Some("a\"b/c"));
    }
}
