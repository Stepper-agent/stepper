//! `stepper-config` — discovery, parsing, and merge of the `.stepper/` contract.
//!
//! This crate is dependency-light (serde only): it owns `setting.json`
//! discovery + user←project deep-merge + model/provider/key resolution. Layer
//! `index.md` frontmatter, skills, commands, and JSON-Schema validation land in
//! a later Phase 3 step alongside the orchestrator that consumes them.

pub mod discovery;
pub mod error;
pub mod frontmatter;
pub mod model;
pub mod schema;
pub mod settings;
pub mod substitution;

pub use discovery::{discover, Discovery};
pub use error::ConfigError;
pub use frontmatter::{
    parse_command, parse_layer, parse_output_style, parse_skill, CommandDef, LayerDef,
    LayerFrontmatter, McpAllow, OutputStyleDef, SkillDef, ToolFilter,
};
pub use model::ResolvedModel;
pub use schema::{settings_schema, validate_settings, validate_settings_values};
pub use settings::{
    deep_merge, ApprovalRule, HookEntry, McpServerConfig, OrchestratorConfig, Permissions,
    ProviderConfig, SettingsFile, PROVIDER_KINDS,
};
pub use substitution::{substitute, CommandArgs, SubstitutionIo};

use serde_json::Value;
use std::path::{Path, PathBuf};

/// A fully loaded configuration: the merged settings plus the located dirs.
#[derive(Debug, Clone)]
pub struct Config {
    pub settings: SettingsFile,
    pub project_dir: Option<PathBuf>,
    pub project_root: Option<PathBuf>,
    pub user_dir: Option<PathBuf>,
}

/// A provider config resolved for a specific model reference, ready to hand to
/// the provider factory (in `stepper-core`). Keeps this crate provider-free.
#[derive(Debug, Clone)]
pub struct ResolvedProvider {
    pub name: String,
    pub kind: String,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub auth: Option<String>,
    pub model: String,
    pub context_window: Option<u64>,
}

impl Config {
    /// Discover and load config for `cwd`: user `~/.stepper/setting.json` is the
    /// base, project `<root>/.stepper/setting.json` is deep-merged on top.
    pub fn load(cwd: &Path) -> Result<Self, ConfigError> {
        let dirs = discover(cwd);

        let mut merged = dirs
            .user_dir
            .as_ref()
            .map(|d| read_value(&d.join("setting.json")))
            .transpose()?
            .flatten()
            .unwrap_or_else(|| Value::Object(Default::default()));

        if let Some(project) = dirs
            .project_dir
            .as_ref()
            .map(|d| read_value(&d.join("setting.json")))
            .transpose()?
            .flatten()
        {
            deep_merge(&mut merged, project);
        }

        let settings: SettingsFile = serde_json::from_value(merged).map_err(|e| {
            ConfigError::Parse {
                path: dirs
                    .project_dir
                    .clone()
                    .unwrap_or_default()
                    .join("setting.json"),
                message: e.to_string(),
            }
        })?;

        Ok(Config {
            settings,
            project_dir: dirs.project_dir,
            project_root: dirs.project_root,
            user_dir: dirs.user_dir,
        })
    }

    /// Build a `Config` straight from an already-parsed `SettingsFile` (tests,
    /// or callers that synthesize settings).
    pub fn from_settings(settings: SettingsFile) -> Self {
        Config {
            settings,
            project_dir: None,
            project_root: None,
            user_dir: None,
        }
    }

    /// The orchestrator's model reference (`orchestrator.model`, else
    /// `defaultModel`).
    pub fn orchestrator_model(&self) -> Option<String> {
        self.settings
            .orchestrator
            .as_ref()
            .and_then(|o| o.model.clone())
            .or_else(|| self.settings.default_model.clone())
    }

    /// A layer's model reference (`layers.<name>.model`, else `defaultModel`).
    pub fn layer_model(&self, layer: &str) -> Option<String> {
        self.settings
            .layers
            .get(layer)
            .and_then(|v| v.get("model"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| self.settings.default_model.clone())
    }

    /// Names of available slash commands (`<.stepper>/commands/<name>.md`), from
    /// both the project and user dirs, deduplicated and sorted (for the `/`
    /// palette).
    pub fn command_names(&self) -> Vec<String> {
        let mut names = std::collections::BTreeSet::new();
        for dir in [self.project_dir.as_ref(), self.user_dir.as_ref()]
            .into_iter()
            .flatten()
        {
            let Ok(entries) = std::fs::read_dir(dir.join("commands")) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|x| x.to_str()) == Some("md")
                    && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
                {
                    names.insert(stem.to_string());
                }
            }
        }
        names.into_iter().collect()
    }

    /// Load the output styles from `<.stepper>/output-styles/*.md` in the
    /// project and user dirs (project wins on a name collision), sorted by
    /// name. Unreadable or bodiless files are skipped (`validate_values`
    /// reports them).
    pub fn output_styles(&self) -> Vec<OutputStyleDef> {
        let mut styles = std::collections::BTreeMap::new();
        for dir in [self.project_dir.as_ref(), self.user_dir.as_ref()]
            .into_iter()
            .flatten()
        {
            for (stem, content) in read_md_files(&dir.join("output-styles")) {
                if let Ok(style) = parse_output_style(&stem, &content) {
                    styles.entry(style.name.clone()).or_insert(style);
                }
            }
        }
        styles.into_values().collect()
    }

    /// Value-level validation across the whole loaded config: settings enums
    /// (provider `kind`, modes, MCP `type`, hook events), layer `on-failure`
    /// frontmatter, broken output styles, and an `outputStyle` that names no
    /// style. Empty = clean.
    pub fn validate_values(&self) -> Vec<String> {
        let mut problems = schema::validate_settings_values(&self.settings);

        let mut seen_layers = std::collections::BTreeSet::new();
        for dir in [self.project_dir.as_ref(), self.user_dir.as_ref()]
            .into_iter()
            .flatten()
        {
            let Ok(entries) = std::fs::read_dir(dir.join("layer")) else {
                continue;
            };
            for entry in entries.flatten() {
                let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                    continue;
                };
                let index = entry.path().join("index.md");
                if !index.is_file() || !seen_layers.insert(name.clone()) {
                    continue;
                }
                match std::fs::read_to_string(&index)
                    .map_err(|e| e.to_string())
                    .and_then(|c| parse_layer(&name, &c).map_err(|e| e.to_string()))
                {
                    Ok(layer) => problems.extend(schema::validate_layer_on_failure(
                        &name,
                        layer.frontmatter.on_failure.as_deref(),
                    )),
                    Err(e) => problems.push(format!("layer/{name}: {e}")),
                }
            }
        }

        let styles = self.output_styles();
        for dir in [self.project_dir.as_ref(), self.user_dir.as_ref()]
            .into_iter()
            .flatten()
        {
            for (stem, content) in read_md_files(&dir.join("output-styles")) {
                if let Err(e) = parse_output_style(&stem, &content) {
                    problems.push(e.to_string());
                }
            }
        }
        if let Some(wanted) = self.settings.output_style.as_deref()
            && !styles.iter().any(|s| s.name == wanted)
        {
            let available: Vec<&str> = styles.iter().map(|s| s.name.as_str()).collect();
            problems.push(format!(
                "outputStyle: no output style named '{wanted}' in .stepper/output-styles (available: {})",
                if available.is_empty() { "none".to_string() } else { available.join(", ") }
            ));
        }

        problems
    }

    /// Resolve a `provider/model-id` reference into a concrete provider
    /// descriptor, applying api-key precedence (env override > config template).
    pub fn resolve_provider(&self, model_ref: &str) -> Result<ResolvedProvider, ConfigError> {
        let resolved = ResolvedModel::parse(model_ref)
            .ok_or_else(|| ConfigError::InvalidModel(model_ref.to_string()))?;
        let provider = self
            .settings
            .providers
            .get(&resolved.provider)
            .ok_or_else(|| ConfigError::UnknownProvider {
                model: model_ref.to_string(),
                provider: resolved.provider.clone(),
            })?;

        Ok(ResolvedProvider {
            api_key: resolve_api_key(&resolved.provider, provider),
            name: resolved.provider,
            kind: provider.kind.clone(),
            base_url: provider.base_url.clone(),
            auth: provider.auth.clone(),
            model: resolved.model_id,
            context_window: provider.context_window,
        })
    }
}

/// `(file stem, content)` for every readable `*.md` directly in `dir`.
fn read_md_files(dir: &Path) -> Vec<(String, String)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|x| x.to_str()) == Some("md")
            && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
            && let Ok(content) = std::fs::read_to_string(&path)
        {
            files.push((stem.to_string(), content));
        }
    }
    files
}

fn read_value(path: &Path) -> Result<Option<Value>, ConfigError> {
    if !path.is_file() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let value = serde_json::from_str(&raw).map_err(|e| ConfigError::Parse {
        path: path.to_path_buf(),
        message: e.to_string(),
    })?;
    Ok(Some(value))
}

/// `STEPPER_<PROVIDER>_API_KEY` env override wins; otherwise the config
/// `apiKey` template is resolved (`{env:VAR}` → env, `none`/null → None, else
/// literal).
fn resolve_api_key(provider_name: &str, provider: &ProviderConfig) -> Option<String> {
    let env_var = format!(
        "STEPPER_{}_API_KEY",
        provider_name.to_ascii_uppercase().replace('-', "_")
    );
    if let Ok(v) = std::env::var(&env_var) {
        let v = v.trim();
        if !v.is_empty() {
            return Some(v.to_string());
        }
    }
    resolve_template(provider.api_key.as_deref()?)
}

fn resolve_template(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == "none" || trimmed == "null" {
        return None;
    }
    if let Some(var) = trimmed
        .strip_prefix("{env:")
        .and_then(|s| s.strip_suffix('}'))
    {
        return std::env::var(var.trim()).ok().filter(|v| !v.is_empty());
    }
    Some(trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deep_merge_replaces_arrays_and_merges_objects() {
        let mut base = serde_json::json!({
            "step": ["a", "b"],
            "providers": { "x": { "kind": "anthropic" } },
            "mode": "auto"
        });
        let over = serde_json::json!({
            "step": ["c"],
            "providers": { "y": { "kind": "openai-compat" } }
        });
        deep_merge(&mut base, over);

        assert_eq!(base["step"], serde_json::json!(["c"]));
        assert_eq!(base["mode"], "auto");
        assert_eq!(base["providers"]["x"]["kind"], "anthropic");
        assert_eq!(base["providers"]["y"]["kind"], "openai-compat");
    }

    #[test]
    fn unknown_fields_are_ignored_for_forward_compat() {
        let settings: SettingsFile = serde_json::from_value(serde_json::json!({
            "step": ["plan"],
            "futureFeature": { "anything": true }
        }))
        .unwrap();
        assert_eq!(settings.step, vec!["plan"]);
    }

    #[test]
    fn resolves_provider_and_env_key_override() {
        // SAFETY: single-threaded test setting a unique env var.
        unsafe {
            std::env::set_var("STEPPER_ANTHROPIC_API_KEY", "from-env");
        }
        let settings: SettingsFile = serde_json::from_value(serde_json::json!({
            "providers": {
                "anthropic": { "kind": "anthropic", "baseUrl": "https://api.anthropic.com", "apiKey": "{env:UNUSED}" }
            }
        }))
        .unwrap();
        let cfg = Config::from_settings(settings);
        let rp = cfg.resolve_provider("anthropic/claude-x").unwrap();
        assert_eq!(rp.kind, "anthropic");
        assert_eq!(rp.model, "claude-x");
        assert_eq!(rp.api_key.as_deref(), Some("from-env"));
        unsafe {
            std::env::remove_var("STEPPER_ANTHROPIC_API_KEY");
        }
    }

    #[test]
    fn localhost_provider_has_no_key() {
        let settings: SettingsFile = serde_json::from_value(serde_json::json!({
            "providers": { "omlx": { "kind": "openai-compat", "baseUrl": "http://localhost:8000/v1", "apiKey": null } }
        }))
        .unwrap();
        let cfg = Config::from_settings(settings);
        let rp = cfg.resolve_provider("omlx/deepseek").unwrap();
        assert_eq!(rp.api_key, None);
        assert_eq!(rp.base_url.as_deref(), Some("http://localhost:8000/v1"));
    }

    #[test]
    fn unknown_provider_and_bad_model_error() {
        let cfg = Config::from_settings(SettingsFile::default());
        assert!(matches!(
            cfg.resolve_provider("missing/m"),
            Err(ConfigError::UnknownProvider { .. })
        ));
        assert!(matches!(
            cfg.resolve_provider("no-slash"),
            Err(ConfigError::InvalidModel(_))
        ));
    }

    fn cfg_with_provider(name: &str, kind: &str, api_key: Value) -> Config {
        let settings: SettingsFile = serde_json::from_value(serde_json::json!({
            "providers": { name: { "kind": kind, "apiKey": api_key } }
        }))
        .unwrap();
        Config::from_settings(settings)
    }

    #[test]
    fn deep_merge_recurses_into_nested_objects() {
        let mut base = serde_json::json!({
            "providers": { "x": { "kind": "anthropic", "baseUrl": "https://old" } }
        });
        let over = serde_json::json!({
            "providers": { "x": { "baseUrl": "https://new", "apiKey": "k" } }
        });
        deep_merge(&mut base, over);
        assert_eq!(base["providers"]["x"]["kind"], "anthropic");
        assert_eq!(base["providers"]["x"]["baseUrl"], "https://new");
        assert_eq!(base["providers"]["x"]["apiKey"], "k");
    }

    #[test]
    fn deep_merge_scalar_replaces_object() {
        let mut base = serde_json::json!({ "orchestrator": { "model": "a/b" } });
        deep_merge(&mut base, serde_json::json!({ "orchestrator": "scalar" }));
        assert_eq!(base["orchestrator"], "scalar");
    }

    #[test]
    fn deep_merge_object_replaces_scalar() {
        let mut base = serde_json::json!({ "mode": "auto" });
        deep_merge(&mut base, serde_json::json!({ "mode": { "nested": true } }));
        assert_eq!(base["mode"], serde_json::json!({ "nested": true }));
    }

    #[test]
    fn deep_merge_null_over_replaces_value() {
        let mut base = serde_json::json!({ "defaultModel": "a/b" });
        deep_merge(&mut base, serde_json::json!({ "defaultModel": null }));
        assert!(base["defaultModel"].is_null());
    }

    #[test]
    fn deep_merge_keeps_base_keys_absent_in_over() {
        let mut base = serde_json::json!({ "step": ["a"], "mode": "auto" });
        deep_merge(&mut base, serde_json::json!({ "step": ["b"] }));
        assert_eq!(base["step"], serde_json::json!(["b"]));
        assert_eq!(base["mode"], "auto");
    }

    #[test]
    fn resolve_provider_uses_literal_api_key() {
        let cfg = cfg_with_provider("acme", "openai-compat", Value::String("sk-literal".into()));
        let rp = cfg.resolve_provider("acme/m").unwrap();
        assert_eq!(rp.api_key.as_deref(), Some("sk-literal"));
        assert_eq!(rp.name, "acme");
    }

    #[test]
    fn resolve_provider_none_template_yields_no_key() {
        let cfg = cfg_with_provider("acme", "openai-compat", Value::String("none".into()));
        assert_eq!(cfg.resolve_provider("acme/m").unwrap().api_key, None);
    }

    #[test]
    fn resolve_provider_carries_context_window_override() {
        let settings: SettingsFile = serde_json::from_value(serde_json::json!({
            "providers": { "acme": { "kind": "openai-compat", "contextWindow": 64000 } }
        }))
        .unwrap();
        let cfg = Config::from_settings(settings);
        assert_eq!(cfg.resolve_provider("acme/m").unwrap().context_window, Some(64000));
    }

    #[test]
    fn resolve_provider_without_context_window_is_none() {
        let cfg = cfg_with_provider("acme", "openai-compat", Value::Null);
        assert_eq!(cfg.resolve_provider("acme/m").unwrap().context_window, None);
    }

    #[test]
    fn resolve_provider_empty_template_yields_no_key() {
        let cfg = cfg_with_provider("acme", "openai-compat", Value::String("   ".into()));
        assert_eq!(cfg.resolve_provider("acme/m").unwrap().api_key, None);
    }

    #[test]
    fn resolve_provider_env_template_reads_named_env() {
        let var = "STEPPER_CFG_TEST_TPL_KEY";
        // SAFETY: single-threaded test setting a unique env var.
        unsafe {
            std::env::set_var(var, "tpl-value");
        }
        let cfg = cfg_with_provider(
            "acme",
            "openai-compat",
            Value::String(format!("{{env:{var}}}")),
        );
        let rp = cfg.resolve_provider("acme/m").unwrap();
        unsafe {
            std::env::remove_var(var);
        }
        assert_eq!(rp.api_key.as_deref(), Some("tpl-value"));
    }

    #[test]
    fn resolve_provider_env_template_missing_var_yields_no_key() {
        let cfg = cfg_with_provider(
            "acme",
            "openai-compat",
            Value::String("{env:STEPPER_CFG_TEST_DEFINITELY_UNSET}".into()),
        );
        assert_eq!(cfg.resolve_provider("acme/m").unwrap().api_key, None);
    }

    #[test]
    fn resolve_provider_env_template_set_but_empty_var_yields_no_key() {
        let var = "STEPPER_CFG_TEST_TPL_EMPTY_KEY";
        // SAFETY: single-threaded test setting a unique env var.
        unsafe {
            std::env::set_var(var, "");
        }
        let cfg = cfg_with_provider(
            "acme",
            "openai-compat",
            Value::String(format!("{{env:{var}}}")),
        );
        let key = cfg.resolve_provider("acme/m").unwrap().api_key;
        unsafe {
            std::env::remove_var(var);
        }
        assert_eq!(key, None);
    }

    #[test]
    fn stepper_env_override_beats_env_template() {
        let override_var = "STEPPER_OVERPROV_API_KEY";
        let tpl_var = "STEPPER_CFG_TEST_LOSER_KEY";
        // SAFETY: single-threaded test setting unique env vars.
        unsafe {
            std::env::set_var(override_var, "winner");
            std::env::set_var(tpl_var, "loser");
        }
        let cfg = cfg_with_provider(
            "overprov",
            "openai-compat",
            Value::String(format!("{{env:{tpl_var}}}")),
        );
        let rp = cfg.resolve_provider("overprov/m").unwrap();
        unsafe {
            std::env::remove_var(override_var);
            std::env::remove_var(tpl_var);
        }
        assert_eq!(rp.api_key.as_deref(), Some("winner"));
    }

    #[test]
    fn stepper_env_var_name_uppercases_and_replaces_dashes() {
        let var = "STEPPER_OPENAI_RESPONSES_API_KEY";
        // SAFETY: single-threaded test setting a unique env var.
        unsafe {
            std::env::set_var(var, "responses-key");
        }
        let cfg = cfg_with_provider("openai-responses", "openai-responses", Value::Null);
        let rp = cfg.resolve_provider("openai-responses/gpt-x").unwrap();
        unsafe {
            std::env::remove_var(var);
        }
        assert_eq!(rp.api_key.as_deref(), Some("responses-key"));
    }

    #[test]
    fn stepper_env_override_blank_falls_through_to_template() {
        let override_var = "STEPPER_BLANKPROV_API_KEY";
        // SAFETY: single-threaded test setting a unique env var.
        unsafe {
            std::env::set_var(override_var, "   ");
        }
        let cfg = cfg_with_provider(
            "blankprov",
            "openai-compat",
            Value::String("sk-fallback".into()),
        );
        let rp = cfg.resolve_provider("blankprov/m").unwrap();
        unsafe {
            std::env::remove_var(override_var);
        }
        assert_eq!(rp.api_key.as_deref(), Some("sk-fallback"));
    }

    #[test]
    fn resolve_provider_carries_auth_and_base_url() {
        let settings: SettingsFile = serde_json::from_value(serde_json::json!({
            "providers": {
                "chatgpt": { "kind": "openai-responses", "auth": "codex-oauth", "baseUrl": "https://chatgpt.com/backend" }
            }
        }))
        .unwrap();
        let cfg = Config::from_settings(settings);
        let rp = cfg.resolve_provider("chatgpt/gpt-5").unwrap();
        assert_eq!(rp.auth.as_deref(), Some("codex-oauth"));
        assert_eq!(rp.base_url.as_deref(), Some("https://chatgpt.com/backend"));
        assert_eq!(rp.model, "gpt-5");
        assert_eq!(rp.api_key, None);
    }
}
