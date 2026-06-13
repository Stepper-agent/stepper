use crate::message::Message;
use crate::tool::ToolSpec;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub enum ToolChoice {
    /// Model decides whether to call a tool.
    #[default]
    Auto,
    /// Model must not call a tool this turn.
    None,
    /// Model must call some tool.
    Required,
    /// Model must call this specific tool.
    Tool(String),
}

/// Extended-thinking budget. Anthropic maps it to the top-level
/// `thinking: { type: "enabled", budget_tokens }` config; OpenAI dialects use
/// `reasoning_effort` instead and ignore this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThinkingConfig {
    pub budget_tokens: u32,
}

/// A provider-agnostic chat request. `system` is hoisted out of `messages` so
/// the Anthropic adapter can place it in the top-level `system` field and the
/// Responses/Codex adapter can place it in `instructions`, while the OpenAI
/// adapter re-injects it as a leading `role:system` message.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChatRequest {
    pub model: String,
    pub system: Option<String>,
    pub messages: Vec<Message>,
    #[serde(default)]
    pub tools: Vec<ToolSpec>,
    #[serde(default)]
    pub tool_choice: ToolChoice,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    #[serde(default)]
    pub stop: Vec<String>,
    /// Request extended thinking (Anthropic budget). Additive: absent in older
    /// serialized requests, `None` keeps thinking off.
    #[serde(default)]
    pub thinking: Option<ThinkingConfig>,
    /// OpenAI-family reasoning effort (`low`/`medium`/`high`). Chat Completions
    /// maps it to `reasoning_effort`, Responses to `reasoning.effort`; Anthropic
    /// ignores it.
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    /// Request prompt caching of the stable prefix (system + tools). Anthropic
    /// marks them `cache_control: ephemeral` so repeated requests (the ReAct loop
    /// re-sends the same prefix every step) hit cache-read pricing; OpenAI /
    /// Responses cache by prefix automatically and ignore this.
    #[serde(default)]
    pub cache: bool,
}

impl ChatRequest {
    pub fn new(model: impl Into<String>) -> Self {
        ChatRequest {
            model: model.into(),
            ..Default::default()
        }
    }

    pub fn with_system(mut self, system: impl Into<String>) -> Self {
        self.system = Some(system.into());
        self
    }

    pub fn with_messages(mut self, messages: Vec<Message>) -> Self {
        self.messages = messages;
        self
    }

    pub fn with_tools(mut self, tools: Vec<ToolSpec>) -> Self {
        self.tools = tools;
        self
    }
}
