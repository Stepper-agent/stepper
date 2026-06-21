mod cli;
mod core_setup;
mod logging;
mod onboarding;

use clap::Parser;
use cli::{AuthCmd, Cli, Command, GlobalArgs};
use core_setup::{build_orchestrator_with_fallback, DEFAULT_MODEL};
use std::io::Write;
use stepper_core::{
    spawn_core, ConfigProviderResolver, ModelRegistry, Orchestrator, ProviderResolver, SessionLimits, SessionRecord,
    SessionStore,
};
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
    // Keep the appender guard alive for the whole process so logs flush on exit.
    // `None` (no `--log-level`) installs nothing — the tracing macros stay no-ops.
    let _log_guard = logging::init_logging(&cli.global);
    match cli.command {
        None | Some(Command::Run) => launch(cli.global).await,
        Some(Command::Auth(args)) => match args.cmd {
            AuthCmd::Login(login) => auth_login(login.codex).await,
            AuthCmd::SetKey { provider } => set_key(&provider),
            AuthCmd::DeleteKey { provider } => delete_key(&provider),
        },
        Some(Command::Config(args)) => config_cmd(args, cli.global),
        Some(Command::Doctor) => doctor_cmd(cli.global).await,
        Some(Command::Init) => init_cmd(cli.global),
        Some(Command::Layer { name }) => scaffold_layer_cmd(&name, cli.global),
        Some(Command::Cmd { name }) => scaffold_command_cmd(&name, cli.global),
        Some(Command::ScaffoldLayer) => scaffold_pipeline_cmd(cli.global),
        Some(Command::Import(args)) => import_cmd(args),
        Some(Command::Session(args)) => session_cmd(args, cli.global),
        Some(Command::Mcp(args)) => mcp_cmd(args, cli.global).await,
        Some(Command::Stats(args)) => stats_cmd(args, cli.global),
        Some(Command::Models(args)) => models_cmd(args, cli.global).await,
    }
}

/// `stepper mcp auth|logout|status`: manage OAuth for remote (http) MCP servers.
/// Tokens live in `~/.stepper/mcp-auth.json` (0600), never in `setting.json`.
/// Build a `mcpServers` JSON entry for `stepper mcp add`: an http server when a
/// `--url` is given, otherwise a stdio server from `--command` + `--arg`s.
fn mcp_server_entry(command: Option<String>, args: Vec<String>, url: Option<String>) -> serde_json::Value {
    match url {
        Some(url) => serde_json::json!({ "type": "http", "url": url }),
        None => serde_json::json!({ "command": command, "args": args }),
    }
}

/// The `.stepper` directory to write MCP config into: the project's if a project
/// config exists, else the user's `~/.stepper`, else `<cwd>/.stepper` (created on
/// first write by `update_settings`).
fn mcp_write_dir(cfg: &stepper_config::Config, cwd: &std::path::Path) -> std::path::PathBuf {
    cfg.project_dir
        .clone()
        .or_else(|| cfg.user_dir.clone())
        .unwrap_or_else(|| cwd.join(".stepper"))
}

async fn mcp_cmd(args: cli::McpArgs, global: GlobalArgs) -> anyhow::Result<()> {
    let cwd = global_cwd(&global)?;
    let cfg = stepper_config::Config::load(&cwd).map_err(|e| anyhow::anyhow!("load config: {e}"))?;
    match args.cmd {
        cli::McpCmd::List => {
            if cfg.settings.mcp_servers.is_empty() {
                println!("No MCP servers configured.");
                return Ok(());
            }
            for (name, s) in &cfg.settings.mcp_servers {
                let transport = s.transport.as_deref().unwrap_or("stdio");
                let endpoint = s.url.clone().or_else(|| s.command.clone()).unwrap_or_default();
                let disabled = if s.enabled == Some(false) { "  (disabled)" } else { "" };
                println!("{name}  [{transport}]  {endpoint}{disabled}");
            }
            Ok(())
        }
        cli::McpCmd::Get { name } => {
            let server = cfg
                .settings
                .mcp_servers
                .get(&name)
                .ok_or_else(|| anyhow::anyhow!("no MCP server '{name}' in setting.json"))?
                .clone();
            println!("{name}  [{}]", server.transport.as_deref().unwrap_or("stdio"));
            if let Some(cmd) = &server.command {
                println!("  command: {cmd} {}", server.args.join(" "));
            }
            if let Some(url) = &server.url {
                println!("  url: {url}");
            }
            // Connect just this server and list everything it advertises.
            let base_dir = cfg.project_root.clone().unwrap_or_else(|| cwd.clone());
            let one = std::collections::BTreeMap::from([(name.clone(), server)]);
            let mgr = stepper_mcp::McpManager::connect(&one, &base_dir, cfg.settings.proxy.as_ref()).await;
            let tools = mgr.tool_names();
            println!("  tools ({}):", tools.len());
            for t in &tools {
                println!("    {t}");
            }
            let resources = mgr.list_all_resources().await;
            println!("  resources ({}):", resources.len());
            for r in &resources {
                println!("    {r}");
            }
            let prompts = mgr.list_all_prompts().await;
            println!("  prompts ({}):", prompts.len());
            for p in &prompts {
                println!("    {p}");
            }
            mgr.shutdown().await;
            Ok(())
        }
        cli::McpCmd::Add { name, command, args: cmd_args, url } => {
            if command.is_none() && url.is_none() {
                anyhow::bail!("provide --command <cmd> (stdio) or --url <url> (http)");
            }
            let dir = mcp_write_dir(&cfg, &cwd);
            let entry = mcp_server_entry(command, cmd_args, url);
            stepper_config::scaffold::update_settings(&dir, |obj| {
                let servers = obj
                    .entry("mcpServers")
                    .or_insert_with(|| serde_json::json!({}));
                if let Some(map) = servers.as_object_mut() {
                    map.insert(name.clone(), entry);
                }
            })
            .map_err(|e| anyhow::anyhow!("write {}: {e}", dir.join("setting.json").display()))?;
            println!("Added MCP server '{name}' to {}.", dir.join("setting.json").display());
            Ok(())
        }
        cli::McpCmd::Remove { name } => {
            // `list`/`get` read the merged (user+project) config, so `remove` must
            // reach BOTH scopes — a server defined only in `~/.stepper` is otherwise
            // unremovable from inside a project. Only existing setting.json files are
            // touched (no empty file is created in a scope that lacks the server).
            let dirs: Vec<std::path::PathBuf> =
                [cfg.project_dir.clone(), cfg.user_dir.clone()].into_iter().flatten().collect();
            let mut removed = false;
            for dir in &dirs {
                if !dir.join("setting.json").is_file() {
                    continue;
                }
                stepper_config::scaffold::update_settings(dir, |obj| {
                    if let Some(map) = obj.get_mut("mcpServers").and_then(|v| v.as_object_mut())
                        && map.remove(&name).is_some()
                    {
                        removed = true;
                    }
                })
                .map_err(|e| anyhow::anyhow!("write {}: {e}", dir.join("setting.json").display()))?;
            }
            if removed {
                println!("Removed MCP server '{name}'.");
            } else {
                println!("No MCP server '{name}' in the project or user config.");
            }
            Ok(())
        }
        cli::McpCmd::Auth { name } => {
            let server = cfg
                .settings
                .mcp_servers
                .get(&name)
                .ok_or_else(|| anyhow::anyhow!("no MCP server '{name}' in setting.json"))?;
            // Match `is_oauth_enabled` (the gate connect/status use) exactly, so a
            // server you can auth is one whose tokens actually get used: only an
            // http/streamable-http server with a url + non-disabled oauth qualifies.
            if !matches!(server.transport.as_deref(), Some("http") | Some("streamable-http")) {
                anyhow::bail!("server '{name}' is not http — set \"type\": \"http\" (OAuth is http-only)");
            }
            let url = server
                .url
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("server '{name}' has no `url`"))?;
            let oauth_cfg = server.oauth.clone().ok_or_else(|| {
                anyhow::anyhow!("server '{name}' has no `oauth` config — add \"oauth\": {{}} to enable it")
            })?;
            if oauth_cfg.disabled {
                anyhow::bail!("server '{name}' has `oauth.disabled = true`");
            }
            // Follows redirects and carries any extra CA + the configured proxy
            // (so OAuth works in a corp-proxy / non-env-proxy environment too).
            let client = ProviderFactory::with_proxy(cfg.settings.proxy.as_ref())?.http_client();
            stepper_mcp::authenticate(&name, &oauth_cfg, url, client)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            println!("Authorized MCP server '{name}'.");
            Ok(())
        }
        cli::McpCmd::Logout { name } => {
            let removed = stepper_mcp::logout(&name).await.map_err(|e| anyhow::anyhow!("{e}"))?;
            println!(
                "{}",
                if removed {
                    format!("Removed stored OAuth tokens for '{name}'.")
                } else {
                    format!("No stored OAuth tokens for '{name}'.")
                }
            );
            Ok(())
        }
        cli::McpCmd::Status => {
            let entries = stepper_mcp::status(&cfg.settings.mcp_servers);
            if entries.is_empty() {
                println!("No OAuth-capable MCP servers configured.");
                return Ok(());
            }
            for entry in entries {
                let mark = if entry.authenticated { "authed " } else { "no token" };
                println!("[{mark}] {}", entry.server);
            }
            Ok(())
        }
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

/// `stepper doctor`: a full health check. Validates the config, checks each
/// provider's API key, resolves the default model + fallback chain, attempts to
/// connect every MCP server, fetches the models.dev catalog, and compares the
/// running version to the latest published release. Prints a per-check report and
/// exits non-zero only on a hard problem (invalid config, an unresolvable default
/// model) — missing keys / unreachable servers are warnings, not failures.
async fn doctor_cmd(global: GlobalArgs) -> anyhow::Result<()> {
    let cwd = global_cwd(&global)?;
    println!("stepper {} — doctor", env!("CARGO_PKG_VERSION"));
    println!("  cwd: {}", cwd.display());
    let mut errors = 0usize;

    // 1. Config: structural + value validation, plus the permission ruleset.
    let mut config = match stepper_config::Config::load(&cwd) {
        Ok(cfg) => {
            let mut problems = cfg.validate_values();
            let perms = &cfg.settings.permissions;
            match RuleSet::from_lists_checked(&perms.allow, &perms.ask, &perms.deny) {
                Ok((_, dropped)) => problems.extend(
                    dropped.iter().map(|s| format!("malformed allow/ask rule (ignored): {s}")),
                ),
                Err(e) => problems.push(format!("permissions: {e}")),
            }
            if problems.is_empty() {
                println!(
                    "✓ config: valid ({} step(s), {} provider(s))",
                    cfg.settings.step.len(),
                    cfg.settings.providers.len()
                );
            } else {
                errors += 1;
                println!("✗ config: {} problem(s)", problems.len());
                for p in &problems {
                    println!("    - {p}");
                }
            }
            cfg
        }
        // Without a loadable config there's nothing further to check.
        Err(e) => anyhow::bail!("✗ config: failed to load: {e}"),
    };

    // 2. Provider API keys (env / keyring / explicit cfg). A missing key is only a
    // warning — localhost and OAuth providers don't need one. The check mirrors the
    // factory exactly: `resolve_provider` first expands the config key (`{env:VAR}`
    // / `{file:}` / the `null` sentinel → None), then `resolve_key` layers env +
    // keyring on top — so an unset `{env:VAR}` is NOT falsely reported as resolved.
    if config.settings.providers.is_empty() {
        println!("· providers: none configured (convention defaults + env keys apply)");
    } else {
        let names: Vec<String> = config.settings.providers.keys().cloned().collect();
        for name in &names {
            let explicit = config.resolve_provider(&format!("{name}/_")).ok().and_then(|p| p.api_key);
            if stepper_providers::resolve_key(name, explicit.as_deref()).is_some() {
                println!("✓ provider {name}: key resolved");
            } else {
                println!("· provider {name}: no key (env/keyring/config) — ok for local/oauth");
            }
        }
    }

    // Read everything config-derived BEFORE the resolver consumes the config.
    let proxy = config.settings.proxy.clone();
    let mcp_servers = config.settings.mcp_servers.clone();
    let base_dir = config.project_root.clone().unwrap_or_else(|| cwd.clone());
    let default_model = config
        .settings
        .default_model
        .clone()
        .unwrap_or_else(|| DEFAULT_MODEL.to_string());
    let fallback_models = core_setup::resolve_fallback_models(&global.fallback_model, &config);
    // Mirror `launch`: synthesize the convention provider for the default model +
    // each fallback so doctor checks what an actual run would resolve (without
    // this, the convention default like `ollama-cloud/...` reports a false error).
    core_setup::ensure_provider(&mut config, &default_model);
    for fb in &fallback_models {
        core_setup::ensure_provider(&mut config, fb);
    }

    let factory = ProviderFactory::with_proxy(proxy.as_ref())?;
    // `http_client()` is a cheap clone, so the catalog + release fetches keep a
    // client of their own while `factory` moves into the resolver below.
    let client = factory.http_client();

    // 6. models.dev catalog (network) — reused by the resolver for overlays.
    let catalog = match stepper_providers::models::fetch_catalog(&client).await {
        Ok(c) => {
            println!("✓ models.dev catalog: {} provider(s)", c.provider_seeds().len());
            Some(c)
        }
        Err(e) => {
            println!("! models.dev catalog: fetch failed ({e})");
            None
        }
    };
    let resolver =
        ConfigProviderResolver::new(config, factory, ModelRegistry::builtin(), None, catalog);

    // 3. Default model + fallback chain resolve cleanly (key + provider kind). A
    // missing key is a warning (consistent with step 2 and the fresh-checkout
    // convention default that exists precisely for a keyless start); only a
    // genuinely unresolvable default (unknown provider kind, malformed ref) fails.
    match resolver.resolve(&default_model) {
        Ok(_) => println!("✓ default model {default_model}: resolves"),
        Err(stepper_core::CoreError::Provider(stepper_providers::ProviderError::Auth(_))) => {
            println!("! default model {default_model}: no API key reachable (set a key to use it)");
        }
        Err(e) => {
            errors += 1;
            println!("✗ default model {default_model}: {e}");
        }
    }
    for fb in &fallback_models {
        match resolver.resolve(fb) {
            Ok(_) => println!("✓ fallback model {fb}: resolves"),
            Err(e) => println!("! fallback model {fb}: {e}"),
        }
    }

    // 4. MCP servers — a real connection attempt per server (network/process).
    if mcp_servers.is_empty() {
        println!("· mcp: no servers configured");
    } else {
        for (name, server) in &mcp_servers {
            if server.enabled == Some(false) {
                println!("· mcp {name}: disabled");
                continue;
            }
            let one = std::collections::BTreeMap::from([(name.clone(), server.clone())]);
            let mgr = stepper_mcp::McpManager::connect(&one, &base_dir, proxy.as_ref()).await;
            let count = mgr.tool_names().len();
            if count == 0 {
                println!("! mcp {name}: connected with no tools, or failed to connect");
            } else {
                println!("✓ mcp {name}: {count} tool(s)");
            }
            mgr.shutdown().await;
        }
    }

    // 7. Latest release (network) — informational only.
    match stepper_providers::fetch_latest_release_tag(&client, "Stepper-agent/stepper").await {
        Ok(tag) => {
            let current = format!("v{}", env!("CARGO_PKG_VERSION"));
            if tag == current {
                println!("✓ version: {current} (latest)");
            } else {
                println!("! version: running {current}, latest is {tag}");
            }
        }
        Err(e) => println!("· version: update check skipped ({e})"),
    }

    println!();
    if errors == 0 {
        println!("doctor: no problems found");
        Ok(())
    } else {
        anyhow::bail!("doctor: {errors} problem(s) found");
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

    let (mut orchestrator, _mcp) = build_orchestrator_with_fallback(
        effective_model.as_deref(),
        &global.fallback_model,
        cli_mode,
        global.effort.clone(),
        cwd.clone(),
        limits,
    )
    .await?;
    apply_system_prompt_overrides(&mut orchestrator, &global)?;
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
    // Per-project prompt-history file, resolved before the orchestrator moves.
    let history_path = history_file_path(&orchestrator.project_root);
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
    // (on_complete, on_approval, on_error) terminal-bell triggers; silent default.
    let mut notify = (false, false, false);
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
        if let Some(notification) = &cfg.settings.notification {
            notify = notification.resolve();
        }
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
        notify_on_complete: notify.0,
        notify_on_approval: notify.1,
        notify_on_error: notify.2,
        history_path,
    };
    run_tui(event_rx, action_tx, init, cancel).await
}

/// The per-project prompt-history file: `~/.stepper/history/<slug>-<hash>.json`.
/// The slug (project dir name) keeps it human-recognizable; the stable hash of
/// the full path disambiguates same-named projects. `None` without `$HOME` (then
/// history stays in memory only).
fn history_file_path(project_root: &std::path::Path) -> Option<std::path::PathBuf> {
    use std::hash::{Hash, Hasher};
    let home = std::env::var_os("HOME")?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    project_root.hash(&mut hasher);
    let hash = hasher.finish();
    let slug: String = project_root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project")
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect();
    Some(
        std::path::PathBuf::from(home)
            .join(".stepper")
            .join("history")
            .join(format!("{slug}-{hash:016x}.json")),
    )
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
    let (mut orchestrator, _mcp) = build_orchestrator_with_fallback(
        global.model.as_deref(),
        &global.fallback_model,
        cli_mode,
        global.effort.clone(),
        cwd,
        limits,
    )
    .await?;
    apply_system_prompt_overrides(&mut orchestrator, global)?;
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
    // `--output-schema`: compile the JSON Schema (inline string or a file path)
    // once; the run then requires the reply to validate against it, re-prompting
    // on a mismatch. None = no structured-output enforcement.
    let validator = match global.output_schema.as_deref() {
        Some(spec) => Some(compile_output_schema(spec)?),
        None => None,
    };

    let (action_tx, action_rx) = tokio::sync::mpsc::channel(64);
    let cancel = CancellationToken::new();
    let mut event_rx = spawn_core(orchestrator, session, action_rx, cancel);

    let json = global.format == Some(cli::OutputFormat::Json);
    // In schema mode the reply is buffered and validated, then the final (valid)
    // JSON is printed once — never streamed live, and `--format` json events are
    // suppressed (the JSON document itself is the output).
    let stream_live = !json && validator.is_none();
    let emit_json = json && validator.is_none();

    let mut input = Action::SubmitInput(prompt);
    let mut assistant = String::new();
    let mut turn_error: Option<String> = None;
    // attempt 0 = the original prompt; up to `output_schema_retries` corrections.
    let max_attempts = if validator.is_some() { global.output_schema_retries + 1 } else { 1 };
    for _attempt in 0..max_attempts {
        let (reply, err) = drive_one_turn(
            &mut event_rx,
            &action_tx,
            std::mem::replace(&mut input, Action::Redraw),
            stream_live,
            emit_json,
            global.dangerously_auto_approve,
        )
        .await;
        assistant = reply;
        turn_error = err;
        // A hard turn error (cap/timeout/provider) is terminal — don't retry.
        if turn_error.is_some() {
            break;
        }
        let Some(validator) = validator.as_ref() else {
            break; // no schema → one turn only
        };
        match validate_output(validator, &assistant) {
            Ok(()) => break,
            Err(reason) => {
                // Re-prompt with the validation error so the model can correct.
                input = Action::SubmitInput(format!(
                    "Your previous reply did not satisfy the required JSON schema: {reason}\n\
                     Respond with ONLY a JSON value that conforms to the schema — no prose, no code fences.",
                ));
                turn_error = Some(format!("output did not match the schema: {reason}"));
            }
        }
    }

    if emit_json {
        if !assistant.is_empty() {
            println!("{}", serde_json::json!({ "type": "text", "text": assistant }));
        }
        println!("{}", serde_json::json!({ "type": "done" }));
    } else if validator.is_some() {
        // Print the final structured result (the valid JSON, or the last attempt).
        println!("{}", assistant.trim());
    } else {
        println!();
    }
    let _ = action_tx.send(Action::Quit).await;
    if let Some(e) = turn_error {
        anyhow::bail!("turn failed: {e}");
    }
    Ok(())
}

/// Drive one headless turn: submit `input`, then collect events until the turn
/// completes, returning the accumulated assistant text and any terminal error.
/// `stream_live` prints text deltas to stdout as they arrive; `emit_json` writes
/// one JSON event per tool/error line; `auto_approve` blindly allows approvals.
async fn drive_one_turn(
    event_rx: &mut stepper_protocol::EventRx,
    action_tx: &stepper_protocol::ActionTx,
    input: Action,
    stream_live: bool,
    emit_json: bool,
    auto_approve: bool,
) -> (String, Option<String>) {
    let _ = action_tx.send(input).await;
    let mut stdout = std::io::stdout();
    let mut assistant = String::new();
    let mut turn_error = None;
    while let Some(event) = event_rx.recv().await {
        if emit_json && let Some(line) = json_event(&event) {
            println!("{line}");
        }
        match event {
            AppEvent::AssistantTokenDelta(t) => {
                assistant.push_str(&t);
                if stream_live {
                    print!("{t}");
                    stdout.flush().ok();
                }
            }
            AppEvent::ApprovalRequested(req) => {
                if auto_approve {
                    let _ = req.reply.send(ApprovalDecision::AllowOnce);
                } else {
                    eprintln!(
                        "\n[denied] {} (headless denies approval prompts; pass --dangerously-auto-approve to allow)",
                        describe_approval(&req.kind)
                    );
                    let _ = req.reply.send(ApprovalDecision::Deny);
                }
            }
            AppEvent::ToolCallStarted(view) if !emit_json => {
                eprintln!("\n[tool] {}", view.summary);
            }
            AppEvent::Error(e) => {
                if !emit_json {
                    eprintln!("\nerror: {e}");
                }
                turn_error = Some(e);
            }
            // A wall-clock timeout stops the turn as a silent `Cancelled`; surface
            // it as a failure so a headless/CI caller exits non-zero, like the
            // --max-turns / --max-budget-usd caps do.
            AppEvent::Notice { text, .. } if text.starts_with(stepper_core::TURN_TIMEOUT_NOTICE) => {
                if emit_json {
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
    (assistant, turn_error)
}

/// Compile a `--output-schema` spec (an inline JSON Schema string, or a path to a
/// `.json` schema file) into a reusable validator.
fn compile_output_schema(spec: &str) -> anyhow::Result<jsonschema::Validator> {
    let text = if std::path::Path::new(spec).is_file() {
        std::fs::read_to_string(spec).map_err(|e| anyhow::anyhow!("--output-schema {spec}: {e}"))?
    } else {
        spec.to_string()
    };
    let schema: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| anyhow::anyhow!("--output-schema is not valid JSON: {e}"))?;
    jsonschema::validator_for(&schema).map_err(|e| anyhow::anyhow!("invalid JSON Schema: {e}"))
}

/// Validate a reply against the output schema. The reply must be a single JSON
/// value (a leading/trailing ```` ```json ```` fence is tolerated) that conforms.
fn validate_output(validator: &jsonschema::Validator, reply: &str) -> Result<(), String> {
    let trimmed = strip_code_fence(reply.trim());
    let value: serde_json::Value =
        serde_json::from_str(trimmed).map_err(|e| format!("reply is not valid JSON ({e})"))?;
    match validator.validate(&value) {
        Ok(()) => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

/// Strip a single Markdown code fence (```` ``` ```` or ```` ```json ````) around
/// `s`, so a fenced JSON reply still validates. Returns `s` unchanged otherwise.
fn strip_code_fence(s: &str) -> &str {
    let Some(rest) = s.strip_prefix("```") else {
        return s;
    };
    // Drop the optional language tag on the opening fence's line.
    let rest = rest.split_once('\n').map(|(_, body)| body).unwrap_or(rest);
    rest.trim_end().strip_suffix("```").unwrap_or(rest).trim()
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

/// Resolve a `--system-prompt[-file]` / `--append-system-prompt[-file]` pair to
/// the text to use: inline text wins, else the file is read (clap already rejects
/// passing both). `None` when neither is set.
fn read_prompt_arg(text: Option<&str>, file: Option<&std::path::Path>) -> anyhow::Result<Option<String>> {
    if let Some(t) = text {
        return Ok(Some(t.to_string()));
    }
    if let Some(p) = file {
        let body = std::fs::read_to_string(p).map_err(|e| anyhow::anyhow!("--system-prompt-file {}: {e}", p.display()))?;
        return Ok(Some(body));
    }
    Ok(None)
}

/// Append extra instructions after a layer's role prompt (blank role → just the
/// extra). The append lands at the very end of the composed system message.
fn appended_role(role: &str, extra: &str) -> String {
    let role = role.trim_end();
    if role.is_empty() {
        extra.to_string()
    } else {
        format!("{role}\n\n{extra}")
    }
}

/// Apply `--system-prompt` (replace the project base context) and
/// `--append-system-prompt` (append to every layer's role) to an already-built
/// orchestrator. The base-context replacement also reaches dispatched sub-agents
/// (they clone `base_context`); the append affects the main layers' roles only.
fn apply_system_prompt_overrides(orchestrator: &mut Orchestrator, global: &GlobalArgs) -> anyhow::Result<()> {
    if let Some(base) = read_prompt_arg(global.system_prompt.as_deref(), global.system_prompt_file.as_deref())? {
        orchestrator.base_context = base;
    }
    if let Some(extra) = read_prompt_arg(global.append_system_prompt.as_deref(), global.append_system_prompt_file.as_deref())? {
        let extra = extra.trim();
        if !extra.is_empty() {
            for step in &mut orchestrator.steps {
                step.system_prompt = appended_role(&step.system_prompt, extra);
            }
        }
    }
    Ok(())
}

/// Build the prompt for a headless `--agent <name>` run: validate the name
/// against the configured agents (an unknown one errors, listing the known
/// names) and prefix the prompt with the `#<name>` sub-agent trigger.
fn agent_prompt(agent: &str, prompt: &str, known: &[String]) -> anyhow::Result<String> {
    // The `#name` route splits on the first whitespace, so a space-containing
    // name could never round-trip — fail loudly rather than silently drop it.
    if agent.contains(char::is_whitespace) {
        anyhow::bail!("agent name '{agent}' must not contain whitespace");
    }
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
    // Resolve the project root (as resume does) so `session` works from any
    // subdirectory, not only where `.stepper/` lives.
    let root = stepper_config::discovery::discover(&cwd).project_root.unwrap_or(cwd);
    let store = SessionStore::new(&root);
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
        cli::SessionCmd::Rename { id, name } => {
            let mut record = store
                .load(&id)
                .ok_or_else(|| anyhow::anyhow!("no session '{id}' found"))?;
            record.name = Some(name.clone());
            store.save(&record).map_err(|e| anyhow::anyhow!("{e}"))?;
            println!("renamed session {id} to '{name}'");
        }
    }
    Ok(())
}

/// `stepper stats`: aggregate token/cost usage across every saved session.
fn stats_cmd(args: cli::StatsArgs, global: GlobalArgs) -> anyhow::Result<()> {
    let cwd = global.cwd.clone().map(Ok).unwrap_or_else(std::env::current_dir)?;
    // Resolve the project root (as session/resume do) so it works from any subdir.
    let root = stepper_config::discovery::discover(&cwd).project_root.unwrap_or(cwd);
    let store = SessionStore::new(&root);
    let records: Vec<_> = store.list_recent(usize::MAX).into_iter().map(|(r, _)| r).collect();
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let stats = stepper_core::aggregate_stats(&records, args.days, now_secs);

    if let Some(path) = args.export {
        let is_csv = path.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("csv"));
        let body = if is_csv { stats.to_csv() } else { serde_json::to_string_pretty(&stats)? };
        std::fs::write(&path, body)?;
        println!("wrote stats to {}", path.display());
    } else if args.json {
        println!("{}", serde_json::to_string_pretty(&stats)?);
    } else {
        print!("{}", stats.render_text(args.models, args.tools));
    }
    Ok(())
}

/// `stepper models [provider]`: list the models reachable across the configured
/// providers (each provider's live list merged with the models.dev catalog),
/// sorted by `provider/model-id`, one per line. `--verbose` appends context /
/// output / pricing; `--json` emits the raw entries.
async fn models_cmd(args: cli::ModelsArgs, global: GlobalArgs) -> anyhow::Result<()> {
    let cwd = global_cwd(&global)?;
    let config = stepper_config::Config::load(&cwd).map_err(|e| anyhow::anyhow!("load config: {e}"))?;
    let factory = ProviderFactory::with_proxy(config.settings.proxy.as_ref())?;
    let catalog = stepper_providers::models::fetch_catalog(&factory.http_client()).await.ok();
    let resolver = ConfigProviderResolver::new(config, factory, ModelRegistry::builtin(), None, catalog);

    let mut entries = resolver.list_model_entries().await;
    if let Some(p) = args.provider.as_deref() {
        entries.retain(|e| provider_of(&e.model_ref) == p);
        if entries.is_empty() {
            anyhow::bail!("no models for provider '{p}' — is it configured under `providers` in setting.json?");
        }
    }
    entries.sort_by(|a, b| a.model_ref.cmp(&b.model_ref));

    if args.json {
        println!("{}", serde_json::to_string_pretty(&entries)?);
        return Ok(());
    }
    let mut out = String::new();
    for e in &entries {
        out.push_str(&e.model_ref);
        if args.verbose {
            let mut meta = Vec::new();
            if let Some(c) = e.context_window {
                meta.push(format!("ctx {}", fmt_tokens(c)));
            }
            if let Some(o) = e.max_output_tokens {
                meta.push(format!("out {}", fmt_tokens(o)));
            }
            if let (Some(i), Some(o)) = (e.input_per_mtok, e.output_per_mtok) {
                meta.push(format!("${i:.2}/${o:.2}"));
            }
            if !meta.is_empty() {
                out.push_str(&format!("  ({})", meta.join(" · ")));
            }
        }
        out.push('\n');
    }
    print!("{out}");
    Ok(())
}

/// `1234567 → "1M"`, `200000 → "200k"`, small values unchanged.
fn fmt_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{}M", n / 1_000_000)
    } else if n >= 1_000 {
        format!("{}k", n / 1_000)
    } else {
        n.to_string()
    }
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
    fn read_prompt_arg_prefers_inline_text_else_file() {
        // Inline text wins.
        assert_eq!(read_prompt_arg(Some("inline"), None).unwrap().as_deref(), Some("inline"));
        // File is read when no inline text.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sys.txt");
        std::fs::write(&p, "from file").unwrap();
        assert_eq!(read_prompt_arg(None, Some(p.as_path())).unwrap().as_deref(), Some("from file"));
        // Neither → None.
        assert!(read_prompt_arg(None, None).unwrap().is_none());
        // Missing file errors.
        assert!(read_prompt_arg(None, Some(std::path::Path::new("/no/such/file"))).is_err());
    }

    #[test]
    fn mcp_server_entry_builds_http_or_stdio() {
        let http = mcp_server_entry(None, vec![], Some("https://x/mcp".into()));
        assert_eq!(http["type"], "http");
        assert_eq!(http["url"], "https://x/mcp");
        assert!(http.get("command").is_none());

        let stdio = mcp_server_entry(Some("uvx".into()), vec!["server".into(), "--flag".into()], None);
        assert_eq!(stdio["command"], "uvx");
        assert_eq!(stdio["args"], serde_json::json!(["server", "--flag"]));
        assert!(stdio.get("type").is_none());
    }

    #[test]
    fn mcp_add_then_remove_round_trips_setting_json() {
        let dir = tempfile::tempdir().unwrap();
        let sd = dir.path().join(".stepper");
        // Add writes a mcpServers entry (creating the file/dir on first write).
        stepper_config::scaffold::update_settings(&sd, |obj| {
            let servers = obj.entry("mcpServers").or_insert_with(|| serde_json::json!({}));
            servers
                .as_object_mut()
                .unwrap()
                .insert("fs".into(), mcp_server_entry(Some("uvx".into()), vec!["mcp-fs".into()], None));
        })
        .unwrap();
        let written = std::fs::read_to_string(sd.join("setting.json")).unwrap();
        assert!(written.contains("\"fs\"") && written.contains("mcp-fs"), "added: {written}");
        // Remove deletes it.
        let mut removed = false;
        stepper_config::scaffold::update_settings(&sd, |obj| {
            if let Some(map) = obj.get_mut("mcpServers").and_then(|v| v.as_object_mut()) {
                removed = map.remove("fs").is_some();
            }
        })
        .unwrap();
        assert!(removed);
        assert!(!std::fs::read_to_string(sd.join("setting.json")).unwrap().contains("mcp-fs"));
    }

    #[test]
    fn appended_role_places_extra_after_role() {
        assert_eq!(appended_role("You are a reviewer.", "Be terse."), "You are a reviewer.\n\nBe terse.");
        // Trailing whitespace on the role is trimmed before the join.
        assert_eq!(appended_role("role\n\n", "extra"), "role\n\nextra");
        // A blank role yields just the appended text (no leading blank lines).
        assert_eq!(appended_role("   ", "extra"), "extra");
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
        // A space-containing name can't round-trip through `#name` routing → error,
        // even if such a directory name were configured.
        let spaced = vec!["my agent".to_string()];
        assert!(
            agent_prompt("my agent", "p", &spaced).unwrap_err().to_string().contains("whitespace"),
        );
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

    #[test]
    fn strip_code_fence_unwraps_fenced_json() {
        assert_eq!(strip_code_fence("{\"a\":1}"), "{\"a\":1}");
        assert_eq!(strip_code_fence("```json\n{\"a\":1}\n```"), "{\"a\":1}");
        assert_eq!(strip_code_fence("```\n{\"a\":1}\n```"), "{\"a\":1}");
    }

    #[test]
    fn validate_output_enforces_the_schema() {
        let schema = serde_json::json!({
            "type": "object",
            "required": ["name"],
            "properties": { "name": { "type": "string" } }
        });
        let v = jsonschema::validator_for(&schema).unwrap();
        // Conforming (bare and fenced).
        assert!(validate_output(&v, "{\"name\":\"ok\"}").is_ok());
        assert!(validate_output(&v, "```json\n{\"name\":\"ok\"}\n```").is_ok());
        // Missing required key, wrong type, and non-JSON all fail.
        assert!(validate_output(&v, "{\"other\":1}").is_err());
        assert!(validate_output(&v, "{\"name\":5}").is_err());
        assert!(validate_output(&v, "not json at all").is_err());
    }

    #[test]
    fn compile_output_schema_accepts_inline_and_rejects_bad_json() {
        assert!(compile_output_schema("{\"type\":\"string\"}").is_ok());
        assert!(compile_output_schema("{not json}").is_err());
    }
}
