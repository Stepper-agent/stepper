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
    let mut value: serde_json::Value = if path.exists() {
        serde_json::from_str(&fs::read_to_string(&path)?)
            .unwrap_or_else(|_| serde_json::json!({ "$schema": "stepper://setting.schema.json" }))
    } else {
        serde_json::json!({ "$schema": "stepper://setting.schema.json" })
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
        Ok(raw) => serde_json::from_str(&raw).map_err(|e| {
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
    fn scaffold_layer_and_command_are_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert!(scaffold_layer(root, "audit", "Audit the code.").unwrap().is_some());
        assert!(scaffold_layer(root, "audit", "Audit the code.").unwrap().is_none());
        assert!(scaffold_command(root, "greet").unwrap().is_some());
        assert!(scaffold_command(root, "greet").unwrap().is_none());
    }
}
