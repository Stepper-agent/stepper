//! Assemble a real `Orchestrator` from config + the CLI's convention fallback
//! (so a bare `cargo run --model anthropic/...` works without a `.stepper/`).

use std::path::PathBuf;
use std::sync::Arc;
use stepper_config::{Config, Permissions, ProviderConfig};
use stepper_core::{
    build_steps, load_base_context, ConfigProviderResolver, HookHost, ModelRegistry, Orchestrator,
    SessionLimits,
};
use stepper_mcp::McpManager;
use stepper_permission::{PermissionMode, RuleSet};
use stepper_providers::{CodexTokenStore, ProviderFactory};

pub const DEFAULT_MODEL: &str = "ollama-cloud/qwen3-coder";

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

    let mcp = McpManager::connect(&config.settings.mcp_servers).await;
    let mut base_tools = stepper_tools::ToolRegistry::builtins();
    for tool in mcp.tools() {
        base_tools.register(tool);
    }

    let factory = ProviderFactory::new()?;
    let codex_store = CodexTokenStore::load(CodexTokenStore::default_path(), factory.client()).ok();
    let base_context = load_base_context(&config);
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
        limits,
        fallback_model: fallback_model.map(str::to_string),
        resume_seed: Vec::new(),
    };
    Ok((orchestrator, mcp))
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
}
