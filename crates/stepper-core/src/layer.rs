use stepper_provider::Message;

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
}

impl Handoff {
    pub fn new(user_turn: String) -> Self {
        Handoff {
            user_turn,
            prior: Vec::new(),
        }
    }

    /// The opening user message for a layer: prior summaries (if any) then the
    /// user's turn.
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
        vec![Message::user(text)]
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
