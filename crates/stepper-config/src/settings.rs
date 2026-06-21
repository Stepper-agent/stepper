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
    /// Fallback model chain (`provider/model-id`), tried in order when a step's
    /// primary model fails non-retryably or exhausts its retries. A bare string
    /// is a single fallback; an array is the ordered chain. `--fallback-model`
    /// (comma-separated) overrides this for a run. See `fallback_models()`.
    #[serde(default)]
    pub fallback_model: Option<FallbackModels>,
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
    /// Terminal-bell notifications. Omitted/`false` = silent (the default); `true`
    /// rings the bell when a turn completes, an approval is awaited, or a turn
    /// errors; an object sets each trigger independently. Delivery is a portable
    /// terminal bell (`\x07`) emitted by the TUI — no OS notifications or sounds.
    #[serde(default)]
    pub notification: Option<NotificationConfig>,
    /// Explicit HTTP/HTTPS proxy for all outbound requests (provider calls,
    /// `web_fetch`, http MCP). Omitted = honor the standard `HTTP(S)_PROXY` /
    /// `NO_PROXY` environment variables (reqwest's default). Setting it overrides
    /// the environment; `disabled: true` forces a direct connection.
    #[serde(default)]
    pub proxy: Option<ProxyConfig>,
}

/// `setting.json` `fallbackModel`: a single `provider/model-id` string or an
/// ordered array of them. Normalize with [`FallbackModels::into_vec`].
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum FallbackModels {
    /// `fallbackModel: "provider/model-id"`.
    One(String),
    /// `fallbackModel: ["provider/a", "provider/b"]` (tried in order).
    Many(Vec<String>),
}

impl FallbackModels {
    /// Flatten to the ordered chain, trimming each entry and dropping empties.
    pub fn into_vec(self) -> Vec<String> {
        let trim_keep = |m: String| {
            let t = m.trim();
            (!t.is_empty()).then(|| t.to_string())
        };
        match self {
            FallbackModels::One(m) => trim_keep(m).into_iter().collect(),
            FallbackModels::Many(v) => v.into_iter().filter_map(trim_keep).collect(),
        }
    }
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

/// `setting.json` `notification`: a master on/off switch or per-trigger flags for
/// the terminal bell. Mirrors the `formatter`/`lsp` untagged shape.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum NotificationConfig {
    /// `notification: true` (bell on every trigger) / `false` (silent).
    All(bool),
    /// `notification: { onComplete, onApproval, onError }`.
    Detailed(NotificationDetail),
}

/// Per-trigger bell flags. A present object defaults each trigger to on (so
/// `notification: {}` rings on all three); `enabled: false` silences everything.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NotificationDetail {
    /// Master switch for the object form (default on). `enabled: false` = silent.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Ring when a turn finishes.
    #[serde(default)]
    pub on_complete: Option<bool>,
    /// Ring when an approval prompt appears.
    #[serde(default)]
    pub on_approval: Option<bool>,
    /// Ring when a turn errors.
    #[serde(default)]
    pub on_error: Option<bool>,
}

impl NotificationConfig {
    /// `(on_complete, on_approval, on_error)`. An absent config (`None`) maps to
    /// all-false at the call site — silence is the default.
    pub fn resolve(&self) -> (bool, bool, bool) {
        match self {
            NotificationConfig::All(on) => (*on, *on, *on),
            NotificationConfig::Detailed(detail) => {
                if detail.enabled == Some(false) {
                    return (false, false, false);
                }
                (
                    detail.on_complete.unwrap_or(true),
                    detail.on_approval.unwrap_or(true),
                    detail.on_error.unwrap_or(true),
                )
            }
        }
    }
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

/// Explicit proxy. Any of `http`/`https`/`all` set REPLACES the environment
/// proxy (reqwest turns off env auto-proxy once an explicit proxy is set);
/// `disabled: true` forces a direct connection (ignoring the environment).
/// `noProxy` is a comma-separated host/suffix bypass list for the explicit proxy.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProxyConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub https: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub all: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_proxy: Option<String>,
    #[serde(default)]
    pub disabled: bool,
}

impl ProxyConfig {
    /// Whether this asks for any non-default behavior (an explicit proxy or a
    /// forced direct connection). `false` → leave reqwest on its env default. A
    /// lone `noProxy` with no proxy URL is meaningless, so it counts as inactive.
    pub fn is_active(&self) -> bool {
        self.disabled || self.http.is_some() || self.https.is_some() || self.all.is_some()
    }
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
    /// Per-model overrides keyed by model id (the part after `provider/`). A
    /// model's entry wins over the provider-wide `contextWindow` — useful for
    /// per-model limits / pricing the catalog doesn't carry.
    #[serde(default)]
    pub models: BTreeMap<String, ModelOverride>,
}

/// Per-model overrides in a provider's `models` map: limits and pricing that win
/// over the catalog/registry estimate (and the provider-wide `contextWindow`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ModelOverride {
    #[serde(default)]
    pub context_window: Option<u64>,
    #[serde(default)]
    pub max_output_tokens: Option<u64>,
    #[serde(default)]
    pub input_per_mtok: Option<f64>,
    #[serde(default)]
    pub output_per_mtok: Option<f64>,
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
    /// Whether to connect this server. `false` keeps it in config but skips it at
    /// startup; omitted or `true` connects.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Working directory for a stdio server (relative paths resolve against the
    /// project root). Ignored for http.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Per-server connect/handshake timeout in milliseconds (overrides the global
    /// `STEPPER_MCP_CONNECT_TIMEOUT_MS` / 10s default).
    #[serde(default)]
    pub timeout: Option<u64>,
    /// OAuth for a remote (http) server. Present = use stored creds if any (run
    /// `stepper mcp auth <name>` to obtain them); `oauth.disabled = true` opts out.
    /// Tokens live in `~/.stepper/mcp-auth.json` (0600), never in this file.
    #[serde(default)]
    pub oauth: Option<McpOAuthConfig>,
}

/// Per-server MCP OAuth. Omit `clientId` to use RFC 7591 Dynamic Client
/// Registration; supply it (and optionally `clientSecret`) for a pre-registered
/// client. `scope` defaults to what the server advertises. Tokens and any DCR
/// registration are persisted to the keyless `~/.stepper/mcp-auth.json` store —
/// never here.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct McpOAuthConfig {
    /// Pre-registered OAuth client id (omitted → Dynamic Client Registration).
    #[serde(default)]
    pub client_id: Option<String>,
    /// Confidential-client secret (public PKCE client when omitted).
    #[serde(default)]
    pub client_secret: Option<String>,
    /// Requested scopes (empty → the server's advertised default).
    #[serde(default)]
    pub scope: Vec<String>,
    /// Local redirect-listener port (default 33418, +1 fallback).
    #[serde(default)]
    pub callback_port: Option<u16>,
    /// Full redirect URI override (otherwise `http://127.0.0.1:<port>/callback`).
    #[serde(default)]
    pub redirect_uri: Option<String>,
    /// Opt out of OAuth for this server even though it is http.
    #[serde(default)]
    pub disabled: bool,
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
    fn notification_config_resolves_each_form() {
        // Absent = silent.
        assert!(serde_json::from_str::<SettingsFile>("{}").unwrap().notification.is_none());
        // Bare bool: true rings everything, false silences everything.
        let on: SettingsFile = serde_json::from_str(r#"{"notification":true}"#).unwrap();
        assert_eq!(on.notification.unwrap().resolve(), (true, true, true));
        let off: SettingsFile = serde_json::from_str(r#"{"notification":false}"#).unwrap();
        assert_eq!(off.notification.unwrap().resolve(), (false, false, false));
        // Empty object opts in to all three.
        let empty: SettingsFile = serde_json::from_str(r#"{"notification":{}}"#).unwrap();
        assert_eq!(empty.notification.unwrap().resolve(), (true, true, true));
        // Per-trigger: silence the noisy turn-complete, keep approval+error.
        let detail: SettingsFile =
            serde_json::from_str(r#"{"notification":{"onComplete":false}}"#).unwrap();
        assert_eq!(detail.notification.unwrap().resolve(), (false, true, true));
        // `enabled: false` overrides the per-trigger flags.
        let disabled: SettingsFile =
            serde_json::from_str(r#"{"notification":{"enabled":false,"onError":true}}"#).unwrap();
        assert_eq!(disabled.notification.unwrap().resolve(), (false, false, false));
    }

    #[test]
    fn mcp_oauth_config_parses_full_empty_and_disabled() {
        // Auto-detect default: oauth present, empty (DCR + advertised scopes).
        let auto: SettingsFile =
            serde_json::from_str(r#"{"mcpServers":{"s":{"type":"http","url":"https://x","oauth":{}}}}"#)
                .unwrap();
        let o = auto.mcp_servers.get("s").unwrap().oauth.clone().unwrap();
        assert!(o.client_id.is_none() && o.scope.is_empty() && !o.disabled);
        // Full pre-registered confidential client with explicit scopes + port.
        let full: SettingsFile = serde_json::from_str(
            r#"{"mcpServers":{"s":{"oauth":{"clientId":"abc","clientSecret":"sh","scope":["read","write"],"callbackPort":40000}}}}"#,
        )
        .unwrap();
        let o = full.mcp_servers.get("s").unwrap().oauth.clone().unwrap();
        assert_eq!(o.client_id.as_deref(), Some("abc"));
        assert_eq!(o.client_secret.as_deref(), Some("sh"));
        assert_eq!(o.scope, vec!["read".to_string(), "write".to_string()]);
        assert_eq!(o.callback_port, Some(40000));
        // Opt out.
        let off: SettingsFile =
            serde_json::from_str(r#"{"mcpServers":{"s":{"oauth":{"disabled":true}}}}"#).unwrap();
        assert!(off.mcp_servers.get("s").unwrap().oauth.as_ref().unwrap().disabled);
        // Absent oauth = None.
        let none: SettingsFile = serde_json::from_str(r#"{"mcpServers":{"s":{"type":"http"}}}"#).unwrap();
        assert!(none.mcp_servers.get("s").unwrap().oauth.is_none());
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
    fn proxy_config_parses_and_camelcases_no_proxy() {
        // Absent by default.
        assert!(serde_json::from_str::<SettingsFile>("{}").unwrap().proxy.is_none());
        // A full config (camelCase noProxy) parses and is active.
        let s: SettingsFile = serde_json::from_str(
            r#"{"proxy":{"http":"http://p:3128","https":"http://p:3128","all":"http://p:8080","noProxy":"localhost,127.0.0.1"}}"#,
        )
        .unwrap();
        let p = s.proxy.unwrap();
        assert_eq!(p.http.as_deref(), Some("http://p:3128"));
        assert_eq!(p.all.as_deref(), Some("http://p:8080"));
        assert_eq!(p.no_proxy.as_deref(), Some("localhost,127.0.0.1"));
        assert!(!p.disabled);
        assert!(p.is_active());
        // `disabled: true` alone (forced direct) is active.
        let d: SettingsFile = serde_json::from_str(r#"{"proxy":{"disabled":true}}"#).unwrap();
        assert!(d.proxy.unwrap().is_active());
        // An empty block / lone noProxy is inactive (no proxy URL, not disabled).
        let empty: SettingsFile = serde_json::from_str(r#"{"proxy":{}}"#).unwrap();
        assert!(!empty.proxy.unwrap().is_active());
        let lone: SettingsFile = serde_json::from_str(r#"{"proxy":{"noProxy":"localhost"}}"#).unwrap();
        assert!(!lone.proxy.unwrap().is_active());
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

    #[test]
    fn fallback_model_accepts_a_string_or_an_array() {
        // Absent by default.
        assert!(serde_json::from_str::<SettingsFile>("{}").unwrap().fallback_model.is_none());
        // Bare string → one-element chain.
        let one: SettingsFile = serde_json::from_str(r#"{"fallbackModel":"anthropic/claude-opus-4-8"}"#).unwrap();
        assert_eq!(one.fallback_model.unwrap().into_vec(), vec!["anthropic/claude-opus-4-8"]);
        // Array → ordered chain.
        let many: SettingsFile =
            serde_json::from_str(r#"{"fallbackModel":["openai/gpt-5","anthropic/claude-sonnet-4-6"]}"#).unwrap();
        assert_eq!(
            many.fallback_model.unwrap().into_vec(),
            vec!["openai/gpt-5", "anthropic/claude-sonnet-4-6"]
        );
        // Empty/blank entries are dropped, and surrounding whitespace is trimmed
        // (so a `"a, b"`-style value doesn't keep a leading space on `b`).
        assert!(FallbackModels::One("  ".into()).into_vec().is_empty());
        assert_eq!(FallbackModels::Many(vec!["a".into(), "".into()]).into_vec(), vec!["a"]);
        assert_eq!(FallbackModels::One("  p/m  ".into()).into_vec(), vec!["p/m"]);
        assert_eq!(FallbackModels::Many(vec!["p/a".into(), " p/b ".into()]).into_vec(), vec!["p/a", "p/b"]);
    }
}
