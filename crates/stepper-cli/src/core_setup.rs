//! Assemble a real `Orchestrator` from config + the CLI's convention fallback
//! (so a bare `cargo run --model anthropic/...` works without a `.stepper/`).

use std::path::PathBuf;
use std::sync::Arc;
use stepper_config::{Config, LimitsConfig, Permissions, ProviderConfig};
use stepper_core::{
    build_steps, load_base_context, ConfigProviderResolver, HookHost, ModelRegistry, Orchestrator,
    SessionLimits,
};
use stepper_mcp::McpManager;
use stepper_permission::{PermissionMode, RuleSet};
use stepper_providers::{CodexTokenStore, ProviderFactory};

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

/// Build the orchestrator and connect MCP servers. The returned `McpManager`
/// must be kept alive for the session (it owns the live connections).
/// `fallback_model` (`--fallback-model`) is re-resolved once when a step's
/// primary model fails non-retryably or exhausts its retries.
pub async fn build_orchestrator_with_fallback(
    model: Option<&str>,
    fallback_model: Option<&str>,
    cli_mode: Option<PermissionMode>,
    cwd: PathBuf,
    limits: SessionLimits,
) -> anyhow::Result<(Orchestrator, McpManager)> {
    let mut config = Config::load(&cwd)?;
    // Effective limits: a CLI flag wins; otherwise fall back to `setting.json`
    // `limits`; otherwise no limit. (Set at first-run setup, per project.)
    let limits = merge_limits(limits, config.settings.limits.as_ref());
    let default_model = model.unwrap_or(DEFAULT_MODEL).to_string();
    // Precedence: `--mode` flag > `setting.json` `mode` >
    // `permissions.defaultMode` > AcceptEdits.
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
        .unwrap_or(PermissionMode::AcceptEdits);

    let steps = build_steps(&config, &default_model);
    for step in &steps {
        ensure_provider(&mut config, &step.model_ref);
    }
    if let Some(fallback) = fallback_model {
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

    let mcp = McpManager::connect(&config.settings.mcp_servers).await;
    let mut base_tools = stepper_tools::ToolRegistry::builtins();
    for tool in mcp.tools() {
        base_tools.register(tool);
    }

    let factory = ProviderFactory::new()?;
    let codex_store = CodexTokenStore::load(CodexTokenStore::default_path(), factory.client()).ok();
    // The selected `outputStyle` body is folded into the base context so it
    // reaches every layer's system prompt (via `compose_system`) and is counted
    // honestly by `/context` (as part of `base_context`).
    let base_context = with_output_style(&config, load_base_context(&config));
    let project_root = config.project_root.clone().unwrap_or_else(|| cwd.clone());

    let approvals: Vec<String> = config
        .settings
        .approvals
        .iter()
        .map(|a| a.rule.clone())
        .collect();
    let rules = Arc::new(checked_rules(&config.settings.permissions, &approvals)?);

    let hooks = Arc::new(HookHost::new(config.settings.hooks.clone(), cwd.clone()));

    let resolver = Arc::new(ConfigProviderResolver::new(
        config,
        factory,
        ModelRegistry::builtin(),
        codex_store,
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
        fallback_model: fallback_model.map(str::to_string),
        resume_seed: Vec::new(),
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
fn ensure_provider(config: &mut Config, model_ref: &str) {
    let Some((name, _)) = model_ref.split_once('/') else {
        return;
    };
    if config.settings.providers.contains_key(name) {
        return;
    }
    config
        .settings
        .providers
        .insert(name.to_string(), convention_provider(name));
}

fn convention_provider(name: &str) -> ProviderConfig {
    let mut pc = ProviderConfig {
        kind: "openai-compat".into(),
        base_url: None,
        api_key: None,
        auth: None,
        default_model: None,
        context_window: None,
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
        _ => pc.base_url = Some("https://api.openai.com/v1".into()),
    }
    pc
}

#[cfg(test)]
mod tests {
    use super::*;

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
