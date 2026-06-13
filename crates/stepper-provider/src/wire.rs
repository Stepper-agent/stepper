use crate::event::StopReason;
use crate::usage::Usage;

/// The neutral intermediate every dialect parser produces. A dialect's `wire`
/// module turns one raw SSE frame into zero or more `WireDelta`s; the
/// `StreamAccumulator` turns `WireDelta`s into `ChatEvent`s. This is the single
/// seam where OpenAI index-keyed string fragments and Anthropic
/// `input_json_delta` fragments become the same thing.
#[derive(Debug, Clone, PartialEq)]
pub enum WireDelta {
    Text(String),
    Thinking(String),
    /// Anthropic's `signature_delta` for the in-flight thinking block.
    ThinkingSignature(String),
    /// A tool call appears. `id`/`name` may arrive here (OpenAI first frame,
    /// Anthropic `content_block_start`) or be filled by a later frame. `index`
    /// is `None` for OpenAI-compat servers (oMLX/Ollama) that omit
    /// `tool_calls[].index` — the accumulator attributes those to the most
    /// recently started call.
    ToolCallStart {
        index: Option<usize>,
        id: Option<String>,
        name: Option<String>,
    },
    /// A raw JSON-argument fragment to append to the tool call at `index`
    /// (`None` → the most recently started call).
    ToolCallArgs {
        index: Option<usize>,
        fragment: String,
    },
    /// A tool call's arguments are complete (Anthropic `content_block_stop`,
    /// Responses `function_call_arguments.done`). OpenAI omits this — completion
    /// is inferred at `Stop`.
    ToolCallEnd {
        index: usize,
    },
    Usage(Usage),
    Stop(StopReason),
}
