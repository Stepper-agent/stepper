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
use stepper_tui::{run_tui, AgentInfo, CommandInfo, TuiInit};
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
        Some(Command::Import(args)) => import_cmd(args),
        Some(Command::Session(args)) => session_cmd(args, cli.global),
    }
}

/// `stepper import`: detect a Claude/Codex/Cursor/Gemini global config and
/// migrate the portable parts into `~/.stepper/`. Prints the plan, then applies
/// it — `--dry-run` stops after the preview, `--yes` skips the confirmation, and
/// otherwise a `y/N` prompt gates the write.
fn import_cmd(args: cli::ImportArgs) -> anyhow::Result<()> {
    let from = match args.resolved_source() {
        cli::ImportSourceArg::Claude => stepper_config::ImportFrom::Claude,
        cli::ImportSourceArg::Codex => stepper_config::ImportFrom::Codex,
        cli::ImportSourceArg::Cursor => stepper_config::ImportFrom::Cursor,
        cli::ImportSourceArg::Gemini => stepper_config::ImportFrom::Gemini,
        cli::ImportSourceArg::All => stepper_config::ImportFrom::All,
    };
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("HOME is not set — cannot locate ~/.stepper"))?;

    let plan = stepper_config::build_plan(&home, from)?;
    print!("{}", stepper_config::render_preview(&plan));

    if plan.is_empty() {
        return Ok(());
    }
    if args.dry_run {
        println!("\n(dry run — nothing was written)");
        return Ok(());
    }
    if !args.yes && !confirm("\nApply this migration? [y/N] ")? {
        println!("aborted — nothing was written");
        return Ok(());
    }

    let summary = stepper_config::apply_plan(&plan)?;
    println!(
        "imported: {} stepper.md section(s), {} setting.json, {} file(s) copied",
        summary.sections_appended,
        if summary.settings_written { "wrote" } else { "no change to" },
        summary.files_copied,
    );
    Ok(())
}

/// Prompt on stderr and read a `y/yes` answer from stdin. A non-interactive
/// stdin (EOF) reads as "no", so a piped run never silently writes.
fn confirm(prompt: &str) -> anyhow::Result<bool> {
    eprint!("{prompt}");
    std::io::stderr().flush().ok();
    let mut answer = String::new();
    if std::io::stdin().read_line(&mut answer)? == 0 {
        return Ok(false);
    }
    Ok(matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes"))
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

    if let Some(action) = args.action {
        return match action {
            cli::ConfigAction::Get { key } => {
                let cfg = stepper_config::Config::load(&cwd)
                    .map_err(|e| anyhow::anyhow!("invalid config: {e}"))?;
                match stepper_config::get_scalar(&cfg.settings, &key) {
                    Ok(Some(v)) => println!("{v}"),
                    Ok(None) => println!("(unset)"),
                    Err(e) => anyhow::bail!("{e}"),
                }
                Ok(())
            }
            cli::ConfigAction::Set { key, value } => {
                let disc = stepper_config::discover(&cwd);
                let dir = disc
                    .project_dir
                    .or(disc.user_dir)
                    .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".stepper")))
                    .ok_or_else(|| anyhow::anyhow!("no project .stepper and HOME is unset"))?;
                stepper_config::set_scalar(&dir, &key, &value).map_err(|e| anyhow::anyhow!("{e}"))?;
                println!("set {key} = {value} in {}", dir.join("setting.json").display());
                Ok(())
            }
        };
    }

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
        println!("usage: stepper config --schema | --validate | set <key> <value> | get <key>");
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
        std::fs::write(&setting, scaffold_setting_json(None, "accept-edits", None))?;
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
/// the starting permission mode; `limits` adds a `limits` block when any cap is
/// set. With `None`/`"accept-edits"`/`None` the output is the plain `stepper
/// init` template.
pub(crate) fn scaffold_setting_json(
    default_model: Option<&str>,
    mode: &str,
    limits: Option<&stepper_config::LimitsConfig>,
) -> String {
    let default_model_line = match default_model {
        // Serialize the value through serde_json so any model string is escaped
        // into a valid JSON string (the scaffold is otherwise hand-formatted).
        Some(m) => format!(
            "  \"defaultModel\": {},\n",
            serde_json::to_string(m).unwrap_or_else(|_| "\"\"".into())
        ),
        None => String::new(),
    };
    let limits_line = match limits {
        Some(l) if l.is_set() => format!(
            "  \"limits\": {},\n",
            serde_json::to_string(l).unwrap_or_else(|_| "{}".into())
        ),
        _ => String::new(),
    };
    format!(
        "{{\n  \"$schema\": \"stepper://setting.schema.json\",\n  \"step\": [],\n{default_model_line}  \"mode\": \"{mode}\",\n{limits_line}  \"providers\": {{}},\n  \"permissions\": {{\n    \"allow\": [\"Read(/**)\", \"Bash(cargo *)\"],\n    \"ask\": [\"Bash(git push:*)\"],\n    \"deny\": [\"Read(//etc/**)\", \"Bash(rm -rf *)\", \"Bash(rm -fr *)\", \"Bash(sudo *)\", \"Bash(git push --force *)\", \"Bash(git push -f *)\"]\n  }},\n  \"approvals\": [],\n  \"hooks\": {{}}\n}}\n"
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
    let limits = SessionLimits::new(
        global.max_turns,
        global.max_budget_usd,
        global.turn_timeout.map(std::time::Duration::from_secs),
    );

    if let Some(prompt) = global.print.clone() {
        return oneshot(&global, cli_mode, cwd, prompt, limits).await;
    }

    // First-run setup runs only on the interactive path (headless returned
    // above). When it writes a config its chosen model becomes this session's
    // model too, so the orchestrator and the footer agree.
    // `STEPPER_NO_INIT` opts out by presence (any value except an explicitly
    // falsey one); resolved here, not via clap's bool `env` which would abort the
    // whole command on a non-"true"/"false" value.
    let no_init = global.no_init
        || std::env::var_os("STEPPER_NO_INIT").is_some_and(|v| {
            !matches!(v.to_string_lossy().trim(), "" | "0" | "false" | "no" | "off")
        });
    let onboarding_model = onboarding::maybe_first_run(&cwd, no_init).await?;
    let effective_model = global.model.clone().or(onboarding_model);
    let (provider, model) = split_model(effective_model.as_deref());

    let (orchestrator, _mcp) = build_orchestrator_with_fallback(
        effective_model.as_deref(),
        global.fallback_model.as_deref(),
        cli_mode,
        global.effort.clone(),
        cwd.clone(),
        limits,
    )
    .await?;
    let session = resume_or_fresh(
        &orchestrator.project_root,
        global.resume.as_deref(),
        global.continue_session,
        global.name.as_deref(),
        global.fork,
    );
    // The orchestrator's mode is the resolved one (flag > setting.json > default).
    let resolved_mode = perm_to_mode(*orchestrator.mode.read().unwrap());
    // First-run / keyless start: if the active model's provider needs an API key
    // and none is resolvable (env / keyring / config), open the key overlay right
    // away by replaying a `/login <provider>` once the session is up.
    let model_ref = effective_model.clone().unwrap_or_else(|| DEFAULT_MODEL.to_string());
    let key_prompt = needs_api_key(&*orchestrator.resolver, &model_ref)
        .then(|| provider_of(&model_ref).to_string());
    let (action_tx, action_rx) = tokio::sync::mpsc::channel(64);
    let cancel = CancellationToken::new();
    // Snapshot the named sub-agents for the TUI's `#`-agent picker before
    // `spawn_core` consumes the orchestrator.
    let agents: Vec<AgentInfo> = orchestrator
        .agents
        .iter()
        .map(|a| AgentInfo { name: a.name.clone(), description: a.description.clone() })
        .collect();
    let event_rx = spawn_core(orchestrator, session, action_rx, cancel.clone());
    if let Some(provider) = key_prompt {
        let _ = action_tx
            .send(Action::SlashCommand { name: "login".into(), args: provider })
            .await;
    }

    let mut commands: Vec<CommandInfo> = stepper_core::builtin_command_descriptions()
        .into_iter()
        .map(|(name, description)| CommandInfo {
            name: name.to_string(),
            description: description.to_string(),
            argument_hint: None,
        })
        .collect();
    let mut theme_preset: Option<String> = None;
    let mut theme_colors: Vec<(String, String)> = Vec::new();
    let mut effort_setting: Option<String> = None;
    if let Ok(cfg) = stepper_config::Config::load(&cwd) {
        // `argument-hint` is keyed by command name; attach it to each user command.
        let hints: std::collections::BTreeMap<String, String> =
            cfg.command_hints().into_iter().collect();
        commands.extend(
            cfg.command_descriptions()
                .into_iter()
                .map(|(name, description)| CommandInfo {
                    argument_hint: hints.get(&name).cloned(),
                    name,
                    description,
                }),
        );
        // Load the persisted color theme (preset + per-role overrides).
        if let Some(theme) = &cfg.settings.theme {
            theme_preset = theme.preset.clone();
            theme_colors = theme.colors.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        }
        effort_setting = cfg.settings.reasoning_effort.clone();
    }
    // `--effort` wins over the setting; "off"/absent shows no footer indicator.
    let effort = global.effort.clone().or(effort_setting).filter(|e| e != "off");

    // `_mcp` keeps the MCP server connections open for the whole TUI session.
    let init = TuiInit {
        inline_height: 14,
        model: ModelView { provider, model },
        mode: resolved_mode,
        cwd,
        commands,
        agents,
        theme_preset,
        theme_colors,
        effort,
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
        global.effort.clone(),
        cwd,
        limits,
    )
    .await?;
    // --file: inline attachments as leading context, then --agent prefixes the
    // `#name` trigger (which must stay at the very start of the prompt).
    let prompt = attach_files(&global.file, &prompt, &orchestrator.project_root)?;
    // --agent: route this headless run to a named sub-agent via the `#agent`
    // trigger; an unknown name is an error here (the interactive `#name` falls
    // through to a normal turn, but an explicit flag should fail loudly).
    let prompt = match global.agent.as_deref() {
        Some(agent) => {
            let known: Vec<String> = orchestrator.agents.iter().map(|a| a.name.clone()).collect();
            agent_prompt(agent, &prompt, &known)?
        }
        None => prompt,
    };
    let session = resume_or_fresh(
        &orchestrator.project_root,
        global.resume.as_deref(),
        global.continue_session,
        global.name.as_deref(),
        global.fork,
    );
    let (action_tx, action_rx) = tokio::sync::mpsc::channel(64);
    let cancel = CancellationToken::new();
    let mut event_rx = spawn_core(orchestrator, session, action_rx, cancel);

    action_tx.send(Action::SubmitInput(prompt)).await?;
    let json = global.format == Some(cli::OutputFormat::Json);
    let mut stdout = std::io::stdout();
    let mut turn_error: Option<String> = None;
    // In JSON mode the assistant text is buffered and emitted as one `text` event
    // at the end (a stream of per-token JSON lines would be unusable).
    let mut assistant = String::new();

    while let Some(event) = event_rx.recv().await {
        // Tool/error events become one JSON line each (text/done are handled below).
        if json && let Some(line) = json_event(&event) {
            println!("{line}");
        }
        match event {
            AppEvent::AssistantTokenDelta(t) => {
                if json {
                    assistant.push_str(&t);
                } else {
                    print!("{t}");
                    stdout.flush().ok();
                }
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
            // JSON mode already emitted this above; otherwise note it on stderr.
            AppEvent::ToolCallStarted(view) if !json => {
                eprintln!("\n[tool] {}", view.summary);
            }
            AppEvent::Error(e) => {
                if !json {
                    eprintln!("\nerror: {e}");
                }
                turn_error = Some(e);
            }
            // A wall-clock timeout stops the turn as a silent `Cancelled`; surface
            // it as a failure so a headless/CI caller exits non-zero, like the
            // --max-turns / --max-budget-usd caps do.
            AppEvent::Notice { text, .. } if text.starts_with(stepper_core::TURN_TIMEOUT_NOTICE) => {
                if json {
                    println!("{}", serde_json::json!({ "type": "error", "message": text }));
                } else {
                    eprintln!("\n{text}");
                }
                turn_error = Some(text);
            }
            AppEvent::TurnComplete { .. } => break,
            _ => {}
        }
    }
    if json {
        if !assistant.is_empty() {
            println!("{}", serde_json::json!({ "type": "text", "text": assistant }));
        }
        println!("{}", serde_json::json!({ "type": "done" }));
    } else {
        println!();
    }
    let _ = action_tx.send(Action::Quit).await;
    if let Some(e) = turn_error {
        anyhow::bail!("turn failed: {e}");
    }
    Ok(())
}

/// Inline `--file` attachments into the prompt as tagged blocks. stepper runs
/// the agent in-process, so it folds the file contents directly into the message
/// (rather than passing a `file://` URL like opencode). Relative paths resolve
/// against `cwd`; a missing or non-UTF-8 file errors. The prompt follows the
/// files so they read as leading context.
fn attach_files(files: &[std::path::PathBuf], prompt: &str, cwd: &std::path::Path) -> anyhow::Result<String> {
    if files.is_empty() {
        return Ok(prompt.to_string());
    }
    let mut out = String::new();
    for file in files {
        let path = if file.is_absolute() { file.clone() } else { cwd.join(file) };
        let content = std::fs::read_to_string(&path)
            .map_err(|e| anyhow::anyhow!("--file {}: {e}", path.display()))?;
        out.push_str(&format!("<file path=\"{}\">\n{content}\n</file>\n\n", file.display()));
    }
    out.push_str(prompt);
    Ok(out)
}

/// Build the prompt for a headless `--agent <name>` run: validate the name
/// against the configured agents (an unknown one errors, listing the known
/// names) and prefix the prompt with the `#<name>` sub-agent trigger.
fn agent_prompt(agent: &str, prompt: &str, known: &[String]) -> anyhow::Result<String> {
    if !known.iter().any(|n| n == agent) {
        let list = if known.is_empty() { "none".to_string() } else { known.join(", ") };
        anyhow::bail!("unknown agent '{agent}' (configured: {list})");
    }
    Ok(format!("#{agent} {prompt}"))
}

/// One JSON line for a headless `--format json` event (stepper's own minimal
/// schema: `{type, ...}`), or `None` for events the headless stream doesn't
/// surface this way. Streaming text and the terminal `done` are emitted by
/// `oneshot` directly (they need the accumulated turn text / loop control).
fn json_event(event: &AppEvent) -> Option<String> {
    let value = match event {
        AppEvent::ToolCallStarted(v) => {
            serde_json::json!({ "type": "tool", "name": v.name, "summary": v.summary })
        }
        AppEvent::Error(e) => serde_json::json!({ "type": "error", "message": e }),
        _ => return None,
    };
    Some(value.to_string())
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
    fork: bool,
) -> SessionRecord {
    let store = SessionStore::new(project_root);
    let resumed = match (resume, continue_latest) {
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
    // `--fork`: branch the resumed session under a fresh id so the original is
    // left untouched. A no-op when there is nothing to fork (a fresh session).
    let mut session = if fork && !resumed.turns.is_empty() {
        resumed.forked()
    } else {
        resumed
    };
    if let Some(name) = name {
        session.name = Some(name.to_string());
    }
    session
}

/// `stepper session list|delete` — manage this project's saved sessions
/// (`.stepper/sessions/`), the non-interactive counterpart of the `/resume`
/// picker.
fn session_cmd(args: cli::SessionArgs, global: GlobalArgs) -> anyhow::Result<()> {
    let cwd = global.cwd.clone().map(Ok).unwrap_or_else(std::env::current_dir)?;
    let store = SessionStore::new(&cwd);
    match args.cmd {
        cli::SessionCmd::List { limit, json } => {
            let recent = store.list_recent(limit.unwrap_or(usize::MAX));
            let now = std::time::SystemTime::now();
            if json {
                let items: Vec<_> = recent
                    .iter()
                    .map(|(r, modified)| {
                        serde_json::json!({
                            "id": r.id,
                            "name": r.name,
                            "turns": r.turns.len(),
                            "ageSecs": now.duration_since(*modified).map(|d| d.as_secs()).unwrap_or(0),
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&items)?);
            } else if recent.is_empty() {
                println!("(no saved sessions)");
            } else {
                for (record, modified) in &recent {
                    let label = record.name.clone().unwrap_or_else(|| {
                        record
                            .turns
                            .first()
                            .and_then(|t| t.user.lines().next())
                            .unwrap_or("(empty)")
                            .to_string()
                    });
                    println!(
                        "{}  {} turn(s)  {}  {}",
                        record.id,
                        record.turns.len(),
                        stepper_core::age_label(now, *modified),
                        label
                    );
                }
            }
        }
        cli::SessionCmd::Delete { id } => {
            if store.delete(&id)? {
                println!("deleted session {id}");
            } else {
                anyhow::bail!("no session '{id}' found");
            }
        }
    }
    Ok(())
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

    #[test]
    fn attach_files_inlines_contents_then_prompt() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello from a").unwrap();
        let out = attach_files(&[std::path::PathBuf::from("a.txt")], "do it", dir.path()).unwrap();
        assert!(out.contains("<file path=\"a.txt\">"), "tagged block: {out}");
        assert!(out.contains("hello from a"));
        assert!(out.trim_end().ends_with("do it"), "the prompt follows the files: {out}");
        // No files → the prompt is unchanged.
        assert_eq!(attach_files(&[], "just this", dir.path()).unwrap(), "just this");
        // A missing file errors.
        assert!(attach_files(&[std::path::PathBuf::from("nope.txt")], "x", dir.path()).is_err());
    }

    #[test]
    fn json_event_maps_tool_and_error_only() {
        let tool = json_event(&AppEvent::ToolCallStarted(stepper_protocol::ToolCallView {
            id: "1".into(),
            name: "bash".into(),
            summary: "ls -la".into(),
        }))
        .unwrap();
        assert!(tool.contains("\"type\":\"tool\""), "{tool}");
        assert!(tool.contains("bash") && tool.contains("ls -la"));
        let err = json_event(&AppEvent::Error("boom".into())).unwrap();
        assert!(err.contains("\"type\":\"error\"") && err.contains("boom"));
        // Other events aren't surfaced as JSON lines (text/done handled by oneshot).
        assert!(json_event(&AppEvent::TurnComplete { turn_id: 1 }).is_none());
        assert!(json_event(&AppEvent::AssistantTokenDelta("hi".into())).is_none());
    }

    #[test]
    fn agent_prompt_validates_and_prefixes_the_trigger() {
        let known = vec!["reviewer".to_string(), "researcher".to_string()];
        assert_eq!(agent_prompt("reviewer", "do it", &known).unwrap(), "#reviewer do it");
        let err = agent_prompt("nope", "do it", &known).unwrap_err().to_string();
        assert!(err.contains("unknown agent 'nope'"), "{err}");
        assert!(err.contains("reviewer"), "lists configured agents: {err}");
        assert!(agent_prompt("x", "p", &[]).unwrap_err().to_string().contains("none"));
    }

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
        let s = scaffold_setting_json(None, "accept-edits", None);
        assert!(!s.contains("defaultModel"));
        assert!(!s.contains("limits"));
        let parsed: stepper_config::SettingsFile = serde_json::from_str(&s).unwrap();
        assert_eq!(parsed.mode.as_deref(), Some("accept-edits"));
        assert!(parsed.default_model.is_none());
        assert!(parsed.step.is_empty());
        assert!(parsed.limits.is_none());
    }

    #[test]
    fn scaffold_embeds_chosen_model_and_mode() {
        let s = scaffold_setting_json(Some("anthropic/claude-opus-4-8"), "plan", None);
        let parsed: stepper_config::SettingsFile = serde_json::from_str(&s).unwrap();
        assert_eq!(parsed.default_model.as_deref(), Some("anthropic/claude-opus-4-8"));
        assert_eq!(parsed.mode.as_deref(), Some("plan"));
    }

    #[test]
    fn scaffold_embeds_chosen_limits() {
        let limits = stepper_config::LimitsConfig {
            turn_timeout_secs: Some(600),
            max_budget_usd: Some(5.0),
            max_turns: None,
        };
        let s = scaffold_setting_json(Some("anthropic/claude-opus-4-8"), "auto", Some(&limits));
        let parsed: stepper_config::SettingsFile = serde_json::from_str(&s).unwrap();
        let got = parsed.limits.unwrap();
        assert_eq!(got.turn_timeout_secs, Some(600));
        assert_eq!(got.max_budget_usd, Some(5.0));
        assert_eq!(got.max_turns, None);
    }

    #[test]
    fn scaffold_escapes_model_so_json_stays_valid() {
        // Even a model string with a quote (rejected upstream, but the scaffold
        // must not be the thing that produces invalid JSON) round-trips safely.
        let s = scaffold_setting_json(Some("a\"b/c"), "accept-edits", None);
        let parsed: stepper_config::SettingsFile = serde_json::from_str(&s).unwrap();
        assert_eq!(parsed.default_model.as_deref(), Some("a\"b/c"));
    }
}
