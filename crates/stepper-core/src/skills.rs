//! The model-callable `skill` tool — Claude-Code-style progressive disclosure.
//!
//! A layer advertises its skills (name + description) in the system prompt; the
//! model calls `skill { name }` to load a skill's full body only when it's
//! relevant, keeping the base context lean. The tool is scoped per layer: it only
//! serves the skills that layer declared (`skills:` frontmatter), and it performs
//! no side effects (it returns text), so it needs no permission gate.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use stepper_config::SkillDef;
use stepper_tools::{Tool, ToolCx};
use stepper_provider::{ToolError, ToolResult, ToolSpec};

pub struct SkillTool {
    spec: ToolSpec,
    skills: Vec<SkillDef>,
}

#[derive(Deserialize)]
struct Args {
    name: String,
}

impl SkillTool {
    pub fn new(skills: Vec<SkillDef>) -> Self {
        SkillTool {
            spec: ToolSpec {
                name: "skill".into(),
                description: "Load the full instructions for one of this layer's available skills \
                              (progressive disclosure). Call with {\"name\": \"<skill>\"} when a \
                              listed skill is relevant, then follow the returned instructions."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": { "name": {"type": "string"} },
                    "required": ["name"]
                }),
                read_only: true,
                parallel_safe: true,
            },
            skills,
        }
    }
}

#[async_trait]
impl Tool for SkillTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn call(&self, args: Value, _cx: &ToolCx) -> Result<ToolResult, ToolError> {
        let a: Args =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;
        match self.skills.iter().find(|s| s.name == a.name) {
            Some(skill) => Ok(ToolResult::text(skill.body.trim().to_string())),
            None => {
                let available: Vec<&str> = self.skills.iter().map(|s| s.name.as_str()).collect();
                Err(ToolError::InvalidArgs(format!(
                    "unknown skill '{}'; available: {}",
                    a.name,
                    available.join(", ")
                )))
            }
        }
    }
}

/// The system-prompt advertisement for a layer's skills: names + descriptions,
/// plus how to load one. Empty string when the layer has no skills.
pub fn advertise(skills: &[SkillDef]) -> String {
    if skills.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "\n\n# Available skills\n\nCall the `skill` tool with a skill `name` to load its full \
         instructions before using it.\n",
    );
    for s in skills {
        out.push_str(&format!("\n- {}: {}", s.name, s.description.trim()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use stepper_permission::{Decision, PermissionMode, RuleSet};
    use stepper_tools::{Approval, Approver};
    use tokio_util::sync::CancellationToken;

    struct NoApprover;
    #[async_trait]
    impl Approver for NoApprover {
        async fn request(&self, _: Approval) -> Decision {
            Decision::Deny
        }
    }

    fn skill(name: &str, desc: &str, body: &str) -> SkillDef {
        SkillDef {
            name: name.into(),
            description: desc.into(),
            allowed_tools: Vec::new(),
            model: None,
            body: body.into(),
        }
    }

    fn cx() -> ToolCx {
        ToolCx {
            cwd: ".".into(),
            project_root: ".".into(),
            home: None,
            mode: PermissionMode::Auto,
            live_mode: None,
            rules: Arc::new(RuleSet::default()),
            approver: Arc::new(NoApprover),
            cancel: CancellationToken::new(),
            sandbox_writable_roots: None,
        }
    }

    #[tokio::test]
    async fn loads_a_declared_skill_body_and_rejects_unknown_names() {
        let tool = SkillTool::new(vec![skill("rust-style", "Rust conventions", "Run clippy.")]);
        let out = tool
            .call(json!({ "name": "rust-style" }), &cx())
            .await
            .expect("declared skill loads");
        assert!(out.content_text().contains("Run clippy."));

        let err = tool.call(json!({ "name": "ghost" }), &cx()).await;
        assert!(err.is_err(), "an undeclared skill must be refused");
    }

    #[test]
    fn advertise_lists_names_and_descriptions_but_not_bodies() {
        let ad = advertise(&[skill("a", "does A", "SECRET BODY")]);
        assert!(ad.contains("# Available skills"));
        assert!(ad.contains("a: does A"));
        assert!(!ad.contains("SECRET BODY"));
        assert!(advertise(&[]).is_empty());
    }
}
