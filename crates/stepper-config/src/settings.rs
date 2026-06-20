use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// The provider `kind` values the resolver actually understands. `kind` stays a
/// `String` in serde (so an unknown kind still parses and can be reported with
/// context), but the JSON Schema and value-level validation constrain it here.
pub const PROVIDER_KINDS: [&str; 4] = ["openai-compat", "anthropic", "openai-responses", "codex"];

fn provider_kind_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": "string",
        "enum": PROVIDER_KINDS,
    })
}

/// The parsed `.stepper/setting.json`. `serde(default)` everywhere + no
/// `deny_unknown_fields` keeps it forward-compatible: unknown keys are ignored
/// rather than failing the load.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SettingsFile {
    #[serde(default)]
    pub step: Vec<String>,
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub default_model: Option<String>,
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderConfig>,
    #[serde(default)]
    pub orchestrator: Option<OrchestratorConfig>,
    #[serde(default)]
    pub layers: BTreeMap<String, Value>,
    #[serde(default)]
    pub permissions: Permissions,
    #[serde(default)]
    pub approvals: Vec<ApprovalRule>,
    #[serde(default)]
    pub mcp_servers: BTreeMap<String, McpServerConfig>,
    #[serde(default)]
    pub hooks: BTreeMap<String, Vec<HookEntry>>,
    #[serde(default)]
    pub compaction: Option<CompactionConfig>,
    #[serde(default)]
    pub dispatch: Option<DispatchConfig>,
    /// Name of an output style from `.stepper/output-styles/*.md` (the style's
    /// body swaps into the system prompt — consumed by the orchestrator).
    #[serde(default)]
    pub output_style: Option<String>,
    /// Global reasoning effort (`off|low|medium|high`) applied to every layer
    /// that doesn't set its own `reasoning-effort`/`thinking-budget` frontmatter.
    /// Set via `/effort` or `--effort`. Maps to OpenAI `reasoning_effort` +
    /// Anthropic extended-thinking budget.
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    /// Optional per-turn runaway guards. All `None` (the default) means no
    /// limit — a turn runs until the agent finishes. A matching CLI flag
    /// (`--turn-timeout` / `--max-budget-usd` / `--max-turns`) overrides these.
    #[serde(default)]
    pub limits: Option<LimitsConfig>,
    /// Opt-in OS-level bash sandbox (off by default). When enabled, the `bash`
    /// tool's filesystem writes are confined to the project root +
    /// `permissions.additionalDirectories`; it is a best-effort backstop under
    /// the permission engine and a no-op on unsupported platforms.
    #[serde(default)]
    pub sandbox: Option<SandboxConfig>,
    /// TUI color theme (set via the `/theme` editor). `preset` names a built-in
    /// palette; `colors` are per-role `name → color` overrides applied on top.
    #[serde(default)]
    pub theme: Option<ThemeConfig>,
    /// Format-on-edit. Omitted/`false` = disabled (the default); `true` = enable
    /// every built-in formatter; an object keeps built-ins on while adding
    /// per-formatter overrides and custom formatters. Consumed by the
    /// orchestrator, which runs the matching formatter after a file-editing tool.
    #[serde(default)]
    pub formatter: Option<FormatterConfig>,
    /// LSP diagnostics on edit. Omitted/`false` = disabled (the default); `true` =
    /// use every built-in language server **found on PATH**; an object keeps
    /// built-ins on while adding per-server overrides and custom servers. Servers
    /// are never downloaded — only installed ones are used.
    #[serde(default)]
    pub lsp: Option<LspConfig>,
}

/// `setting.json` `lsp`: a master on/off switch or a map of per-server overrides
/// (built-ins detected on PATH stay enabled; an entry with a `command` +
/// `extensions` for an unknown name defines a custom server).
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum LspConfig {
    /// `lsp: true` (all installed built-ins) / `lsp: false` (all off).
    All(bool),
    /// `lsp: { "<id>": { ... } }`.
    Map(BTreeMap<String, LspServerEntry>),
}

/// A single language server's configuration (override of a built-in, or custom).
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LspServerEntry {
    /// Disable this server even though built-ins are on.
    #[serde(default)]
    pub disabled: bool,
    /// argv to launch the server (program + args). Required for a custom server;
    /// overrides the built-in command when set.
    #[serde(default)]
    pub command: Option<Vec<String>>,
    /// File extensions this server handles (overrides the built-in list).
    #[serde(default)]
    pub extensions: Option<Vec<String>>,
    /// Environment variables to set when launching the server.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// LSP `initializationOptions` passed in the `initialize` request.
    #[serde(default)]
    pub initialization: Option<Value>,
}

/// `setting.json` `formatter`: either a master on/off switch or a map of
/// per-formatter overrides (built-ins stay enabled; an entry with a `command` +
/// `extensions` for an unknown name defines a custom formatter).
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum FormatterConfig {
    /// `formatter: true` (all built-ins) / `formatter: false` (all off).
    All(bool),
    /// `formatter: { "<name>": { ... } }`.
    Map(BTreeMap<String, FormatterEntry>),
}

/// A single formatter's configuration (override of a built-in, or a custom one).
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FormatterEntry {
    /// Disable this formatter even though built-ins are on.
    #[serde(default)]
    pub disabled: bool,
    /// The command to run (argv with a `$FILE` placeholder). Required for a custom
    /// formatter; overrides the built-in command when set.
    #[serde(default)]
    pub command: Option<Vec<String>>,
    /// Environment variables to set when running the formatter.
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
    /// File extensions this formatter handles (overrides the built-in list).
    #[serde(default)]
    pub extensions: Option<Vec<String>>,
}

/// Opt-in OS-level sandbox for the `bash` tool. A best-effort defense-in-depth
/// layer (macOS Seatbelt today): with `enabled = true`, shell writes outside the
/// writable set fail, so a permission-engine miss can't escape the project.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SandboxConfig {
    #[serde(default)]
    pub enabled: bool,
}

/// TUI color theme: a built-in `preset` palette plus per-role color overrides.
/// Color strings are `#RRGGBB`, a named color (e.g. `cyan`), or a 0–255 index.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThemeConfig {
    #[serde(default)]
    pub preset: Option<String>,
    #[serde(default)]
    pub colors: BTreeMap<String, String>,
}

/// Per-turn safety limits. Each field is opt-in; omit it (or set `null`) for no
/// limit on that axis. Set at first-run setup or by hand.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LimitsConfig {
    /// Wall-clock seconds a single turn may run before it is stopped (counts time
    /// spent waiting at an approval prompt too). `0`/absent = no limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_timeout_secs: Option<u64>,
    /// USD the session may spend before a turn is stopped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_budget_usd: Option<f64>,
    /// Total ReAct steps (provider requests) a turn may take across all layers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<u32>,
}

impl LimitsConfig {
    /// Whether any limit is set (so callers can skip writing an empty block).
    pub fn is_set(&self) -> bool {
        self.turn_timeout_secs.is_some() || self.max_budget_usd.is_some() || self.max_turns.is_some()
    }
}

/// Context-compaction tuning. `provider` names a (typically cheap) model used to
/// summarize folded-away history; without it, compaction uses a heuristic marker.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CompactionConfig {
    #[serde(default)]
    pub provider: Option<String>,
}

/// Fan-out tuning. `enabled` exposes the model-callable `dispatch` tool;
/// `concurrency`/`stepCap` bound the dispatched sub-agents (defaults: 8, and the
/// calling layer's step cap).
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DispatchConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub concurrency: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_cap: Option<usize>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProviderConfig {
    /// `openai-compat` | `anthropic` | `openai-responses` | `codex`.
    #[schemars(schema_with = "provider_kind_schema")]
    pub kind: String,
    #[serde(default)]
    pub base_url: Option<String>,
    /// Literal key, a `{env:VAR}` template, or null (localhost / oauth).
    #[serde(default)]
    pub api_key: Option<String>,
    /// e.g. `codex-oauth` for the ChatGPT-OAuth path.
    #[serde(default)]
    pub auth: Option<String>,
    #[serde(default)]
    pub default_model: Option<String>,
    /// Override the context window (tokens) for this provider's models — used by
    /// the ctx% footer when a model is not in the built-in registry. `/v1/models`
    /// auto-probe is provider-specific and left as a follow-up.
    #[serde(default)]
    pub context_window: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OrchestratorConfig {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub temperature: Option<f32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Permissions {
    #[serde(default)]
    pub default_mode: Option<String>,
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub ask: Vec<String>,
    #[serde(default)]
    pub deny: Vec<String>,
    #[serde(default)]
    pub additional_directories: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalRule {
    pub rule: String,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub granted_at: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct McpServerConfig {
    /// `stdio` | `http`.
    #[serde(default, rename = "type")]
    pub transport: Option<String>,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub always_load: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HookEntry {
    #[serde(default)]
    pub matcher: Option<String>,
    pub command: String,
}

/// Deep-merge `over` (project) onto `base` (user): objects merge key-by-key,
/// everything else (arrays, scalars) is replaced wholesale — so project `step`
/// and `approvals` arrays override rather than concatenate.
pub fn deep_merge(base: &mut Value, over: Value) {
    match (base, over) {
        (Value::Object(b), Value::Object(o)) => {
            for (k, v) in o {
                match b.get_mut(&k) {
                    Some(bv) => deep_merge(bv, v),
                    None => {
                        b.insert(k, v);
                    }
                }
            }
        }
        (b, o) => *b = o,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatch_config_parses_optional_caps() {
        let s: SettingsFile =
            serde_json::from_str(r#"{"dispatch":{"enabled":true,"concurrency":4,"stepCap":120}}"#)
                .unwrap();
        let d = s.dispatch.unwrap();
        assert!(d.enabled);
        assert_eq!(d.concurrency, Some(4));
        assert_eq!(d.step_cap, Some(120));
    }

    #[test]
    fn dispatch_caps_default_to_none() {
        let s: SettingsFile = serde_json::from_str(r#"{"dispatch":{"enabled":true}}"#).unwrap();
        let d = s.dispatch.unwrap();
        assert_eq!(d.concurrency, None);
        assert_eq!(d.step_cap, None);
    }

    #[test]
    fn theme_config_parses_preset_and_color_overrides() {
        let s: SettingsFile = serde_json::from_str(
            r##"{"theme":{"preset":"dracula","colors":{"accent":"#ff0000","error":"red"}}}"##,
        )
        .unwrap();
        let t = s.theme.unwrap();
        assert_eq!(t.preset.as_deref(), Some("dracula"));
        assert_eq!(t.colors.get("accent").map(String::as_str), Some("#ff0000"));
        assert_eq!(t.colors.get("error").map(String::as_str), Some("red"));
        // Absent by default; an empty block parses to no preset/overrides.
        assert!(serde_json::from_str::<SettingsFile>("{}").unwrap().theme.is_none());
        let empty: SettingsFile = serde_json::from_str(r#"{"theme":{}}"#).unwrap();
        let t = empty.theme.unwrap();
        assert!(t.preset.is_none() && t.colors.is_empty());
    }

    #[test]
    fn sandbox_is_absent_by_default() {
        let s: SettingsFile = serde_json::from_str("{}").unwrap();
        assert!(s.sandbox.is_none());
        // An explicit empty block parses but stays disabled.
        let s: SettingsFile = serde_json::from_str(r#"{"sandbox":{}}"#).unwrap();
        assert!(!s.sandbox.unwrap().enabled);
    }

    #[test]
    fn sandbox_enabled_round_trips() {
        let s: SettingsFile = serde_json::from_str(r#"{"sandbox":{"enabled":true}}"#).unwrap();
        assert!(s.sandbox.as_ref().unwrap().enabled);
        let back = serde_json::to_value(&s).unwrap();
        assert_eq!(back["sandbox"]["enabled"], serde_json::json!(true));
    }
}
