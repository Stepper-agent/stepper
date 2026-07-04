//! YAML-frontmatter parsing for the authored `.stepper/` document types:
//! layer `index.md`, skill `SKILL.md`, command `<name>.md`, and output style
//! `output-styles/<name>.md`. The body after the frontmatter is the document's
//! prompt/template.

use crate::error::ConfigError;
use gray_matter::engine::YAML;
use gray_matter::Matter;
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ToolFilter {
    pub allow: Vec<String>,
    pub deny: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct McpAllow {
    pub allow: Vec<String>,
}

/// Parsed `layer/<name>/index.md` frontmatter (the layer's config overlay).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct LayerFrontmatter {
    pub description: Option<String>,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    /// OpenAI-family reasoning effort: `minimal` | `low` | `medium` | `high`.
    #[serde(alias = "reasoning-effort")]
    pub reasoning_effort: Option<String>,
    /// Anthropic extended-thinking budget in tokens (maps to `thinking`).
    #[serde(alias = "thinking-budget")]
    pub thinking_budget: Option<u32>,
    pub tools: ToolFilter,
    pub permission: BTreeMap<String, String>,
    pub mcp: McpAllow,
    pub skills: Vec<String>,
    pub steps: Option<u32>,
    pub hidden: bool,
    pub color: Option<String>,
    /// `stop` (default) | `skip` — what the pipeline does if this layer fails.
    #[serde(alias = "on-failure")]
    pub on_failure: Option<String>,
    /// Extra attempts before applying `on_failure`.
    pub retries: u32,
    /// Run this layer as a fan-out: when reached, the prior layer's task list
    /// (`assign_tasks`) spawns one parallel worker per item. Off by default.
    pub parallel: bool,
    /// Upper bound on concurrent workers for a `parallel` layer (the prior task
    /// list is capped to this). `None` = the built-in default.
    #[serde(alias = "parallel-max")]
    pub parallel_max: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct LayerDef {
    pub name: String,
    pub frontmatter: LayerFrontmatter,
    /// The body after the frontmatter — this layer's system prompt (never
    /// concatenated with the orchestrator's).
    pub system_prompt: String,
}

#[derive(Debug, Clone)]
pub struct SkillDef {
    pub name: String,
    pub description: String,
    pub allowed_tools: Vec<String>,
    pub model: Option<String>,
    pub body: String,
}

#[derive(Debug, Clone)]
pub struct CommandDef {
    pub name: String,
    pub description: Option<String>,
    pub argument_hint: Option<String>,
    pub arguments: Vec<String>,
    pub allowed_tools: Vec<String>,
    pub model: Option<String>,
    pub disable_model_invocation: bool,
    pub template: String,
}

/// `description` is required (it is both the human label and the routing signal).
pub fn parse_layer(name: &str, content: &str) -> Result<LayerDef, ConfigError> {
    let (data, body) = split_frontmatter(content, &format!("layer/{name}"))?;
    let frontmatter: LayerFrontmatter = serde_json::from_value(data).map_err(|e| {
        ConfigError::Frontmatter {
            which: format!("layer/{name}"),
            message: e.to_string(),
        }
    })?;
    if frontmatter
        .description
        .as_deref()
        .unwrap_or("")
        .trim()
        .is_empty()
    {
        return Err(ConfigError::Frontmatter {
            which: format!("layer/{name}"),
            message: "missing required `description`".into(),
        });
    }
    Ok(LayerDef {
        name: name.to_string(),
        frontmatter,
        system_prompt: body,
    })
}

pub fn parse_skill(content: &str) -> Result<SkillDef, ConfigError> {
    let (data, body) = split_frontmatter(content, "skill")?;
    let name = data
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| frontmatter_err("skill", "missing required `name`"))?;
    validate_skill_name(&name)?;

    let description = data
        .get("description")
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|d| !d.trim().is_empty())
        .ok_or_else(|| frontmatter_err("skill", "missing required `description`"))?;
    if description.chars().count() > 1024 {
        return Err(frontmatter_err("skill", "`description` exceeds 1024 chars"));
    }

    Ok(SkillDef {
        name,
        description,
        allowed_tools: string_or_list(data.get("allowed-tools")),
        model: data
            .get("stepper-model")
            .and_then(Value::as_str)
            .map(str::to_string),
        body,
    })
}

/// An output style from `.stepper/output-styles/<name>.md` — the body replaces
/// (or augments) the layer system prompt when the style is selected.
#[derive(Debug, Clone)]
pub struct OutputStyleDef {
    pub name: String,
    pub description: Option<String>,
    pub body: String,
}

/// `name` falls back to the file stem; `description` is optional; the body must
/// be non-empty (an empty style would silently blank the system prompt).
pub fn parse_output_style(stem: &str, content: &str) -> Result<OutputStyleDef, ConfigError> {
    let (data, body) = split_frontmatter(content, "output-style")?;
    let name = data
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .unwrap_or(stem)
        .to_string();
    if body.trim().is_empty() {
        return Err(frontmatter_err(
            &format!("output-style/{stem}"),
            "missing body (the style's prompt text)",
        ));
    }
    Ok(OutputStyleDef {
        name,
        description: data
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_string),
        body,
    })
}

/// A path-scoped rule file (`.stepper/rules/*.md`): an optional `paths:`
/// directory-scope list (relative to the project root) plus the rule body. With
/// no `paths` the rule always applies; otherwise it loads only when the working
/// directory is one of the scopes or beneath it. Each entry is a directory — a
/// bare dir, `dir/`, `dir/*`, or `dir/**` (all "this dir and below"); a non-
/// trailing wildcard (`**/x`, `*.rs`) is not a supported scope.
#[derive(Debug, Clone)]
pub struct RuleDef {
    pub paths: Vec<String>,
    pub body: String,
}

/// Parse a `.stepper/rules/*.md` file into its `paths` scope + body.
pub fn parse_rule(content: &str) -> Result<RuleDef, ConfigError> {
    let (data, body) = split_frontmatter(content, "rule")?;
    Ok(RuleDef {
        paths: comma_list(data.get("paths")),
        body,
    })
}

/// Like [`string_or_list`] but a scalar string splits ONLY on commas — a path
/// scope can legitimately contain spaces (`paths: my dir`), so whitespace must
/// not fragment it into separate (mis-scoped) globs.
fn comma_list(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::String(s)) => s
            .split(',')
            .map(str::trim)
            .filter(|x| !x.is_empty())
            .map(str::to_string)
            .collect(),
        _ => string_or_list(value),
    }
}

pub fn parse_command(name: &str, content: &str) -> Result<CommandDef, ConfigError> {
    let (data, body) = split_frontmatter(content, "command")?;
    Ok(CommandDef {
        name: name.to_string(),
        description: data
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_string),
        argument_hint: data
            .get("argument-hint")
            .and_then(Value::as_str)
            .map(str::to_string),
        arguments: string_or_list(data.get("arguments")),
        allowed_tools: string_or_list(data.get("allowed-tools")),
        model: data.get("model").and_then(Value::as_str).map(str::to_string),
        disable_model_invocation: data
            .get("disable-model-invocation")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        template: body,
    })
}

fn split_frontmatter(content: &str, which: &str) -> Result<(Value, String), ConfigError> {
    let matter = Matter::<YAML>::new();
    let entity = matter.parse(content);
    let data = match entity.data {
        // A present-but-Null block is malformed (or empty) YAML. gray_matter
        // swallows the parse error into `Null`; surface it instead of silently
        // dropping every declared field (which would, e.g., strip a layer's
        // tool/permission restrictions or a command's `allowed-tools`).
        Some(pod) => {
            let value: Value = pod.into();
            if value.is_null() {
                return Err(frontmatter_err(which, "malformed YAML frontmatter"));
            }
            value
        }
        None => Value::Object(Default::default()),
    };
    Ok((data, entity.content))
}

fn validate_skill_name(name: &str) -> Result<(), ConfigError> {
    let lower = name.to_ascii_lowercase();
    let charset_ok = !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if !charset_ok {
        return Err(frontmatter_err(
            "skill",
            &format!("invalid name '{name}' (<=64 chars, [a-z0-9-])"),
        ));
    }
    if lower.contains("claude") || lower.contains("anthropic") {
        return Err(frontmatter_err(
            "skill",
            &format!("skill name '{name}' must not contain reserved words"),
        ));
    }
    Ok(())
}

/// `allowed-tools` / `arguments` accept either a string or a YAML list. A string
/// splits on commas when present (so specifiers like `Bash(git *)` keep their
/// inner spaces), otherwise on whitespace.
fn string_or_list(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::String(s)) => {
            let by_comma = s.contains(',');
            s.split(|c: char| if by_comma { c == ',' } else { c.is_whitespace() })
                .map(str::trim)
                .filter(|x| !x.is_empty())
                .map(str::to_string)
                .collect()
        }
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

fn frontmatter_err(which: &str, message: &str) -> ConfigError {
    ConfigError::Frontmatter {
        which: which.to_string(),
        message: message.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rule_paths_split_only_on_commas_so_a_path_may_contain_spaces() {
        // A single path with a space must stay one scope (not two mis-scoped globs).
        let one = parse_rule("---\npaths: my feature\n---\nbody").unwrap();
        assert_eq!(one.paths, vec!["my feature"]);
        // A comma-separated list still splits, trimming each.
        let many = parse_rule("---\npaths: src/**, docs/x\n---\nbody").unwrap();
        assert_eq!(many.paths, vec!["src/**", "docs/x"]);
        // A YAML list works too.
        let list = parse_rule("---\npaths:\n  - a\n  - b c\n---\nbody").unwrap();
        assert_eq!(list.paths, vec!["a", "b c"]);
    }

    #[test]
    fn parses_layer_frontmatter_and_body() {
        let doc = "---\n\
description: implementation layer\n\
model: omlx/deepseek-coder-v2\n\
temperature: 0.2\n\
tools:\n  allow: [read_file, edit_file, bash]\n  deny: [web_fetch]\n\
steps: 40\n\
color: green\n\
---\n\
You are the implement layer.\n";
        let layer = parse_layer("implement", doc).unwrap();
        assert_eq!(layer.frontmatter.model.as_deref(), Some("omlx/deepseek-coder-v2"));
        assert_eq!(layer.frontmatter.tools.allow, vec!["read_file", "edit_file", "bash"]);
        assert_eq!(layer.frontmatter.tools.deny, vec!["web_fetch"]);
        assert_eq!(layer.frontmatter.steps, Some(40));
        assert!(layer.system_prompt.contains("implement layer"));
    }

    #[test]
    fn layer_requires_description() {
        let doc = "---\nmodel: x/y\n---\nbody";
        assert!(matches!(
            parse_layer("plan", doc),
            Err(ConfigError::Frontmatter { .. })
        ));
    }

    #[test]
    fn parses_on_failure_and_retries() {
        let doc = "---\ndescription: d\non-failure: skip\nretries: 2\n---\nbody";
        let layer = parse_layer("build", doc).unwrap();
        assert_eq!(layer.frontmatter.on_failure.as_deref(), Some("skip"));
        assert_eq!(layer.frontmatter.retries, 2);
    }

    #[test]
    fn on_failure_and_retries_default_when_absent() {
        let doc = "---\ndescription: d\n---\nbody";
        let layer = parse_layer("build", doc).unwrap();
        assert_eq!(layer.frontmatter.on_failure, None);
        assert_eq!(layer.frontmatter.retries, 0);
    }

    #[test]
    fn skill_name_validation_rejects_reserved_and_charset() {
        let bad_word = "---\nname: claude-helper\ndescription: x\n---\nb";
        assert!(parse_skill(bad_word).is_err());
        let bad_char = "---\nname: My_Skill\ndescription: x\n---\nb";
        assert!(parse_skill(bad_char).is_err());
        let ok = "---\nname: rust-conventions\ndescription: Rust style.\nallowed-tools: Read Grep\n---\nbody";
        let skill = parse_skill(ok).unwrap();
        assert_eq!(skill.name, "rust-conventions");
        assert_eq!(skill.allowed_tools, vec!["Read", "Grep"]);
    }

    #[test]
    fn command_parses_named_args_and_flags() {
        let doc = "---\n\
description: summarize diff\n\
argument-hint: \"[path]\"\n\
arguments: [path]\n\
allowed-tools: Bash(git *), Read\n\
disable-model-invocation: true\n\
---\n\
Review {arg:path}.\n";
        let cmd = parse_command("review", doc).unwrap();
        assert_eq!(cmd.arguments, vec!["path"]);
        assert_eq!(cmd.allowed_tools, vec!["Bash(git *)", "Read"]);
        assert!(cmd.disable_model_invocation);
        assert!(cmd.template.contains("Review"));
    }

    #[test]
    fn skill_allowed_tools_splits_on_comma_when_present() {
        let doc = "---\nname: rust-style\ndescription: x\nallowed-tools: Bash(git status), Read, Grep\n---\nbody";
        let skill = parse_skill(doc).unwrap();
        assert_eq!(skill.allowed_tools, vec!["Bash(git status)", "Read", "Grep"]);
    }

    #[test]
    fn skill_allowed_tools_accepts_yaml_list() {
        let doc = "---\nname: rust-style\ndescription: x\nallowed-tools:\n  - Read\n  - Grep\n---\nbody";
        let skill = parse_skill(doc).unwrap();
        assert_eq!(skill.allowed_tools, vec!["Read", "Grep"]);
    }

    #[test]
    fn skill_missing_name_is_rejected() {
        let doc = "---\ndescription: only a description\n---\nbody";
        assert!(matches!(
            parse_skill(doc),
            Err(ConfigError::Frontmatter { .. })
        ));
    }

    #[test]
    fn skill_missing_or_blank_description_is_rejected() {
        let missing = "---\nname: rust-style\n---\nbody";
        assert!(parse_skill(missing).is_err());
        let blank = "---\nname: rust-style\ndescription: \"   \"\n---\nbody";
        assert!(parse_skill(blank).is_err());
    }

    #[test]
    fn skill_name_over_64_chars_is_rejected() {
        let long = "a".repeat(65);
        let doc = format!("---\nname: {long}\ndescription: x\n---\nbody");
        assert!(parse_skill(&doc).is_err());
        let ok = "b".repeat(64);
        let doc_ok = format!("---\nname: {ok}\ndescription: x\n---\nbody");
        assert_eq!(parse_skill(&doc_ok).unwrap().name, ok);
    }

    #[test]
    fn skill_description_over_1024_chars_is_rejected() {
        let desc = "x".repeat(1025);
        let doc = format!("---\nname: rust-style\ndescription: {desc}\n---\nbody");
        assert!(matches!(
            parse_skill(&doc),
            Err(ConfigError::Frontmatter { which, message })
                if which == "skill" && message.contains("1024")
        ));
    }

    #[test]
    fn skill_name_rejects_anthropic_reserved_word() {
        let doc = "---\nname: anthropic-tools\ndescription: x\n---\nbody";
        assert!(parse_skill(doc).is_err());
    }

    #[test]
    fn skill_carries_model_and_body() {
        let doc = "---\nname: rust-style\ndescription: x\nstepper-model: omlx/deepseek\n---\nThe skill body.\n";
        let skill = parse_skill(doc).unwrap();
        assert_eq!(skill.model.as_deref(), Some("omlx/deepseek"));
        assert!(skill.body.contains("The skill body."));
    }

    #[test]
    fn command_allowed_tools_splits_on_whitespace_without_comma() {
        let doc = "---\nallowed-tools: Read Grep Edit\n---\nbody";
        let cmd = parse_command("x", doc).unwrap();
        assert_eq!(cmd.allowed_tools, vec!["Read", "Grep", "Edit"]);
    }

    #[test]
    fn malformed_frontmatter_is_an_error_not_silently_dropped() {
        // A broken YAML block must error rather than parse as "no frontmatter"
        // (which would silently strip a command's allowed-tools or a layer's
        // tool/permission restrictions) — uniformly across all document types.
        assert!(parse_command("c", "---\nallowed-tools: [Read\n---\nbody").is_err());
        assert!(parse_skill("---\nname: [oops\n---\nb").is_err());
        assert!(parse_layer("l", "---\nkey: \"unterminated\n---\nbody").is_err());
    }

    #[test]
    fn command_defaults_when_frontmatter_absent() {
        let cmd = parse_command("bare", "just a template body with no frontmatter").unwrap();
        assert_eq!(cmd.description, None);
        assert_eq!(cmd.argument_hint, None);
        assert!(cmd.arguments.is_empty());
        assert!(cmd.allowed_tools.is_empty());
        assert_eq!(cmd.model, None);
        assert!(!cmd.disable_model_invocation);
        assert!(cmd.template.contains("template body"));
    }

    #[test]
    fn layer_parses_mcp_permission_skills_and_hidden_default() {
        let doc = "---\n\
description: planning layer\n\
mcp:\n  allow: [server-a, server-b]\n\
permission:\n  bash: ask\n  edit_file: allow\n\
skills: [rust-style]\n\
top_p: 0.9\n\
---\n\
Plan body.\n";
        let layer = parse_layer("plan", doc).unwrap();
        assert_eq!(layer.frontmatter.mcp.allow, vec!["server-a", "server-b"]);
        assert_eq!(
            layer.frontmatter.permission.get("bash").map(String::as_str),
            Some("ask")
        );
        assert_eq!(layer.frontmatter.skills, vec!["rust-style"]);
        assert_eq!(layer.frontmatter.top_p, Some(0.9));
        assert!(!layer.frontmatter.hidden);
        assert_eq!(layer.name, "plan");
    }

    #[test]
    fn parses_reasoning_overrides() {
        let doc = "---\n\
description: deep thinker\n\
reasoning-effort: high\n\
thinking-budget: 8000\n\
---\nThink hard.\n";
        let layer = parse_layer("plan", doc).unwrap();
        assert_eq!(layer.frontmatter.reasoning_effort.as_deref(), Some("high"));
        assert_eq!(layer.frontmatter.thinking_budget, Some(8000));
    }

    #[test]
    fn reasoning_overrides_default_to_none() {
        let layer = parse_layer("plan", "---\ndescription: d\n---\nbody").unwrap();
        assert!(layer.frontmatter.reasoning_effort.is_none());
        assert!(layer.frontmatter.thinking_budget.is_none());
    }

    #[test]
    fn parses_parallel_layer_fields() {
        let doc = "---\n\
description: parallel implement layer\n\
parallel: true\n\
parallel-max: 4\n\
---\n\
Work on your assigned subtask.\n";
        let layer = parse_layer("implement", doc).unwrap();
        assert!(layer.frontmatter.parallel);
        assert_eq!(layer.frontmatter.parallel_max, Some(4));
    }

    #[test]
    fn parallel_defaults_to_false_when_absent() {
        let doc = "---\ndescription: d\n---\nbody";
        let layer = parse_layer("plan", doc).unwrap();
        assert!(!layer.frontmatter.parallel);
        assert_eq!(layer.frontmatter.parallel_max, None);
    }

    #[test]
    fn layer_blank_description_is_rejected() {
        let doc = "---\ndescription: \"   \"\nmodel: x/y\n---\nbody";
        assert!(matches!(
            parse_layer("plan", doc),
            Err(ConfigError::Frontmatter { .. })
        ));
    }

    #[test]
    fn parses_output_style_with_frontmatter_name_and_description() {
        let doc = "---\nname: Explanatory\ndescription: teaches while coding\n---\nExplain every change.\n";
        let style = parse_output_style("explanatory", doc).unwrap();
        assert_eq!(style.name, "Explanatory");
        assert_eq!(style.description.as_deref(), Some("teaches while coding"));
        assert_eq!(style.body.trim(), "Explain every change.");
    }

    #[test]
    fn output_style_name_falls_back_to_file_stem() {
        let style = parse_output_style("terse", "Be terse.\n").unwrap();
        assert_eq!(style.name, "terse");
        assert_eq!(style.description, None);
        assert_eq!(style.body.trim(), "Be terse.");
    }

    #[test]
    fn output_style_blank_frontmatter_name_falls_back_to_stem() {
        let doc = "---\nname: \"  \"\n---\nbody text";
        let style = parse_output_style("fallback", doc).unwrap();
        assert_eq!(style.name, "fallback");
    }

    #[test]
    fn output_style_without_body_is_rejected() {
        let doc = "---\nname: empty\ndescription: d\n---\n   \n";
        assert!(matches!(
            parse_output_style("empty", doc),
            Err(ConfigError::Frontmatter { .. })
        ));
    }
}
