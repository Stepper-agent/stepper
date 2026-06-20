use stepper_provider::{ContentBlock, Message, Role};

/// What the pipeline does when a layer fails (after exhausting `retries`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FailurePolicy {
    /// Abort the whole turn (default).
    #[default]
    Stop,
    /// Record the failure as the layer's outcome and continue to the next layer.
    Skip,
}

impl FailurePolicy {
    pub fn parse(s: Option<&str>) -> Self {
        match s {
            Some("skip") => FailurePolicy::Skip,
            _ => FailurePolicy::Stop,
        }
    }
}

/// A named sub-agent (`.stepper/agents/<name>/index.md`), invocable on demand via
/// the `task` tool or the `#<name>` prompt trigger — distinct from the `step`
/// pipeline. Parsed from the same layer frontmatter (model/tools/body=role).
#[derive(Debug, Clone)]
pub struct AgentDef {
    pub name: String,
    pub description: String,
    /// `provider/model` (or `None` to inherit the default model).
    pub model_ref: Option<String>,
    pub tool_allow: Vec<String>,
    pub tool_deny: Vec<String>,
    /// The agent's role prompt (markdown body), composed with the project context
    /// into its system prompt when it runs.
    pub role_prompt: String,
}

/// A precomputed layer in the `step` pipeline (model + prompt + tool view +
/// caps), built from `setting.json` + the layer's `index.md` frontmatter.
#[derive(Debug, Clone)]
pub struct StepDef {
    pub name: String,
    pub model_ref: String,
    pub system_prompt: String,
    pub tool_allow: Vec<String>,
    pub tool_deny: Vec<String>,
    /// MCP servers this layer may see (empty = inherit all). Scopes
    /// `mcp__<server>__*` tools.
    pub mcp_allow: Vec<String>,
    pub step_cap: usize,
    pub color: Option<String>,
    /// What to do if the layer fails after `retries` extra attempts.
    pub on_failure: FailurePolicy,
    /// Extra attempts before applying `on_failure` (0 = a single attempt).
    pub retries: usize,
    /// Sampling overrides forwarded to the provider (None = provider default).
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    /// Reasoning overrides forwarded to the provider: OpenAI-family
    /// `reasoning_effort` and Anthropic extended-thinking `thinking_budget`
    /// (None = off / provider default).
    pub reasoning_effort: Option<String>,
    pub thinking_budget: Option<u32>,
    /// Per-layer permission overrides `(rule, decision)` from frontmatter
    /// `permission:` — merged onto the base rules (tighten-only).
    pub permission: Vec<(String, String)>,
    /// Run this layer as a fan-out: when reached, the prior layer's task list
    /// (produced via the `assign_tasks` tool) spawns one worker per item, each a
    /// fresh context window running this layer's config. They run concurrently
    /// (capped by `parallel_max`) and their summaries converge into one handoff.
    pub parallel: bool,
    /// Upper bound on concurrent workers for a `parallel` layer.
    pub parallel_max: usize,
    /// Skills this layer may load (Claude-Code-style progressive disclosure):
    /// the system prompt advertises each skill's name + description, and the
    /// model loads a skill's full body on demand via the `skill` tool.
    pub skills: Vec<stepper_config::SkillDef>,
}

/// One unit of work assigned to a parallel worker — produced by the prior layer
/// via the `assign_tasks` tool and consumed by the next `parallel` layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubTask {
    pub label: String,
    pub prompt: String,
}

/// What threads between layers: the user's turn plus each prior layer's free-text
/// summary (separate context windows, so this is the only carrier).
#[derive(Debug, Clone)]
pub struct Handoff {
    pub user_turn: String,
    pub prior: Vec<(String, String)>,
    /// Images pasted with this turn (media_type, base64), attached to the opening
    /// user message so the model can see them.
    pub images: Vec<(String, String)>,
}

impl Handoff {
    pub fn new(user_turn: String, images: Vec<(String, String)>) -> Self {
        Handoff {
            user_turn,
            prior: Vec::new(),
            images,
        }
    }

    /// The opening user message for a layer: prior summaries (if any) then the
    /// user's turn, plus any pasted images.
    pub fn initial_messages(&self) -> Vec<Message> {
        let mut text = String::new();
        if !self.prior.is_empty() {
            text.push_str("# Output from prior layers\n\n");
            for (name, summary) in &self.prior {
                text.push_str(&format!("## {name}\n\n{summary}\n\n"));
            }
            text.push_str("---\n\n# Your task\n\n");
        }
        text.push_str(&self.user_turn);
        if self.images.is_empty() {
            return vec![Message::user(text)];
        }
        let mut content = vec![ContentBlock::Text(text)];
        for (media_type, data) in &self.images {
            content.push(ContentBlock::Image {
                media_type: media_type.clone(),
                data: data.clone(),
            });
        }
        vec![Message {
            role: Role::User,
            content,
        }]
    }

    /// The opening message for one parallel worker: the shared prior-layer context
    /// (so every worker sees the plan) plus this worker's assigned subtask.
    pub fn worker_messages(&self, subtask: &str) -> Vec<Message> {
        let mut text = String::new();
        if !self.prior.is_empty() {
            text.push_str("# Output from prior layers\n\n");
            for (name, summary) in &self.prior {
                text.push_str(&format!("## {name}\n\n{summary}\n\n"));
            }
            text.push_str("---\n\n");
        }
        text.push_str("# Your assigned subtask\n\n");
        text.push_str(subtask);
        vec![Message::user(text)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_messages_attach_pasted_images_to_the_user_message() {
        let h = Handoff::new(
            "what is in this screenshot?".into(),
            vec![("image/png".into(), "AAAB".into())],
        );
        let msgs = h.initial_messages();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, Role::User);
        let content = &msgs[0].content;
        assert!(
            matches!(&content[0], ContentBlock::Text(t) if t.contains("screenshot")),
            "text block first"
        );
        match &content[1] {
            ContentBlock::Image { media_type, data } => {
                assert_eq!(media_type, "image/png");
                assert_eq!(data, "AAAB");
            }
            other => panic!("expected an image block, got {other:?}"),
        }
    }

    #[test]
    fn initial_messages_without_images_stay_plain_text() {
        let h = Handoff::new("hi".into(), Vec::new());
        let msgs = h.initial_messages();
        assert_eq!(msgs[0].content.len(), 1);
        assert!(matches!(&msgs[0].content[0], ContentBlock::Text(_)));
    }
}
