use crate::usage::Usage;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Why generation stopped, normalized across dialects.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StopReason {
    /// Natural end of the assistant turn (OpenAI `stop`, Anthropic `end_turn`).
    EndTurn,
    /// The model wants tool results before continuing (OpenAI `tool_calls`,
    /// Anthropic `tool_use`). The agent loop continues after running tools.
    ToolUse,
    MaxTokens,
    StopSequence,
    Refusal,
    Other(String),
}

impl StopReason {
    pub fn wants_tools(&self) -> bool {
        matches!(self, StopReason::ToolUse)
    }
}

/// The single, dialect-agnostic item type the `StreamAccumulator` emits and the
/// core agent loop consumes. Adapters never produce this directly — they parse
/// wire frames into `WireDelta`, and the accumulator turns those into
/// `ChatEvent`s (so tool-call fragmentation is handled in exactly one place).
#[derive(Debug, Clone, PartialEq)]
pub enum ChatEvent {
    TextDelta(String),
    ThinkingDelta(String),
    /// Anthropic's `signature_delta` for the current thinking block; attached to
    /// the block by `ChatResponse::from_events` so it can be replayed in history.
    ThinkingSignature(String),
    ToolCallStarted {
        index: usize,
        id: String,
        name: String,
    },
    ToolCallArgsDelta {
        index: usize,
        fragment: String,
    },
    ToolCallCompleted {
        index: usize,
        id: String,
        name: String,
        input: Value,
    },
    Usage(Usage),
    Done(StopReason),
}
