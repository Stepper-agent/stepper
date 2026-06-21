//! JSON Schema generation + validation for `setting.json`. The schema is derived
//! from the Rust types (single source of truth) and emitted by
//! `stepper config --schema`; validation reuses the serde parse so a malformed
//! file reports a precise path/message via `stepper config --validate`.

use crate::error::ConfigError;
use crate::settings::{SettingsFile, PROVIDER_KINDS};
use serde_json::Value;
use std::path::PathBuf;

const MODE_NAMES: [&str; 9] = [
    "auto",
    "plan",
    "accept-edits",
    "acceptedits",
    "accept_edits",
    "default",
    "dont-ask",
    "dontask",
    "dont_ask",
];
const MCP_TRANSPORTS: [&str; 3] = ["stdio", "http", "streamable-http"];
const HOOK_EVENTS: [&str; 9] = [
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PreCompact",
    "SubagentStop",
    "Notification",
    "Stop",
    "SessionEnd",
];
const FAILURE_POLICIES: [&str; 2] = ["stop", "skip"];

/// The JSON Schema for `setting.json`, derived from `SettingsFile`.
pub fn settings_schema() -> Value {
    serde_json::to_value(schemars::schema_for!(SettingsFile))
        .unwrap_or(Value::Object(Default::default()))
}

/// Validate a raw `setting.json` string by parsing it into `SettingsFile`
/// (structural validation). Returns the parsed settings on success.
pub fn validate_settings(raw: &str) -> Result<SettingsFile, ConfigError> {
    serde_json::from_str(raw).map_err(|e| ConfigError::Parse {
        path: PathBuf::from("setting.json"),
        message: e.to_string(),
    })
}

/// Value-level validation on top of the structural parse: enum-like string
/// fields (provider `kind`, permission mode names, MCP `type`, hook event
/// names, layer `on-failure`) must hold known values. Returns one message per
/// problem; an empty list means the settings values are clean.
pub fn validate_settings_values(settings: &SettingsFile) -> Vec<String> {
    let mut problems = Vec::new();

    for (name, provider) in &settings.providers {
        if !PROVIDER_KINDS.contains(&provider.kind.as_str()) {
            problems.push(format!(
                "providers.{name}.kind: unknown kind '{}' (expected one of: {})",
                provider.kind,
                PROVIDER_KINDS.join(", ")
            ));
        }
    }

    if let Some(mode) = settings.mode.as_deref() {
        check_mode("mode", mode, &mut problems);
    }
    if let Some(mode) = settings.permissions.default_mode.as_deref() {
        check_mode("permissions.defaultMode", mode, &mut problems);
    }

    for (name, server) in &settings.mcp_servers {
        if let Some(t) = server.transport.as_deref()
            && !MCP_TRANSPORTS.contains(&t)
        {
            problems.push(format!(
                "mcpServers.{name}.type: unknown type '{t}' (expected one of: {})",
                MCP_TRANSPORTS.join(", ")
            ));
        }
    }

    for event in settings.hooks.keys() {
        if !HOOK_EVENTS.contains(&event.as_str()) {
            problems.push(format!(
                "hooks.{event}: unknown hook event (expected one of: {})",
                HOOK_EVENTS.join(", ")
            ));
        }
    }

    for (name, layer) in &settings.layers {
        let on_failure = ["on-failure", "onFailure", "on_failure"]
            .iter()
            .find_map(|k| layer.get(k));
        if let Some(v) = on_failure
            && !matches!(v.as_str(), Some(s) if FAILURE_POLICIES.contains(&s))
        {
            problems.push(format!(
                "layers.{name}.on-failure: invalid value {v} (expected one of: {})",
                FAILURE_POLICIES.join(", ")
            ));
        }
    }

    problems
}

/// `bypass` is called out separately: it is a real mode, but deliberately not
/// settable from the (model-writable) settings file.
fn check_mode(field: &str, mode: &str, problems: &mut Vec<String>) {
    let normalized = mode.trim().to_ascii_lowercase();
    if normalized == "bypass" {
        problems.push(format!(
            "{field}: 'bypass' cannot be set from setting.json (use --dangerously-skip-permissions)"
        ));
    } else if !MODE_NAMES.contains(&normalized.as_str()) {
        problems.push(format!(
            "{field}: unknown permission mode '{mode}' (expected one of: auto, plan, accept-edits, default, dont-ask)"
        ));
    }
}

/// Validate a layer's `on-failure` frontmatter value (the typo `skipp` would
/// otherwise silently mean `stop` at startup).
pub fn validate_layer_on_failure(layer_name: &str, on_failure: Option<&str>) -> Option<String> {
    let v = on_failure?;
    (!FAILURE_POLICIES.contains(&v)).then(|| {
        format!(
            "layer/{layer_name}: invalid on-failure '{v}' (expected one of: {})",
            FAILURE_POLICIES.join(", ")
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_has_known_top_level_properties() {
        let schema = settings_schema();
        let props = &schema["properties"];
        assert!(props.get("step").is_some());
        assert!(props.get("providers").is_some());
        assert!(props.get("mcpServers").is_some());
        assert!(props.get("defaultModel").is_some());
    }

    #[test]
    fn validate_accepts_good_and_rejects_bad() {
        assert!(validate_settings(r#"{"step":["plan"],"mode":"auto"}"#).is_ok());
        assert!(validate_settings(r#"{"step": "not-an-array"}"#).is_err());
    }

    #[test]
    fn validate_returns_parsed_settings_on_success() {
        let parsed = validate_settings(
            r#"{"step":["plan","implement"],"defaultModel":"omlx/deepseek","providers":{"omlx":{"kind":"openai-compat"}}}"#,
        )
        .unwrap();
        assert_eq!(parsed.step, vec!["plan", "implement"]);
        assert_eq!(parsed.default_model.as_deref(), Some("omlx/deepseek"));
        assert_eq!(parsed.providers.get("omlx").map(|p| p.kind.as_str()), Some("openai-compat"));
    }

    #[test]
    fn validate_empty_object_yields_defaults() {
        let parsed = validate_settings("{}").unwrap();
        assert!(parsed.step.is_empty());
        assert!(parsed.providers.is_empty());
        assert_eq!(parsed.mode, None);
    }

    #[test]
    fn validate_rejects_malformed_json() {
        assert!(matches!(
            validate_settings("{ this is not json"),
            Err(ConfigError::Parse { .. })
        ));
    }

    #[test]
    fn validate_rejects_wrong_typed_provider_field() {
        assert!(validate_settings(r#"{"providers":{"x":{"kind":123}}}"#).is_err());
    }

    #[test]
    fn validate_rejects_provider_missing_required_kind() {
        assert!(validate_settings(r#"{"providers":{"x":{"baseUrl":"https://y"}}}"#).is_err());
    }

    #[test]
    fn validate_error_path_is_setting_json() {
        let err = validate_settings(r#"{"step": 5}"#).unwrap_err();
        match err {
            ConfigError::Parse { path, .. } => {
                assert_eq!(path, std::path::PathBuf::from("setting.json"))
            }
            other => panic!("expected Parse error, got {other:?}"),
        }
    }

    #[test]
    fn schema_marks_kind_required_on_provider() {
        let schema = settings_schema();
        let defs = schema
            .get("definitions")
            .or_else(|| schema.get("$defs"))
            .and_then(Value::as_object)
            .expect("schema should expose component definitions");
        let provider = defs
            .get("ProviderConfig")
            .expect("ProviderConfig definition present");
        let required = provider["required"].as_array().expect("required array");
        assert!(required.iter().any(|v| v == "kind"));
    }

    #[test]
    fn schema_enum_constrains_provider_kind() {
        let schema = settings_schema();
        let defs = schema
            .get("definitions")
            .or_else(|| schema.get("$defs"))
            .and_then(Value::as_object)
            .expect("schema should expose component definitions");
        let kind = &defs["ProviderConfig"]["properties"]["kind"];
        let variants = kind["enum"].as_array().expect("kind has an enum constraint");
        let names: Vec<&str> = variants.iter().filter_map(Value::as_str).collect();
        assert_eq!(names, PROVIDER_KINDS);
    }

    fn settings_from(json: serde_json::Value) -> SettingsFile {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn values_clean_settings_yield_no_problems() {
        let settings = settings_from(serde_json::json!({
            "mode": "accept-edits",
            "providers": { "omlx": { "kind": "openai-compat" }, "ant": { "kind": "anthropic" } },
            "permissions": { "defaultMode": "plan" },
            "mcpServers": { "a": { "type": "stdio" }, "b": { "type": "http" }, "c": {} },
            "hooks": { "SessionStart": [], "PreToolUse": [], "PostToolUse": [], "Stop": [] },
            "layers": { "plan": { "model": "x/y", "on-failure": "skip" } }
        }));
        assert_eq!(validate_settings_values(&settings), Vec::<String>::new());
    }

    #[test]
    fn values_reject_unknown_provider_kind() {
        let settings = settings_from(serde_json::json!({
            "providers": { "typo": { "kind": "openai-compt" } }
        }));
        let problems = validate_settings_values(&settings);
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("providers.typo.kind"), "got: {}", problems[0]);
        assert!(problems[0].contains("openai-compt"), "got: {}", problems[0]);
        assert!(problems[0].contains("openai-compat"), "lists known kinds: {}", problems[0]);
    }

    #[test]
    fn values_reject_unknown_mode_names() {
        let settings = settings_from(serde_json::json!({
            "mode": "yolo",
            "permissions": { "defaultMode": "ask-me" }
        }));
        let problems = validate_settings_values(&settings);
        assert_eq!(problems.len(), 2);
        assert!(problems[0].starts_with("mode:"), "got: {}", problems[0]);
        assert!(problems[1].starts_with("permissions.defaultMode:"), "got: {}", problems[1]);
    }

    #[test]
    fn values_accept_mode_spelling_variants() {
        for mode in ["auto", "Plan", " accept-edits ", "acceptEdits", "dont_ask", "DEFAULT"] {
            let settings = settings_from(serde_json::json!({ "mode": mode }));
            assert_eq!(
                validate_settings_values(&settings),
                Vec::<String>::new(),
                "mode {mode:?} should be accepted"
            );
        }
    }

    #[test]
    fn values_reject_bypass_mode_with_dedicated_message() {
        let settings = settings_from(serde_json::json!({ "mode": "bypass" }));
        let problems = validate_settings_values(&settings);
        assert_eq!(problems.len(), 1);
        assert!(
            problems[0].contains("--dangerously-skip-permissions"),
            "got: {}",
            problems[0]
        );
    }

    #[test]
    fn values_reject_unknown_mcp_transport() {
        let settings = settings_from(serde_json::json!({
            "mcpServers": { "srv": { "type": "websocket" } }
        }));
        let problems = validate_settings_values(&settings);
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("mcpServers.srv.type"), "got: {}", problems[0]);
        assert!(problems[0].contains("stdio"), "lists known types: {}", problems[0]);
    }

    #[test]
    fn values_reject_unknown_hook_event() {
        let settings = settings_from(serde_json::json!({
            "hooks": { "OnToolUse": [ { "command": "echo hi" } ] }
        }));
        let problems = validate_settings_values(&settings);
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("hooks.OnToolUse"), "got: {}", problems[0]);
        assert!(problems[0].contains("PreToolUse"), "lists known events: {}", problems[0]);
    }

    #[test]
    fn values_accept_the_extended_lifecycle_hook_events() {
        // The 5 events added for Claude-Code parity all validate as known events.
        let settings = settings_from(serde_json::json!({
            "hooks": {
                "UserPromptSubmit": [ { "command": "echo p" } ],
                "PreCompact": [ { "command": "echo c" } ],
                "SubagentStop": [ { "command": "echo s" } ],
                "Notification": [ { "command": "echo n" } ],
                "SessionEnd": [ { "command": "echo e" } ],
            }
        }));
        assert!(validate_settings_values(&settings).is_empty(), "all five are known events");
    }

    #[test]
    fn values_reject_bad_layer_on_failure_in_settings() {
        let settings = settings_from(serde_json::json!({
            "layers": { "impl": { "on-failure": "skipp" } }
        }));
        let problems = validate_settings_values(&settings);
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("layers.impl.on-failure"), "got: {}", problems[0]);
    }

    #[test]
    fn values_collect_multiple_problems() {
        let settings = settings_from(serde_json::json!({
            "mode": "nope",
            "providers": { "p": { "kind": "wat" } },
            "mcpServers": { "s": { "type": "ftp" } }
        }));
        assert_eq!(validate_settings_values(&settings).len(), 3);
    }

    #[test]
    fn layer_on_failure_helper_accepts_known_and_flags_unknown() {
        assert_eq!(validate_layer_on_failure("plan", None), None);
        assert_eq!(validate_layer_on_failure("plan", Some("stop")), None);
        assert_eq!(validate_layer_on_failure("plan", Some("skip")), None);
        let problem = validate_layer_on_failure("plan", Some("skipp")).unwrap();
        assert!(problem.contains("layer/plan"), "got: {problem}");
        assert!(problem.contains("skipp"), "got: {problem}");
    }
}
