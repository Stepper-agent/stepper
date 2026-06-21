//! Assemble a real `Orchestrator` from config + the CLI's convention fallback
//! (so a bare `cargo run --model anthropic/...` works without a `.stepper/`).

use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use stepper_config::{Config, LimitsConfig, LspConfig, Permissions, ProviderConfig};
use stepper_core::{
    build_steps, load_base_context, ConfigProviderResolver, HookHost, LspDiagnostics, ModelRegistry,
    Orchestrator, SessionLimits,
};
use stepper_lsp::{builtin_catalog, catalog::resolve_builtin, LspManager, ServerSpec};
use stepper_mcp::McpManager;
use stepper_permission::{PermissionMode, RuleSet};
use stepper_providers::{CodexTokenStore, ProviderFactory};

/// Bridges `stepper-lsp`'s manager to the `core` port so `core` stays decoupled
/// from the LSP crate (only the CLI wires it).
struct LspBridge(Arc<LspManager>);

#[async_trait]
impl LspDiagnostics for LspBridge {
    async fn diagnostics_after_edit(&self, path: &Path) -> String {
        self.0.diagnostics_after_edit(path).await
    }
}

/// Resolve `settings.lsp` into the language servers to run. Omitted/`false` →
/// none; `true` → every built-in **found on PATH**; a map keeps installed
/// built-ins on while applying per-server overrides and adding custom servers
/// (an unknown id with `command` + `extensions`). Servers are never downloaded.
fn resolve_lsp_servers(cfg: Option<&LspConfig>) -> Vec<ServerSpec> {
    use std::collections::BTreeMap;
    match cfg {
        None | Some(LspConfig::All(false)) => Vec::new(),
        Some(LspConfig::All(true)) => builtin_catalog().iter().filter_map(resolve_builtin).collect(),
        Some(LspConfig::Map(map)) => {
            // Installed built-ins (minus disabled), keyed by id.
            let mut by_id: BTreeMap<String, ServerSpec> = builtin_catalog()
                .iter()
                .filter(|b| !map.get(b.id).map(|e| e.disabled).unwrap_or(false))
                .filter_map(|b| resolve_builtin(b).map(|s| (s.id.clone(), s)))
                .collect();
            for (id, entry) in map {
                if entry.disabled {
                    continue;
                }
                let env: Vec<(String, String)> =
                    entry.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                match by_id.get_mut(id) {
                    // Override an installed built-in in place.
                    Some(s) => {
                        if let Some(cmd) = &entry.command {
                            s.command = cmd.clone();
                        }
                        if let Some(ext) = &entry.extensions {
                            s.extensions = ext.clone();
                        }
                        if !env.is_empty() {
                            s.env = env;
                        }
                        if entry.initialization.is_some() {
                            s.initialization = entry.initialization.clone();
                        }
                    }
                    // Custom server, or a built-in override whose default binary
                    // isn't installed: needs a command, and extensions (falling
                    // back to the built-in's list when the id is a known built-in).
                    None => {
                        let builtin = builtin_catalog().iter().find(|b| b.id == id.as_str());
                        let extensions = entry.extensions.clone().or_else(|| {
                            builtin.map(|b| b.extensions.iter().map(|s| s.to_string()).collect())
                        });
                        if let (Some(command), Some(extensions)) = (entry.command.clone(), extensions)
                        {
                            by_id.insert(
                                id.clone(),
                                ServerSpec {
                                    id: id.clone(),
                                    command,
                                    extensions,
                                    env,
                                    initialization: entry.initialization.clone(),
                                },
                            );
                        }
                    }
                }
            }
            by_id.into_values().collect()
        }
    }
}

pub const DEFAULT_MODEL: &str = "ollama-cloud/qwen3-coder";

/// Fold `setting.json` `limits` under the CLI-flag limits: each axis takes the
/// CLI value when present, else the config value, else stays unset (no limit).
fn merge_limits(cli: SessionLimits, config: Option<&LimitsConfig>) -> SessionLimits {
    let cfg = config.cloned().unwrap_or_default();
    SessionLimits::new(
        cli.max_turns.or(cfg.max_turns),
        cli.max_budget_usd.or(cfg.max_budget_usd),
        cli.turn_timeout
            .or_else(|| cfg.turn_timeout_secs.map(std::time::Duration::from_secs)),
    )
}

/// Max fallback links honored (mirrors Claude Code's chain cap).
const MAX_FALLBACK_MODELS: usize = 3;

/// The effective fallback chain: the CLI `--fallback-model` list wins when given,
/// else `setting.json` `fallbackModel`. Deduplicated (first occurrence kept) and
/// capped at [`MAX_FALLBACK_MODELS`]. Each link is re-resolved at run time.
pub(crate) fn resolve_fallback_models(cli_fallbacks: &[String], config: &Config) -> Vec<String> {
    let raw: Vec<String> = if cli_fallbacks.is_empty() {
        config.settings.fallback_model.clone().map(|f| f.into_vec()).unwrap_or_default()
    } else {
        cli_fallbacks.to_vec()
    };
    let mut seen = std::collections::HashSet::new();
    // Trim before filtering/dedup: a comma list with spaces (`a, b`) and an
    // already-listed entry with stray whitespace must collapse, not slip through
    // as `" b"` which would later fail to resolve to a provider.
    raw.into_iter()
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty())
        .filter(|m| seen.insert(m.clone()))
        .take(MAX_FALLBACK_MODELS)
        .collect()
}

/// Build the orchestrator and connect MCP servers. The returned `McpManager`
/// must be kept alive for the session (it owns the live connections).
/// `cli_fallbacks` (`--fallback-model`) overrides `setting.json` `fallbackModel`;
/// the resulting chain is re-resolved in order when a step's primary model fails
/// non-retryably or exhausts its retries.
pub async fn build_orchestrator_with_fallback(
    model: Option<&str>,
    cli_fallbacks: &[String],
    cli_mode: Option<PermissionMode>,
    cli_effort: Option<String>,
    cwd: PathBuf,
    limits: SessionLimits,
) -> anyhow::Result<(Orchestrator, McpManager)> {
    let mut config = Config::load(&cwd)?;
    // `--effort` wins over `setting.json` `reasoningEffort` for this run.
    if cli_effort.is_some() {
        config.settings.reasoning_effort = cli_effort;
    }
    // Effective limits: a CLI flag wins; otherwise fall back to `setting.json`
    // `limits`; otherwise no limit. (Set at first-run setup, per project.)
    let limits = merge_limits(limits, config.settings.limits.as_ref());
    let default_model = model.unwrap_or(DEFAULT_MODEL).to_string();
    // Precedence: `--mode` flag > `setting.json` `mode` >
    // `permissions.defaultMode` > Auto. Auto is the autonomous default: it runs
    // read-only tools and in-project edits without prompting and only asks before
    // out-of-project writes (see stepper-permission `mode_default_path`).
    let mode = cli_mode
        .or_else(|| config.settings.mode.as_deref().and_then(parse_mode))
        .or_else(|| {
            config
                .settings
                .permissions
                .default_mode
                .as_deref()
                .and_then(parse_mode)
        })
        .unwrap_or(PermissionMode::Auto);

    let fallback_models = resolve_fallback_models(cli_fallbacks, &config);
    let steps = build_steps(&config, &default_model);
    for step in &steps {
        ensure_provider(&mut config, &step.model_ref);
    }
    for fallback in &fallback_models {
        ensure_provider(&mut config, fallback);
    }

    let always_load_mcp: Vec<String> = config
        .settings
        .mcp_servers
        .iter()
        .filter(|(_, cfg)| cfg.always_load)
        .map(|(name, _)| name.clone())
        .collect();
    let compaction_model = config
        .settings
        .compaction
        .as_ref()
        .and_then(|c| c.provider.clone());
    let dispatch_enabled = config
        .settings
        .dispatch
        .as_ref()
        .map(|d| d.enabled)
        .unwrap_or(false);
    let dispatch_concurrency = config
        .settings
        .dispatch
        .as_ref()
        .and_then(|d| d.concurrency)
        .unwrap_or(8);
    let dispatch_step_cap = config.settings.dispatch.as_ref().and_then(|d| d.step_cap);

    // Explicit proxy from config (None → reqwest's env-proxy default) routes every
    // outbound client: provider calls, `web_fetch`, and http MCP.
    let proxy = config.settings.proxy.clone();

    // stdio MCP servers' `cwd` resolves against the project root (or cwd if none).
    let mcp_base = config.project_root.as_deref().unwrap_or(cwd.as_path());
    let mcp = McpManager::connect(&config.settings.mcp_servers, mcp_base, proxy.as_ref()).await;
    let mut base_tools = stepper_tools::ToolRegistry::builtins_with_proxy(proxy.clone());
    for tool in mcp.tools() {
        base_tools.register(tool);
    }

    let factory = ProviderFactory::with_proxy(proxy.as_ref())?;
    let codex_store = CodexTokenStore::load(CodexTokenStore::default_path(), factory.client()).ok();
    // Seed ModelInfo (context window + pricing) from the live models.dev catalog
    // so unknown-but-cataloged models get real figures instead of the builtin
    // estimate. Best-effort — an offline/failed fetch falls back to the registry.
    let catalog = match stepper_providers::models::fetch_catalog(&factory.http_client()).await {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("warning: models.dev catalog fetch failed, using the builtin model table: {e}");
            None
        }
    };
    // The selected `outputStyle` body is folded into the base context so it
    // reaches every layer's system prompt (via `compose_system`) and is counted
    // honestly by `/context` (as part of `base_context`).
    let base_context = with_output_style(&config, load_base_context(&config, &cwd));
    let project_root = config.project_root.clone().unwrap_or_else(|| cwd.clone());

    let approvals: Vec<String> = config
        .settings
        .approvals
        .iter()
        .map(|a| a.rule.clone())
        .collect();
    // `additionalDirectories` (relative entries resolve against project_root):
    // they extend the in-project set for the permission engine AND, when the OS
    // sandbox is enabled, the bash writable roots.
    let additional_dirs: Vec<PathBuf> = config
        .settings
        .permissions
        .additional_directories
        .iter()
        .map(|d| {
            let p = PathBuf::from(d);
            if p.is_absolute() { p } else { project_root.join(p) }
        })
        .collect();

    // Live, mutable session state: AlwaysAllow grants and /allow|/deny|/ask write
    // `rules`; Shift+Tab and exit_plan_mode write `mode`.
    let rules = Arc::new(std::sync::RwLock::new(
        checked_rules(&config.settings.permissions, &approvals)?
            .with_additional_dirs(additional_dirs.clone()),
    ));
    let mode = Arc::new(std::sync::RwLock::new(mode));

    // Opt-in OS bash sandbox: when `sandbox.enabled`, confine `bash` writes to the
    // project root + `permissions.additionalDirectories`. `None` keeps today's
    // unconfined behavior. Read before `config` moves into the resolver below.
    let sandbox_writable_roots = config
        .settings
        .sandbox
        .as_ref()
        .filter(|s| s.enabled)
        .map(|_| {
            let mut roots = vec![project_root.clone()];
            roots.extend(additional_dirs.clone());
            roots
        });

    // Format-on-edit: resolve `settings.formatter` into the active formatter set
    // before `config` moves into the resolver. Empty = disabled (the default).
    let formatters = Arc::new(stepper_core::resolve_formatters(
        config.settings.formatter.as_ref(),
    ));

    // Named sub-agents (`.stepper/agents/`), exposed via the `task` tool. Read
    // before `config` moves into the resolver.
    let agents = Arc::new(stepper_core::load_agents(&config));

    // LSP diagnostics: resolve `settings.lsp` into the installed/custom servers and
    // build the session-scoped manager. `None` when no server is configured.
    let lsp_servers = resolve_lsp_servers(config.settings.lsp.as_ref());
    let lsp: Option<Arc<dyn LspDiagnostics>> = (!lsp_servers.is_empty()).then(|| {
        let manager = Arc::new(LspManager::new(project_root.clone(), lsp_servers));
        Arc::new(LspBridge(manager)) as Arc<dyn LspDiagnostics>
    });

    let hooks = Arc::new(HookHost::new(config.settings.hooks.clone(), cwd.clone()));

    let resolver = Arc::new(ConfigProviderResolver::new(
        config,
        factory,
        ModelRegistry::builtin(),
        codex_store,
        catalog,
    ));

    let orchestrator = Orchestrator {
        resolver,
        base_tools,
        steps,
        base_context,
        project_root,
        cwd,
        home: std::env::var_os("HOME").map(PathBuf::from),
        rules,
        mode,
        hooks,
        always_load_mcp,
        compaction_model,
        dispatch_enabled,
        dispatch_concurrency,
        dispatch_step_cap,
        limits,
        fallback_models,
        resume_seed: Vec::new(),
        sandbox_writable_roots,
        formatters,
        lsp,
        agents,
    };
    Ok((orchestrator, mcp))
}

/// Append the active `outputStyle` body (if any) to the base context. An unset
/// or unknown style name leaves the base context unchanged (`validate_values`
/// surfaces a name that resolves to nothing).
fn with_output_style(config: &Config, base: String) -> String {
    let Some(name) = config.settings.output_style.as_deref() else {
        return base;
    };
    match config.output_styles().into_iter().find(|s| s.name == name) {
        Some(style) if base.is_empty() => style.body,
        Some(style) => format!("{base}\n\n{}", style.body),
        None => base,
    }
}

/// Parse the permission rule lists, failing startup on a malformed DENY spec
/// (silently dropping it would fail open) and warning about each dropped
/// allow/ask/approval spec. Persisted approvals fold in as allow rules.
fn checked_rules(perms: &Permissions, approvals: &[String]) -> anyhow::Result<RuleSet> {
    let (mut rules, dropped) =
        RuleSet::from_lists_checked(&perms.allow, &perms.ask, &perms.deny).map_err(|e| {
            anyhow::anyhow!("refusing to start: {e} — fix `permissions.deny` in .stepper/setting.json")
        })?;
    for spec in &dropped {
        eprintln!("warning: ignoring malformed permission rule: {spec}");
    }
    let (approval_rules, dropped_approvals) =
        stepper_permission::rule::parse_all_checked(approvals);
    for spec in &dropped_approvals {
        eprintln!("warning: ignoring malformed approval rule: {spec}");
    }
    rules.allow.extend(approval_rules);
    Ok(rules)
}

/// Parse a `setting.json` `mode` / `permissions.defaultMode` string into a
/// `PermissionMode`. `bypass` is deliberately not accepted here: the
/// model-writable settings file must never grant bypass-permissions — only the
/// explicit `--dangerously-skip-permissions` flag can.
pub fn parse_mode(s: &str) -> Option<PermissionMode> {
    match s.trim().to_ascii_lowercase().as_str() {
        "auto" => Some(PermissionMode::Auto),
        "plan" => Some(PermissionMode::Plan),
        "accept-edits" | "acceptedits" | "accept_edits" => Some(PermissionMode::AcceptEdits),
        "default" => Some(PermissionMode::Default),
        "dont-ask" | "dontask" | "dont_ask" => Some(PermissionMode::DontAsk),
        _ => None,
    }
}

/// If a model names a provider not in config, synthesize one from the CLI's
/// known-provider convention (key still comes from `STEPPER_<P>_API_KEY`).
pub(crate) fn ensure_provider(config: &mut Config, model_ref: &str) {
    let Some((name, _)) = model_ref.split_once('/') else {
        return;
    };
    if config.settings.providers.contains_key(name) {
        return;
    }
    // Only synthesize for the known convention providers. An unknown name is left
    // unconfigured so it surfaces a clear `UnknownProvider` error — never silently
    // routed to api.openai.com (which would also pop a wrong-host key prompt).
    if let Some(pc) = convention_provider(name) {
        config.settings.providers.insert(name.to_string(), pc);
    }
}

fn convention_provider(name: &str) -> Option<ProviderConfig> {
    let mut pc = ProviderConfig {
        kind: "openai-compat".into(),
        base_url: None,
        api_key: None,
        auth: None,
        default_model: None,
        context_window: None,
        models: Default::default(),
    };
    match name {
        "anthropic" => pc.kind = "anthropic".into(),
        "openai" => pc.base_url = Some("https://api.openai.com/v1".into()),
        "ollama-cloud" => pc.base_url = Some("https://ollama.com/v1".into()),
        "omlx" | "mlx" => pc.base_url = Some("http://localhost:8000/v1".into()),
        "codex" => {
            pc.kind = "openai-responses".into();
            pc.auth = Some("codex-oauth".into());
        }
        // An unknown provider must be configured explicitly (with a baseUrl) in
        // `.stepper/setting.json`; we do not guess a host for it.
        _ => return None,
    }
    Some(pc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use stepper_config::LspServerEntry;

    #[test]
    fn lsp_omitted_or_false_runs_no_servers() {
        assert!(resolve_lsp_servers(None).is_empty());
        assert!(resolve_lsp_servers(Some(&LspConfig::All(false))).is_empty());
    }

    #[test]
    fn lsp_map_adds_a_custom_server() {
        let mut map = std::collections::BTreeMap::new();
        map.insert(
            "my-lsp".to_string(),
            LspServerEntry {
                command: Some(vec!["my-lsp-server".into(), "--stdio".into()]),
                extensions: Some(vec![".foo".into()]),
                ..Default::default()
            },
        );
        let servers = resolve_lsp_servers(Some(&LspConfig::Map(map)));
        let custom = servers
            .iter()
            .find(|s| s.id == "my-lsp")
            .expect("custom server present");
        assert_eq!(custom.command, vec!["my-lsp-server", "--stdio"]);
        assert_eq!(custom.extensions, vec![".foo".to_string()]);
    }

    #[test]
    fn lsp_map_override_of_uninstalled_builtin_uses_builtin_extensions() {
        // rust-analyzer likely isn't on PATH in CI; overriding its command makes
        // it available and inherits the built-in's `.rs` extension.
        let mut map = std::collections::BTreeMap::new();
        map.insert(
            "rust-analyzer".to_string(),
            LspServerEntry {
                command: Some(vec!["/custom/ra".into()]),
                ..Default::default()
            },
        );
        let servers = resolve_lsp_servers(Some(&LspConfig::Map(map)));
        let ra = servers
            .iter()
            .find(|s| s.id == "rust-analyzer")
            .expect("override present");
        assert_eq!(ra.command, vec!["/custom/ra"]);
        assert!(ra.extensions.iter().any(|e| e == ".rs"));
    }

    fn config_with_fallback(fallback: Option<stepper_config::FallbackModels>) -> Config {
        Config {
            settings: stepper_config::SettingsFile {
                fallback_model: fallback,
                ..Default::default()
            },
            project_dir: None,
            project_root: None,
            user_dir: None,
        }
    }

    #[test]
    fn resolve_fallback_models_prefers_cli_then_config_dedupes_and_caps() {
        use stepper_config::FallbackModels;
        // CLI list wins over the config chain entirely.
        let cfg = config_with_fallback(Some(FallbackModels::One("cfg/only".into())));
        assert_eq!(
            resolve_fallback_models(&["cli/a".to_string(), "cli/b".to_string()], &cfg),
            vec!["cli/a", "cli/b"]
        );
        // No CLI list → fall back to the config chain.
        assert_eq!(resolve_fallback_models(&[], &cfg), vec!["cfg/only"]);
        // Duplicates collapse (first kept) and the chain is capped at 3.
        let cli = ["p/a", "p/a", "p/b", "p/c", "p/d"].map(String::from);
        assert_eq!(
            resolve_fallback_models(&cli, &config_with_fallback(None)),
            vec!["p/a", "p/b", "p/c"]
        );
        // Nothing configured → empty.
        assert!(resolve_fallback_models(&[], &config_with_fallback(None)).is_empty());
        // Whitespace (a `--fallback-model "a, b"` split) is trimmed, and trimming
        // collapses a spaced duplicate with its bare form.
        let spaced = ["p/a".to_string(), " p/b".to_string(), " p/a ".to_string()];
        assert_eq!(resolve_fallback_models(&spaced, &config_with_fallback(None)), vec!["p/a", "p/b"]);
    }

    #[test]
    fn convention_provider_does_not_route_unknown_names_to_openai() {
        // Known providers are synthesized with their real host...
        assert_eq!(
            convention_provider("openai").and_then(|p| p.base_url).as_deref(),
            Some("https://api.openai.com/v1")
        );
        assert_eq!(convention_provider("anthropic").map(|p| p.kind), Some("anthropic".into()));
        // ...but an unknown name is NOT guessed (no silent api.openai.com fallback),
        // so it surfaces a clear UnknownProvider error instead of a misroute.
        assert!(convention_provider("groq").is_none());
        assert!(convention_provider("openrouter").is_none());
    }

    #[test]
    fn merge_limits_prefers_cli_then_config_then_none() {
        use std::time::Duration;
        let cfg = LimitsConfig {
            turn_timeout_secs: Some(600),
            max_budget_usd: Some(5.0),
            max_turns: Some(100),
        };
        // No CLI flags → config values fill in.
        let merged = merge_limits(SessionLimits::new(None, None, None), Some(&cfg));
        assert_eq!(merged.turn_timeout, Some(Duration::from_secs(600)));
        assert_eq!(merged.max_budget_usd, Some(5.0));
        assert_eq!(merged.max_turns, Some(100));

        // A CLI flag wins over the config value on its axis.
        let cli = SessionLimits::new(Some(7), None, Some(Duration::from_secs(30)));
        let merged = merge_limits(cli, Some(&cfg));
        assert_eq!(merged.max_turns, Some(7), "CLI --max-turns wins");
        assert_eq!(merged.turn_timeout, Some(Duration::from_secs(30)), "CLI --turn-timeout wins");
        assert_eq!(merged.max_budget_usd, Some(5.0), "unset CLI axis falls back to config");

        // No config and no flags → no limits.
        let merged = merge_limits(SessionLimits::new(None, None, None), None);
        assert!(merged.turn_timeout.is_none() && merged.max_turns.is_none() && merged.max_budget_usd.is_none());

        // A zero on any axis (from a flag or a hand-edited config) means "no
        // limit" — never an instant kill. Goes through SessionLimits::new.
        let zeros = LimitsConfig {
            turn_timeout_secs: Some(0),
            max_budget_usd: Some(0.0),
            max_turns: Some(0),
        };
        let merged = merge_limits(SessionLimits::new(None, None, None), Some(&zeros));
        assert!(
            merged.turn_timeout.is_none() && merged.max_turns.is_none() && merged.max_budget_usd.is_none(),
            "zero on any axis normalizes to no limit"
        );
    }

    #[test]
    fn parse_mode_accepts_known_strings_and_rejects_others() {
        assert_eq!(parse_mode("auto"), Some(PermissionMode::Auto));
        assert_eq!(parse_mode("plan"), Some(PermissionMode::Plan));
        assert_eq!(parse_mode("accept-edits"), Some(PermissionMode::AcceptEdits));
        assert_eq!(parse_mode(" Plan "), Some(PermissionMode::Plan));
        assert_eq!(parse_mode("default"), Some(PermissionMode::Default));
        assert_eq!(parse_mode("dont-ask"), Some(PermissionMode::DontAsk));
        assert_eq!(parse_mode("nonsense"), None);
    }

    #[test]
    fn parse_mode_never_grants_bypass_from_settings() {
        assert_eq!(parse_mode("bypass"), None);
        assert_eq!(parse_mode("bypass-permissions"), None);
        assert_eq!(parse_mode("bypassPermissions"), None);
    }

    #[test]
    fn checked_rules_rejects_malformed_deny_at_startup() {
        let perms = Permissions {
            deny: vec!["Bash(rm *".into()],
            ..Permissions::default()
        };
        let err = checked_rules(&perms, &[]).unwrap_err();
        assert!(err.to_string().contains("Bash(rm *"), "{err}");
    }

    #[test]
    fn checked_rules_drops_malformed_allow_but_starts() {
        let perms = Permissions {
            allow: vec!["Bash(cargo *)".into(), "Bash(broken".into()],
            deny: vec!["Bash(rm -rf *)".into()],
            ..Permissions::default()
        };
        let rules = checked_rules(&perms, &["Bash(git status)".into()]).unwrap();
        assert_eq!(rules.allow.len(), 2);
        assert_eq!(rules.deny.len(), 1);
    }

    fn config_with_style(project: &std::path::Path, output_style: Option<&str>) -> Config {
        Config {
            settings: stepper_config::SettingsFile {
                output_style: output_style.map(str::to_string),
                ..Default::default()
            },
            project_dir: Some(project.to_path_buf()),
            project_root: project.parent().map(std::path::Path::to_path_buf),
            user_dir: None,
        }
    }

    #[test]
    fn with_output_style_appends_selected_body_and_ignores_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join(".stepper");
        let styles = project.join("output-styles");
        std::fs::create_dir_all(&styles).unwrap();
        std::fs::write(styles.join("terse.md"), "Answer in one line.\n").unwrap();

        // selected style → its body is appended after the base context.
        let cfg = config_with_style(&project, Some("terse"));
        let out = with_output_style(&cfg, "BASE".into());
        assert_eq!(out, "BASE\n\nAnswer in one line.");

        // empty base → the body stands alone (no leading separator).
        let out = with_output_style(&cfg, String::new());
        assert_eq!(out, "Answer in one line.");

        // unknown style name → base unchanged (validate_values reports it).
        let cfg = config_with_style(&project, Some("missing"));
        assert_eq!(with_output_style(&cfg, "BASE".into()), "BASE");

        // no style selected → base unchanged.
        let cfg = config_with_style(&project, None);
        assert_eq!(with_output_style(&cfg, "BASE".into()), "BASE");
    }
}
