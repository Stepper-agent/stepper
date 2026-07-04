//! Scaffolding the `.stepper/` layout — the skeleton directories, a default
//! `plan → implement → review` layer pipeline, and single layer/command
//! templates. This owns the on-disk format (YAML-frontmatter layer/command
//! files, the `setting.json` `step` array) so the CLI (`stepper init` /
//! `stepper layer new`) and the in-TUI slash commands (`/init`,
//! `/scaffold-layer`, `/layer`, `/command`) share one source of truth.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// A layer/command name safe to turn into a path under `.stepper/`: ASCII
/// alphanumerics plus `-`/`_`, non-empty, bounded — no separators, `..`, dots,
/// or whitespace that could escape the tree or yield a malformed path.
pub fn is_safe_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

const SKELETON_DIRS: &[&str] = &["layer", "commands", "skills", "output-styles"];

/// The default starter pipeline: layer name + its `description` (which is both
/// the required frontmatter field and the human/routing label).
const PIPELINE: &[(&str, &str)] = &[
    (
        "plan",
        "Analyze the request and produce a concise implementation plan.",
    ),
    ("implement", "Carry out the plan: make the code changes."),
    (
        "review",
        "Review the changes for correctness and simplicity; fix what is needed.",
    ),
];

/// Create the `.stepper/` subdirectory skeleton (idempotent). Makes the layout
/// discoverable even before any layer/command/skill exists.
pub fn ensure_skeleton(project_root: &Path) -> io::Result<()> {
    let base = project_root.join(".stepper");
    for dir in SKELETON_DIRS {
        fs::create_dir_all(base.join(dir))?;
    }
    Ok(())
}

fn write_if_absent(path: &Path, contents: &str) -> io::Result<bool> {
    if path.exists() {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, contents)?;
    Ok(true)
}

/// A valid `layer/<name>/index.md`: required `description` frontmatter + a body
/// that becomes the layer's system prompt.
pub fn layer_template(name: &str, description: &str) -> String {
    // Quote the description so a colon (`plan: make …`) or other YAML metachar in
    // it can't break the frontmatter (a double-quoted JSON string is valid YAML).
    let description = serde_json::to_string(description).unwrap_or_else(|_| "\"\"".into());
    format!(
        "---\n\
description: {description}\n\
# model: anthropic/claude-sonnet-4   # optional: a per-layer provider/model\n\
# tools:\n\
#   allow: [Read, Bash]             # optional: restrict this layer's tools\n\
# on-failure: continue             # continue | abort | retry\n\
---\n\
You are the `{name}` layer of a stepper pipeline.\n\n\
Describe this layer's job here — this body is the layer's system prompt (it is\n\
never concatenated with the orchestrator's). When done, hand off a concise\n\
free-text summary of what you produced for the next layer.\n"
    )
}

/// A starter `commands/<name>.md` template (the body is the prompt the command
/// expands into).
pub fn command_template(name: &str) -> String {
    let description = serde_json::to_string(&format!("The {name} command."))
        .unwrap_or_else(|_| "\"\"".into());
    format!(
        "---\n\
description: {description}\n\
argument-hint: <args>\n\
# arguments: [topic]               # name positional args → $topic substitution\n\
# allowed-tools: [Read]\n\
---\n\
Write the prompt this command expands into. Substitutions:\n\
- positional `$1`, `$2`, … or named `$topic` (declare under `arguments`)\n\
- `{{file:path}}` to inline a file, `@include path`, `{{env:VAR}}`\n\
- a shell block to inline command output (gated: needs an explicit allow rule)\n"
    )
}

/// Step names of the default pipeline, in order.
pub fn pipeline_step_names() -> Vec<String> {
    PIPELINE.iter().map(|(n, _)| n.to_string()).collect()
}

/// Write the default `plan → implement → review` layer files (each created only
/// if absent). Returns the paths actually created.
pub fn scaffold_default_pipeline(project_root: &Path) -> io::Result<Vec<PathBuf>> {
    let base = project_root.join(".stepper").join("layer");
    let mut created = Vec::new();
    for (name, desc) in PIPELINE {
        let path = base.join(name).join("index.md");
        if write_if_absent(&path, &layer_template(name, desc))? {
            created.push(path);
        }
    }
    Ok(created)
}

/// Set `setting.json`'s `step` array to `steps`, but only when it is currently
/// empty/absent (so a user's existing pipeline is never clobbered). Other keys
/// are preserved. Returns `true` if it set the steps, `false` if a pipeline was
/// already configured.
pub fn set_pipeline_steps_if_empty(project_root: &Path, steps: &[String]) -> io::Result<bool> {
    let path = project_root.join(".stepper").join("setting.json");
    let mut value: serde_json::Value = match fs::read_to_string(&path) {
        // A present-but-unparseable file must be an error, not silently replaced
        // by the skeleton: that would clobber the user's providers/permissions/mcp
        // (same rule `update_settings`/`import` follow). Only an absent file gets
        // the schema-stamped skeleton.
        Ok(raw) => crate::parse_setting_jsonc(&raw).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} is not valid JSON: {e}", path.display()),
            )
        })?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            serde_json::json!({ "$schema": "stepper://setting.schema.json" })
        }
        Err(e) => return Err(e),
    };
    let already = value
        .get("step")
        .and_then(|s| s.as_array())
        .map(|a| !a.is_empty())
        .unwrap_or(false);
    if already {
        return Ok(false);
    }
    if let Some(obj) = value.as_object_mut() {
        obj.insert("step".into(), serde_json::json!(steps));
    }
    fs::write(&path, format!("{}\n", serde_json::to_string_pretty(&value)?))?;
    Ok(true)
}

/// The single safe read-modify-write seam for runtime `setting.json` changes
/// (persisting a `/model` pick, an AlwaysAllow approval, a `config set`, …):
/// read the object (or the schema-stamped skeleton when absent), apply `edit`,
/// and write it back pretty-printed. A present-but-unparseable file is an error,
/// never clobbered (the same rule `stepper import` learned the hard way).
pub fn update_settings(
    stepper_dir: &Path,
    edit: impl FnOnce(&mut serde_json::Map<String, serde_json::Value>),
) -> io::Result<()> {
    let path = stepper_dir.join("setting.json");
    let mut value: serde_json::Value = match fs::read_to_string(&path) {
        Ok(raw) => crate::parse_setting_jsonc(&raw).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} is not valid JSON: {e}", path.display()),
            )
        })?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(stepper_dir)?;
            serde_json::json!({ "$schema": "stepper://setting.schema.json" })
        }
        Err(e) => return Err(e),
    };
    let Some(obj) = value.as_object_mut() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} is not a JSON object", path.display()),
        ));
    };
    edit(obj);
    fs::write(&path, format!("{}\n", serde_json::to_string_pretty(&value)?))?;
    Ok(())
}

/// The scalar `setting.json` keys editable via `stepper config set/get`: the
/// dotted CLI key, its object path, and the value kind to parse the raw input as.
enum ScalarKind {
    Str,
    U64,
    F64,
    Bool,
}

const SCALAR_KEYS: &[(&str, &[&str], ScalarKind)] = &[
    ("defaultModel", &["defaultModel"], ScalarKind::Str),
    ("mode", &["mode"], ScalarKind::Str),
    ("limits.turnTimeoutSecs", &["limits", "turnTimeoutSecs"], ScalarKind::U64),
    ("limits.maxBudgetUsd", &["limits", "maxBudgetUsd"], ScalarKind::F64),
    ("limits.maxTurns", &["limits", "maxTurns"], ScalarKind::U64),
    ("dispatch.enabled", &["dispatch", "enabled"], ScalarKind::Bool),
    ("compaction.provider", &["compaction", "provider"], ScalarKind::Str),
];

fn scalar_key_list() -> String {
    SCALAR_KEYS
        .iter()
        .map(|(k, _, _)| *k)
        .collect::<Vec<_>>()
        .join(", ")
}

fn parse_scalar(kind: &ScalarKind, raw: &str) -> Result<serde_json::Value, String> {
    let raw = raw.trim();
    match kind {
        ScalarKind::Str => Ok(serde_json::Value::String(raw.to_string())),
        ScalarKind::U64 => raw
            .parse::<u64>()
            .map(|n| serde_json::json!(n))
            .map_err(|_| format!("expected a non-negative integer, got '{raw}'")),
        ScalarKind::F64 => raw
            .parse::<f64>()
            .map(|n| serde_json::json!(n))
            .map_err(|_| format!("expected a number, got '{raw}'")),
        ScalarKind::Bool => raw
            .parse::<bool>()
            .map(|b| serde_json::json!(b))
            .map_err(|_| format!("expected true or false, got '{raw}'")),
    }
}

/// Navigate/create the nested object path and set the leaf value.
fn set_pointer(root: &mut serde_json::Value, path: &[&str], leaf: serde_json::Value) -> Result<(), String> {
    let mut cur = root;
    for seg in &path[..path.len() - 1] {
        let obj = cur
            .as_object_mut()
            .ok_or_else(|| format!("`{seg}` parent is not an object"))?;
        cur = obj
            .entry((*seg).to_string())
            .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    }
    let last = path[path.len() - 1];
    cur.as_object_mut()
        .ok_or_else(|| format!("`{last}` parent is not an object"))?
        .insert(last.to_string(), leaf);
    Ok(())
}

/// `stepper config set`: set one allow-listed scalar key in `setting.json`,
/// validating the merged result before writing (an invalid value — bad type,
/// unknown mode — aborts without touching the file). Reuses [`update_settings`]'
/// read-or-skeleton + atomic-write discipline, but is fallible so it can reject
/// before the write. Unknown keys list the supported set.
pub fn set_scalar(stepper_dir: &Path, key: &str, raw: &str) -> Result<(), String> {
    let Some((_, path, kind)) = SCALAR_KEYS.iter().find(|(k, _, _)| *k == key) else {
        return Err(format!("unknown key '{key}' (supported: {})", scalar_key_list()));
    };
    let leaf = parse_scalar(kind, raw)?;

    let settings_path = stepper_dir.join("setting.json");
    let mut root: serde_json::Value = match fs::read_to_string(&settings_path) {
        Ok(text) => crate::parse_setting_jsonc(&text)
            .map_err(|e| format!("{} is not valid JSON: {e}", settings_path.display()))?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            serde_json::json!({ "$schema": "stepper://setting.schema.json" })
        }
        Err(e) => return Err(e.to_string()),
    };
    if !root.is_object() {
        return Err(format!("{} is not a JSON object", settings_path.display()));
    }
    set_pointer(&mut root, path, leaf)?;

    // Validate-before-write: the merged result must parse as SettingsFile and
    // hold known enum values (so `set mode yolo` fails like `--validate`).
    let parsed: crate::settings::SettingsFile = serde_json::from_value(root.clone())
        .map_err(|e| format!("would produce an invalid setting.json: {e}"))?;
    let problems = crate::schema::validate_settings_values(&parsed);
    if !problems.is_empty() {
        return Err(problems.join("; "));
    }

    fs::create_dir_all(stepper_dir).map_err(|e| e.to_string())?;
    let body = serde_json::to_string_pretty(&root).map_err(|e| e.to_string())?;
    fs::write(&settings_path, format!("{body}\n")).map_err(|e| e.to_string())?;
    Ok(())
}

/// `stepper config get`: the current value of an allow-listed scalar key as a
/// display string (`None` = unset). Reads from the already-merged `SettingsFile`.
pub fn get_scalar(settings: &crate::settings::SettingsFile, key: &str) -> Result<Option<String>, String> {
    let Some((_, path, _)) = SCALAR_KEYS.iter().find(|(k, _, _)| *k == key) else {
        return Err(format!("unknown key '{key}' (supported: {})", scalar_key_list()));
    };
    let root = serde_json::to_value(settings).map_err(|e| e.to_string())?;
    let pointer = format!("/{}", path.join("/"));
    Ok(root.pointer(&pointer).filter(|v| !v.is_null()).map(|v| match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }))
}

/// Scaffold one `layer/<name>/index.md`. `Ok(Some(path))` when written,
/// `Ok(None)` if it already existed; the caller must pre-validate the name with
/// [`is_safe_name`].
pub fn scaffold_layer(project_root: &Path, name: &str, description: &str) -> io::Result<Option<PathBuf>> {
    let path = project_root
        .join(".stepper")
        .join("layer")
        .join(name)
        .join("index.md");
    let written = write_if_absent(&path, &layer_template(name, description))?;
    Ok(written.then_some(path))
}

/// Scaffold one `commands/<name>.md`. `Ok(None)` if it already existed.
pub fn scaffold_command(project_root: &Path, name: &str) -> io::Result<Option<PathBuf>> {
    let path = project_root
        .join(".stepper")
        .join("commands")
        .join(format!("{name}.md"));
    let written = write_if_absent(&path, &command_template(name))?;
    Ok(written.then_some(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frontmatter::{parse_command, parse_layer};

    #[test]
    fn is_safe_name_rejects_traversal_and_separators() {
        assert!(is_safe_name("plan"));
        assert!(is_safe_name("my-layer_2"));
        assert!(!is_safe_name(""));
        assert!(!is_safe_name(".."));
        assert!(!is_safe_name("a/b"));
        assert!(!is_safe_name("a.b"));
        assert!(!is_safe_name("a b"));
        assert!(!is_safe_name(&"x".repeat(65)));
    }

    #[test]
    fn templates_parse_back_as_valid_definitions() {
        // The generated layer/command files must satisfy the parsers, or the
        // scaffold would write files the loader rejects.
        let layer = parse_layer("plan", &layer_template("plan", "Plan the work.")).unwrap();
        assert_eq!(layer.frontmatter.description.as_deref(), Some("Plan the work."));
        assert!(layer.system_prompt.contains("plan"));

        let cmd = parse_command("greet", &command_template("greet")).unwrap();
        assert!(cmd.description.as_deref().unwrap().contains("greet"));
    }

    #[test]
    fn scaffold_pipeline_writes_three_layers_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let created = scaffold_default_pipeline(root).unwrap();
        assert_eq!(created.len(), 3);
        for name in ["plan", "implement", "review"] {
            let p = root.join(".stepper/layer").join(name).join("index.md");
            assert!(p.exists(), "{name} layer written");
            // each parses
            parse_layer(name, &std::fs::read_to_string(&p).unwrap()).unwrap();
        }
        // second run creates nothing (idempotent)
        assert!(scaffold_default_pipeline(root).unwrap().is_empty());
    }

    #[test]
    fn set_pipeline_steps_sets_when_empty_then_preserves_existing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".stepper")).unwrap();
        std::fs::write(
            root.join(".stepper/setting.json"),
            r#"{"$schema":"x","step":[],"mode":"auto"}"#,
        )
        .unwrap();
        assert!(set_pipeline_steps_if_empty(root, &pipeline_step_names()).unwrap());
        let after = std::fs::read_to_string(root.join(".stepper/setting.json")).unwrap();
        assert!(after.contains("plan") && after.contains("review"));
        assert!(after.contains("\"mode\""), "other keys preserved: {after}");
        // a configured pipeline is not clobbered
        assert!(!set_pipeline_steps_if_empty(root, &["other".into()]).unwrap());
    }

    #[test]
    fn runtime_writes_accept_a_jsonc_annotated_setting_file() {
        // `Config::load` accepts comments / trailing commas, so the runtime
        // read-modify-write seams must too — otherwise a user who annotated
        // `setting.json` can never persist a `/model` pick, a `config set`, or an
        // initial pipeline (the old strict `from_str` rejected the loadable file).
        let dir = tempfile::tempdir().unwrap();
        let sd = dir.path();
        let annotated = "{\n  // my settings\n  \"$schema\": \"x\",\n  \"mode\": \"auto\",\n}\n";
        std::fs::write(sd.join("setting.json"), annotated).unwrap();

        // update_settings (e.g. a /model pick) succeeds and keeps the other keys.
        update_settings(sd, |obj| {
            obj.insert("defaultModel".into(), serde_json::json!("openai/gpt-5"));
        })
        .unwrap();
        // set_scalar (config set) succeeds on the same annotated file.
        set_scalar(sd, "limits.turnTimeoutSecs", "90").unwrap();

        let raw = std::fs::read_to_string(sd.join("setting.json")).unwrap();
        let parsed: crate::settings::SettingsFile = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed.default_model.as_deref(), Some("openai/gpt-5"));
        assert_eq!(parsed.mode.as_deref(), Some("auto"), "the pre-existing key survives");
        assert_eq!(parsed.limits.unwrap().turn_timeout_secs, Some(90));

        // And set_pipeline_steps_if_empty (which takes a project root and joins
        // `.stepper/setting.json` itself) no longer clobbers an annotated file's
        // existing pipeline by misreading the un-parseable JSONC as empty.
        let proj = tempfile::tempdir().unwrap();
        let root = proj.path();
        std::fs::create_dir_all(root.join(".stepper")).unwrap();
        std::fs::write(
            root.join(".stepper/setting.json"),
            "{\n  // keep mine\n  \"step\": [\"mine\"],\n}\n",
        )
        .unwrap();
        assert!(!set_pipeline_steps_if_empty(root, &["other".into()]).unwrap());
        let after = std::fs::read_to_string(root.join(".stepper/setting.json")).unwrap();
        assert!(after.contains("mine") && !after.contains("other"), "existing pipeline preserved: {after}");
    }

    #[test]
    fn set_pipeline_steps_refuses_to_clobber_an_unparseable_setting_file() {
        // A syntax error in setting.json (not JSONC — a truly broken file) used to
        // be swallowed into the skeleton, so scaffolding overwrote the user's whole
        // config. It must now surface as an error and leave the file untouched.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".stepper")).unwrap();
        let broken = r#"{ "providers": { "x": [ }"#;
        std::fs::write(root.join(".stepper/setting.json"), broken).unwrap();
        let err = set_pipeline_steps_if_empty(root, &["a".into()]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        let after = std::fs::read_to_string(root.join(".stepper/setting.json")).unwrap();
        assert_eq!(after, broken, "the unparseable file is left exactly as-is");
    }

    #[test]
    fn scaffold_layer_and_command_are_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert!(scaffold_layer(root, "audit", "Audit the code.").unwrap().is_some());
        assert!(scaffold_layer(root, "audit", "Audit the code.").unwrap().is_none());
        assert!(scaffold_command(root, "greet").unwrap().is_some());
        assert!(scaffold_command(root, "greet").unwrap().is_none());
    }

    #[test]
    fn set_scalar_writes_typed_values_and_creates_nested_objects() {
        let dir = tempfile::tempdir().unwrap();
        let sd = dir.path();
        set_scalar(sd, "defaultModel", "openai/gpt-5").unwrap();
        set_scalar(sd, "mode", "auto").unwrap();
        set_scalar(sd, "limits.turnTimeoutSecs", "90").unwrap();
        set_scalar(sd, "limits.maxBudgetUsd", "2.5").unwrap();
        set_scalar(sd, "dispatch.enabled", "true").unwrap();
        set_scalar(sd, "compaction.provider", "anthropic").unwrap();

        let raw = std::fs::read_to_string(sd.join("setting.json")).unwrap();
        let parsed: crate::settings::SettingsFile = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed.default_model.as_deref(), Some("openai/gpt-5"));
        assert_eq!(parsed.mode.as_deref(), Some("auto"));
        let limits = parsed.limits.unwrap();
        assert_eq!(limits.turn_timeout_secs, Some(90));
        assert_eq!(limits.max_budget_usd, Some(2.5));
        assert!(parsed.dispatch.unwrap().enabled);
        assert_eq!(parsed.compaction.unwrap().provider.as_deref(), Some("anthropic"));
        // $schema skeleton is stamped on the created file.
        assert!(raw.contains("$schema"));
    }

    #[test]
    fn set_scalar_rejects_unknown_key_bad_type_and_bad_mode_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let sd = dir.path();
        let err = set_scalar(sd, "nope", "x").unwrap_err();
        assert!(err.contains("unknown key") && err.contains("defaultModel"), "lists keys: {err}");
        assert!(set_scalar(sd, "limits.turnTimeoutSecs", "soon").unwrap_err().contains("integer"));
        assert!(set_scalar(sd, "dispatch.enabled", "yes").unwrap_err().contains("true or false"));
        // an invalid mode is rejected by validate_settings_values, like --validate.
        assert!(set_scalar(sd, "mode", "yolo").unwrap_err().contains("unknown permission mode"));
        // None of the rejected sets created a file.
        assert!(!sd.join("setting.json").exists(), "no write on any rejection");
    }

    #[test]
    fn set_scalar_merges_into_existing_file_without_clobbering() {
        let dir = tempfile::tempdir().unwrap();
        let sd = dir.path();
        std::fs::create_dir_all(sd).unwrap();
        std::fs::write(sd.join("setting.json"), r#"{"mode":"plan","limits":{"maxTurns":5}}"#).unwrap();
        set_scalar(sd, "limits.turnTimeoutSecs", "30").unwrap();
        let parsed: crate::settings::SettingsFile =
            serde_json::from_str(&std::fs::read_to_string(sd.join("setting.json")).unwrap()).unwrap();
        assert_eq!(parsed.mode.as_deref(), Some("plan"), "existing scalar preserved");
        let limits = parsed.limits.unwrap();
        assert_eq!(limits.max_turns, Some(5), "existing nested key preserved");
        assert_eq!(limits.turn_timeout_secs, Some(30), "new nested key merged");
    }

    #[test]
    fn set_scalar_errors_on_unparseable_destination_not_clobber() {
        let dir = tempfile::tempdir().unwrap();
        let sd = dir.path();
        std::fs::create_dir_all(sd).unwrap();
        std::fs::write(sd.join("setting.json"), "{ not json").unwrap();
        assert!(set_scalar(sd, "mode", "auto").unwrap_err().contains("not valid JSON"));
        assert_eq!(std::fs::read_to_string(sd.join("setting.json")).unwrap(), "{ not json");
    }

    #[test]
    fn get_scalar_reads_value_or_none() {
        let mut settings = crate::settings::SettingsFile::default();
        assert_eq!(get_scalar(&settings, "mode").unwrap(), None);
        settings.mode = Some("auto".into());
        settings.default_model = Some("openai/gpt-5".into());
        assert_eq!(get_scalar(&settings, "mode").unwrap().as_deref(), Some("auto"));
        assert_eq!(get_scalar(&settings, "defaultModel").unwrap().as_deref(), Some("openai/gpt-5"));
        assert!(get_scalar(&settings, "nope").is_err());
    }
}
