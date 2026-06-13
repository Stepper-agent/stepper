use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

/// A tool advertised to the model. `input_schema` is a raw JSON Schema object so
/// both native tools (schemars) and MCP tools (rmcp) forward 1:1 with no
/// reshaping — each adapter wraps it in its own dialect envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub parallel_safe: bool,
}

impl ToolSpec {
    /// Provider tool-name constraint shared by OpenAI and Anthropic:
    /// `^[a-zA-Z0-9_-]{1,64}$`.
    pub fn name_is_valid(name: &str) -> bool {
        !name.is_empty()
            && name.len() <= 64
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    }
}

/// One piece of a tool's output. Folds into `ContentBlock::ToolResult` on the
/// way back to the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolContent {
    Text { text: String },
    Json { json: Value },
}

impl ToolContent {
    pub fn text(s: impl Into<String>) -> Self {
        ToolContent::Text { text: s.into() }
    }
}

/// What a tool produces. `is_error` maps to Anthropic's native `is_error` flag
/// and to an error-prefixed `role:tool` message for OpenAI.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolResult {
    pub content: Vec<ToolContent>,
    #[serde(default)]
    pub is_error: bool,
    #[serde(default)]
    pub truncated: bool,
}

impl ToolResult {
    pub fn text(s: impl Into<String>) -> Self {
        ToolResult {
            content: vec![ToolContent::text(s)],
            is_error: false,
            truncated: false,
        }
    }

    /// Flatten the content to a single string (JSON parts stringified).
    pub fn content_text(&self) -> String {
        self.content
            .iter()
            .map(|c| match c {
                ToolContent::Text { text } => text.clone(),
                ToolContent::Json { json } => json.to_string(),
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn error(s: impl Into<String>) -> Self {
        ToolResult {
            content: vec![ToolContent::text(s)],
            is_error: true,
            truncated: false,
        }
    }
}

#[derive(Debug, Error)]
pub enum ToolError {
    #[error("tool not found: {0}")]
    NotFound(String),

    #[error("invalid arguments: {0}")]
    InvalidArgs(String),

    #[error("denied by permission policy: {0}")]
    Denied(String),

    #[error("execution failed: {0}")]
    Execution(String),
}
