//! `/create-layer` — have the model author a pipeline layer and place it.
//!
//! The built-in composes one prompt through the same expanded-command turn path
//! `/code-review` uses: an embedded layer reference (the docs' layer guide, so
//! the model needs no network fetch), a snapshot of the effective pipeline
//! config (user AND project `setting.json` — the project file deep-merges over
//! the user file with array replacement, so the model must see both), and the
//! user's request. The model then writes `<root>/.stepper/layer/<name>/index.md`
//! and inserts the name into the `step` array at the requested position using
//! its ordinary tools (every write stays permission-gated). All instructed
//! paths are absolute: tool-relative paths resolve against the session cwd,
//! which may be a subdirectory of the project root.

use std::path::Path;

/// `setting.json` bodies larger than this are elided from the prompt (the model
/// reads the file itself instead) so a huge config cannot blow the window.
const MAX_SETTINGS_EMBED_BYTES: usize = 8_192;

/// The distilled layer reference from the docs (stepper.gumyo.net/docs/layers),
/// embedded so `/create-layer` works offline and never drifts from the binary.
const LAYER_GUIDE: &str = r#"Layers are the step agents of the stepper pipeline. They run in the order
given by the "step" array in `.stepper/setting.json`; each layer runs with its
own provider/model and a fresh context window, and only its free-text summary
is handed to the next layer (never the full transcript or tool output).

A layer named `<name>` lives at `.stepper/layer/<name>/index.md`: a YAML
frontmatter block (configuration) followed by the layer's system prompt body.

```markdown
---
description: implementation layer        # required
model: omlx/deepseek-coder               # optional override (provider/model-id)
temperature: 0.2                         # optional sampling overrides
top_p: 0.9
reasoning-effort: high                   # low|medium|high|xhigh|max — omit to disable
tools:
  allow: [read_file, write_file, edit_file, bash]
  deny:  [web_fetch]
permission:                              # per-layer overrides (tighten-only)
  Bash(rm *): deny
  Write(**): ask
mcp:
  allow: [context7]                      # which MCP servers this layer sees
skills: [rust-style]                     # skill bodies injected on demand
steps: 40                                # ReAct iteration cap
on-failure: skip                         # stop (default) | skip
retries: 1                               # extra attempts before on-failure
color: green                             # optional TUI label color
parallel: true                           # fan out: one worker per subtask
parallel-max: 4                          # concurrency cap (tasks queue, none dropped)
---
You are the implementation layer. Carry out the plan using the tools.
```

Rules:
- `description` is required; every other field is optional.
- Layer names use lowercase letters, digits, `-` or `_` (max 64 chars).
- `reasoning-effort` accepts `low|medium|high|xhigh|max`; there is no `off` —
  omit the field to leave reasoning disabled for the layer.
- Per-layer `permission` rules merge onto the global ones and can only
  TIGHTEN (`deny > ask > allow`) — a layer can never relax a global deny.
- A `parallel: true` layer runs one worker per subtask assigned by the layer
  DIRECTLY BEFORE it, which is offered the `assign_tasks` tool
  (`assign_tasks({ tasks: [{label, prompt}, …] })`). When adding a parallel
  layer, make sure the preceding layer's prompt tells it to split the work; if
  nothing is assigned, the parallel layer runs once without task context.
- `setting.json` is JSONC (comments allowed). The effective config is the USER
  file (`~/.stepper/setting.json`) deep-merged with the PROJECT file
  (`<root>/.stepper/setting.json`) on top — objects merge, but **arrays
  replace**: a project `"step"` array completely overrides the user one."#;

/// Build the `/create-layer` prompt, or a user-facing refusal (`Err`) when the
/// request is empty. Does light project IO (reads both `setting.json` files,
/// lists layer dirs), so call it from `spawn_blocking` like the other prompt
/// builders.
pub fn create_layer_prompt(
    args: &str,
    project_root: &Path,
    home: Option<&Path>,
) -> Result<String, String> {
    let request = args.trim();
    if request.is_empty() {
        return Err(
            "usage: /create-layer <describe the layer and where it belongs> — e.g. \
             /create-layer a security-review layer after implement"
                .to_string(),
        );
    }

    let project_settings_path = project_root.join(".stepper").join("setting.json");
    let project_settings = settings_snapshot(&project_settings_path, "does not exist yet — create it");
    let user_settings = match home {
        Some(home) => settings_snapshot(
            &home.join(".stepper").join("setting.json"),
            "does not exist (no user-level pipeline to preserve)",
        ),
        None => "(no home directory — no user-level config)".to_string(),
    };
    let project_layers = layer_list(&project_root.join(".stepper").join("layer"));
    let user_layers = match home {
        Some(home) => layer_list(&home.join(".stepper").join("layer")),
        None => "(none)".to_string(),
    };
    let root = project_root.display();

    Ok(format!(
        "Create a stepper pipeline layer in this project.\n\n\
         ## Request\n{request}\n\n\
         ## Layer reference\n{LAYER_GUIDE}\n\n\
         ## Current project state\n\
         Project root: `{root}` (use ABSOLUTE paths below — the session cwd may be a subdirectory).\n\n\
         Project `{root}/.stepper/setting.json`: {project_settings}\n\n\
         User `~/.stepper/setting.json` (the base the project file merges over): {user_settings}\n\n\
         Project layer files (`{root}/.stepper/layer/*/index.md`): {project_layers}\n\
         User layer files (`~/.stepper/layer/*/index.md`): {user_layers}\n\n\
         ## Do this\n\
         1. From the request, choose the layer name (lowercase letters, digits, `-`, `_`) and its position in the pipeline (default: appended at the end of the effective `step` order).\n\
         2. Write `{root}/.stepper/layer/<name>/index.md`: the YAML frontmatter (at minimum `description`) plus a focused system prompt body for the layer's role.\n\
         3. Edit `{root}/.stepper/setting.json` so its \"step\" array holds the layer at the requested position. Because the project array REPLACES the user array, write the FULL effective order — if the current pipeline comes from the user config, copy that order and insert the new layer into it, never a one-element array. Create the project file if it is missing; preserve every other setting, the existing order, and any JSONC comments; change nothing unrelated.\n\
         4. If the layer is `parallel: true`, also make sure the preceding layer exists and its prompt tells it to split the work with `assign_tasks`.\n\
         5. Re-read both files to verify them, and run `stepper config --validate` if the `stepper` binary is on PATH.\n\
         6. Report the final `step` order and remind the user that layers load at startup, so the new layer takes effect on the next stepper launch.\n"
    ))
}

/// The embeddable snapshot of one `setting.json`: fenced body, a size-elision
/// note, or `missing_note` when the file is absent/unreadable.
fn settings_snapshot(path: &Path, missing_note: &str) -> String {
    match std::fs::read_to_string(path) {
        Ok(body) if body.len() <= MAX_SETTINGS_EMBED_BYTES => {
            format!("\n```jsonc\n{}\n```", body.trim_end())
        }
        Ok(body) => format!("exists but is too large to embed ({} bytes) — read it yourself first.", body.len()),
        Err(_) => missing_note.to_string(),
    }
}

/// The names under a `.stepper/layer/` dir that actually hold an `index.md`,
/// sorted, or `(none)`.
fn layer_list(dir: &Path) -> String {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return "(none)".to_string();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter(|e| e.path().join("index.md").is_file())
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    if names.is_empty() {
        return "(none)".to_string();
    }
    names.sort();
    names.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn an_empty_request_is_refused_with_usage() {
        let tmp = tempfile::tempdir().unwrap();
        let err = create_layer_prompt("   ", tmp.path(), None).unwrap_err();
        assert!(err.contains("usage: /create-layer"));
    }

    #[test]
    fn the_prompt_embeds_the_guide_the_request_and_absolute_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let prompt =
            create_layer_prompt("add a review layer after implement", tmp.path(), None).unwrap();
        assert!(prompt.contains("## Layer reference"));
        assert!(prompt.contains("description: implementation layer"));
        assert!(prompt.contains("add a review layer after implement"));
        assert!(prompt.contains("does not exist yet"));
        // Instructions use absolute paths (cwd may be a project subdirectory).
        let abs_layer = format!("{}/.stepper/layer/<name>/index.md", tmp.path().display());
        assert!(prompt.contains(&abs_layer));
        assert!(prompt.contains("stepper config --validate"));
        // No `off` effort value is suggested (invalid in layer frontmatter).
        assert!(prompt.contains("low|medium|high|xhigh|max"));
        assert!(!prompt.contains("off|low"));
    }

    #[test]
    fn the_prompt_embeds_project_and_user_settings_and_layer_names() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let stepper = tmp.path().join(".stepper");
        fs::create_dir_all(stepper.join("layer/plan")).unwrap();
        fs::write(stepper.join("layer/plan/index.md"), "---\ndescription: p\n---\nbody").unwrap();
        // A directory without an index.md is not a layer and must not be listed.
        fs::create_dir_all(stepper.join("layer/empty")).unwrap();
        fs::write(stepper.join("setting.json"), "{\n  \"step\": [\"plan\"]\n}").unwrap();
        let user = home.path().join(".stepper");
        fs::create_dir_all(user.join("layer/review")).unwrap();
        fs::write(user.join("layer/review/index.md"), "---\ndescription: r\n---\nbody").unwrap();
        fs::write(user.join("setting.json"), "{\n  \"step\": [\"plan\", \"review\"]\n}").unwrap();

        let prompt =
            create_layer_prompt("insert x before plan", tmp.path(), Some(home.path())).unwrap();
        assert!(prompt.contains("\"step\": [\"plan\"]"));
        assert!(prompt.contains("\"step\": [\"plan\", \"review\"]"), "user base config is visible");
        assert!(prompt.contains("arrays\n  replace") || prompt.contains("REPLACES the user array"));
        assert!(prompt.contains("Project layer files"));
        assert!(prompt.contains("plan"));
        assert!(prompt.contains("review"));
        assert!(!prompt.contains("empty,"));
    }

    #[test]
    fn an_oversized_settings_file_is_elided_not_embedded() {
        let tmp = tempfile::tempdir().unwrap();
        let stepper = tmp.path().join(".stepper");
        fs::create_dir_all(&stepper).unwrap();
        let big = format!("{{\n  \"step\": []\n  // {}\n}}", "x".repeat(MAX_SETTINGS_EMBED_BYTES));
        fs::write(stepper.join("setting.json"), &big).unwrap();
        let prompt = create_layer_prompt("add a layer", tmp.path(), None).unwrap();
        assert!(prompt.contains("too large to embed"));
        assert!(!prompt.contains(&big));
    }
}
