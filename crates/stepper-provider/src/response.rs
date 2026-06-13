use crate::event::{ChatEvent, StopReason};
use crate::message::ContentBlock;
use crate::usage::Usage;

/// The folded result of a streaming turn, used by the non-streaming default
/// `LlmProvider::chat`. Carries the assistant content blocks (text + completed
/// tool calls), the stop reason, and the merged usage.
#[derive(Debug, Clone)]
pub struct ChatResponse {
    pub content: Vec<ContentBlock>,
    pub stop_reason: StopReason,
    pub usage: Usage,
}

impl ChatResponse {
    /// Reassemble a response from the post-accumulator `ChatEvent` stream.
    /// Adjacent `TextDelta`s coalesce into a single `Text` block, preserving the
    /// relative order of text and tool-use blocks as the model emitted them.
    pub fn from_events(events: impl IntoIterator<Item = ChatEvent>) -> Self {
        let mut content: Vec<ContentBlock> = Vec::new();
        let mut usage = Usage::default();
        let mut stop_reason = StopReason::EndTurn;

        for ev in events {
            match ev {
                ChatEvent::TextDelta(t) => match content.last_mut() {
                    Some(ContentBlock::Text(buf)) => buf.push_str(&t),
                    _ => content.push(ContentBlock::Text(t)),
                },
                ChatEvent::ThinkingDelta(t) => match content.last_mut() {
                    Some(ContentBlock::Thinking { text, .. }) => text.push_str(&t),
                    _ => content.push(ContentBlock::Thinking {
                        text: t,
                        signature: None,
                    }),
                },
                ChatEvent::ThinkingSignature(s) => match content.last_mut() {
                    Some(ContentBlock::Thinking { signature, .. }) => {
                        signature.get_or_insert_with(String::new).push_str(&s)
                    }
                    _ => content.push(ContentBlock::Thinking {
                        text: String::new(),
                        signature: Some(s),
                    }),
                },
                ChatEvent::ToolCallCompleted {
                    id, name, input, ..
                } => content.push(ContentBlock::ToolUse { id, name, input }),
                ChatEvent::Usage(u) => usage.merge(&u),
                ChatEvent::Done(reason) => stop_reason = reason,
                ChatEvent::ToolCallStarted { .. } | ChatEvent::ToolCallArgsDelta { .. } => {}
            }
        }

        ChatResponse {
            content,
            stop_reason,
            usage,
        }
    }

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

    pub fn tool_uses(&self) -> impl Iterator<Item = (&str, &str, &serde_json::Value)> {
        self.content.iter().filter_map(|b| match b {
            ContentBlock::ToolUse { id, name, input } => {
                Some((id.as_str(), name.as_str(), input))
            }
            _ => None,
        })
    }
}
