use crate::tool::ToolContent;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// One unit of conversation content. The same set covers every dialect; each
/// adapter projects it into its wire envelope (OpenAI `role:tool` message,
/// Anthropic `tool_result` user block, Responses `function_call_output` item).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ContentBlock {
    Text(String),
    /// Model reasoning. `signature` is Anthropic's integrity token
    /// (`signature_delta`); only signed thinking blocks can be replayed in
    /// history, so encoders drop unsigned ones.
    Thinking {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    /// An assistant-issued tool call (already fully accumulated).
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    /// The result of a previously issued tool call, threaded back to the model.
    ToolResult {
        tool_call_id: String,
        content: Vec<ToolContent>,
        #[serde(default)]
        is_error: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

impl Message {
    pub fn user(text: impl Into<String>) -> Self {
        Message {
            role: Role::User,
            content: vec![ContentBlock::Text(text.into())],
        }
    }

    pub fn assistant(text: impl Into<String>) -> Self {
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text(text.into())],
        }
    }

    /// Convenience accessor for the concatenated plain text of this message.
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }
}
