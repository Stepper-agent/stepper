//! Anthropic Messages dialect. System prompt is top-level (not a message); tool
//! results ride as `tool_result` blocks inside a `user` message; usage arrives
//! split across `message_start` (input + cache) and `message_delta` (output).

use crate::error;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use stepper_provider::{
    ChatRequest, ContentBlock, ProviderError, Role, StopReason, ToolChoice, ToolContent, Usage,
    WireDelta,
};

const ANTHROPIC_VERSION: &str = "2023-06-01";
// Anthropic requires an explicit `max_tokens`; 4096 silently truncated long edits
// and plans. 8192 is accepted by every current Claude model (a safe floor); a
// per-model ceiling from the model registry is a follow-up.
const DEFAULT_MAX_TOKENS: u32 = 8192;

pub fn version() -> &'static str {
    ANTHROPIC_VERSION
}

pub fn build_request_body(req: &ChatRequest, model: &str, stream: bool) -> Value {
    let mut body = Map::new();
    body.insert("model".into(), json!(model));
    body.insert(
        "max_tokens".into(),
        json!(req.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS)),
    );
    body.insert("messages".into(), json!(map_messages(req)));
    body.insert("stream".into(), json!(stream));

    if let Some(sys) = &req.system {
        // With caching on, the system prompt rides as a content-block array so it
        // can carry a `cache_control` breakpoint; otherwise it stays a plain string.
        if req.cache {
            body.insert(
                "system".into(),
                json!([{ "type": "text", "text": sys, "cache_control": { "type": "ephemeral" } }]),
            );
        } else {
            body.insert("system".into(), json!(sys));
        }
    }
    if !req.tools.is_empty() {
        body.insert("tools".into(), json!(map_tools(req)));
        body.insert("tool_choice".into(), map_tool_choice(&req.tool_choice));
    }
    // Modern Claude (Opus ≥ 4.6, Sonnet ≥ 4.6, Fable/Mythos 5) drives reasoning
    // with adaptive thinking + `output_config.effort` and REMOVES the legacy
    // `thinking.budget_tokens` and sampling params (`temperature`/`top_p`), all of
    // which return a 400 on Opus 4.7+. Older Claude and non-Claude anthropic-dialect
    // proxies keep the legacy surface they already accepted.
    let adaptive = adaptive_effort_capable(model);
    if !adaptive {
        if let Some(t) = req.temperature {
            body.insert("temperature".into(), json!(t));
        }
        if let Some(p) = req.top_p {
            body.insert("top_p".into(), json!(p));
        }
    }
    if !req.stop.is_empty() {
        body.insert("stop_sequences".into(), json!(req.stop));
    }
    if adaptive {
        // An effort level (low|medium|high|xhigh|max) maps to adaptive thinking
        // plus `output_config.effort`; absent effort leaves thinking off. An
        // explicit `thinking-budget` (frontmatter) on a modern model can't send a
        // budget, so honor the "think" intent with adaptive at default depth.
        if let Some(eff) = req.reasoning_effort.as_deref().and_then(|l| anthropic_effort_value(model, l)) {
            body.insert("thinking".into(), json!({ "type": "adaptive" }));
            body.insert("output_config".into(), json!({ "effort": eff }));
        } else if req.thinking.is_some() {
            body.insert("thinking".into(), json!({ "type": "adaptive" }));
        }
    } else if let Some(thinking) = &req.thinking {
        body.insert(
            "thinking".into(),
            json!({ "type": "enabled", "budget_tokens": thinking.budget_tokens }),
        );
    }
    Value::Object(body)
}

/// Whether an Anthropic model takes adaptive thinking + `output_config.effort`
/// (Claude Opus ≥ 4.6, Sonnet ≥ 4.6, Fable/Mythos 5). On these models the legacy
/// `thinking.budget_tokens` and sampling params return a 400. Older Claude and
/// non-Claude anthropic-dialect proxies keep the legacy surface, so an unrecognized
/// model is treated as legacy (no behavior change from before this migration).
fn adaptive_effort_capable(model: &str) -> bool {
    let m = model.rsplit('/').next().unwrap_or(model).to_ascii_lowercase();
    if m.contains("fable") || m.contains("mythos") {
        return true;
    }
    for family in ["opus", "sonnet"] {
        if let Some(v) = family_version(&m, family) {
            return v >= (4, 6);
        }
    }
    false
}

/// Extract the `(major, minor)` after a `<family>-` token, e.g. `claude-opus-4-8`
/// with family `opus` → `(4, 8)`. Only 1–2 digit groups are version components, so
/// an 8-digit date suffix is NOT mistaken for the minor: `claude-opus-4-20250514`
/// (the canonical id for Opus 4.0) → `(4, 0)`, classified legacy — not `(4, 20250514)`.
/// A model with a major but no minor (`...-4-<date>`) reads as `.0`.
fn family_version(model: &str, family: &str) -> Option<(u32, u32)> {
    let rest = model.split(family).nth(1)?;
    let mut nums = rest
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty() && s.len() <= 2)
        .filter_map(|s| s.parse::<u32>().ok());
    let major = nums.next()?;
    Some((major, nums.next().unwrap_or(0)))
}

/// `xhigh` exists only on Opus ≥ 4.7 and Fable/Mythos 5.
fn supports_xhigh(model: &str) -> bool {
    let m = model.rsplit('/').next().unwrap_or(model).to_ascii_lowercase();
    m.contains("fable") || m.contains("mythos") || family_version(&m, "opus").is_some_and(|v| v >= (4, 7))
}

/// Clamp an effort level to a wire `output_config.effort` value the model supports:
/// `xhigh` falls back to `high` on models that lack it; `low|medium|high|max` pass
/// through. An unknown level yields `None` (no effort sent).
fn anthropic_effort_value(model: &str, level: &str) -> Option<&'static str> {
    match level.trim().to_ascii_lowercase().as_str() {
        "low" => Some("low"),
        "medium" | "med" => Some("medium"),
        "high" => Some("high"),
        "max" => Some("max"),
        "xhigh" => Some(if supports_xhigh(model) { "xhigh" } else { "high" }),
        _ => None,
    }
}

fn map_messages(req: &ChatRequest) -> Vec<Value> {
    let mut out = Vec::new();
    for m in &req.messages {
        match m.role {
            Role::System => {}
            Role::User => {
                // Text as one block, then any pasted images as base64 source
                // blocks (Anthropic's image content shape).
                let mut blocks: Vec<Value> = vec![json!({ "type": "text", "text": m.text() })];
                for b in &m.content {
                    if let ContentBlock::Image { media_type, data } = b {
                        blocks.push(json!({
                            "type": "image",
                            "source": { "type": "base64", "media_type": media_type, "data": data },
                        }));
                    }
                }
                out.push(json!({ "role": "user", "content": blocks }));
            }
            Role::Assistant => {
                let mut blocks = Vec::new();
                for b in &m.content {
                    match b {
                        ContentBlock::Text(t) => {
                            blocks.push(json!({ "type": "text", "text": t }))
                        }
                        // Only signed thinking blocks can be replayed — the API
                        // verifies the signature. Unsigned ones are dropped.
                        ContentBlock::Thinking {
                            text,
                            signature: Some(sig),
                        } => blocks.push(json!({
                            "type": "thinking",
                            "thinking": text,
                            "signature": sig,
                        })),
                        ContentBlock::ToolUse { id, name, input } => blocks.push(json!({
                            "type": "tool_use",
                            "id": id,
                            "name": name,
                            "input": input,
                        })),
                        _ => {}
                    }
                }
                // A turn whose every block was dropped (e.g. unsigned
                // thinking-only) must vanish — the API rejects an assistant
                // message with an empty content array.
                if !blocks.is_empty() {
                    out.push(json!({ "role": "assistant", "content": blocks }));
                }
            }
            Role::Tool => {
                let mut blocks = Vec::new();
                for b in &m.content {
                    if let ContentBlock::ToolResult {
                        tool_call_id,
                        content,
                        is_error,
                    } = b
                    {
                        blocks.push(json!({
                            "type": "tool_result",
                            "tool_use_id": tool_call_id,
                            "content": [json!({ "type": "text", "text": render_content(content) })],
                            "is_error": is_error,
                        }));
                    }
                }
                out.push(json!({ "role": "user", "content": blocks }));
            }
        }
    }
    // A second breakpoint on the last block of the last message caches the
    // accumulated conversation tail; without it the history that grows by a
    // tool_result every ReAct step is re-billed at full input price each step.
    if req.cache
        && let Some(last_block) = out
            .last_mut()
            .and_then(|m| m.get_mut("content"))
            .and_then(|c| c.as_array_mut())
            .and_then(|blocks| blocks.last_mut())
            .and_then(|b| b.as_object_mut())
    {
        last_block.insert("cache_control".into(), json!({ "type": "ephemeral" }));
    }
    out
}

fn render_content(content: &[ToolContent]) -> String {
    content
        .iter()
        .map(|c| match c {
            ToolContent::Text { text } => text.clone(),
            ToolContent::Json { json } => json.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn map_tools(req: &ChatRequest) -> Vec<Value> {
    let last = req.tools.len().saturating_sub(1);
    req.tools
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let mut tool = json!({
                "name": t.name,
                "description": t.description,
                "input_schema": t.input_schema,
            });
            // One breakpoint on the last tool caches the whole stable prefix
            // (system + tools, in Anthropic's canonical order) up to here.
            if req.cache && i == last {
                tool.as_object_mut().unwrap().insert(
                    "cache_control".into(),
                    json!({ "type": "ephemeral" }),
                );
            }
            tool
        })
        .collect()
}

fn map_tool_choice(tc: &ToolChoice) -> Value {
    match tc {
        ToolChoice::Auto => json!({ "type": "auto" }),
        ToolChoice::None => json!({ "type": "none" }),
        ToolChoice::Required => json!({ "type": "any" }),
        ToolChoice::Tool(name) => json!({ "type": "tool", "name": name }),
    }
}

#[derive(Deserialize)]
struct MessageStart {
    message: MessageStartInner,
}
#[derive(Deserialize)]
struct MessageStartInner {
    #[serde(default)]
    usage: Option<AnthropicUsage>,
}
#[derive(Deserialize, Default)]
struct AnthropicUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
}

#[derive(Deserialize)]
struct ContentBlockStart {
    index: usize,
    content_block: BlockInner,
}
#[derive(Deserialize)]
struct BlockInner {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Deserialize)]
struct ContentBlockDelta {
    index: usize,
    delta: DeltaInner,
}
#[derive(Deserialize)]
struct DeltaInner {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    partial_json: Option<String>,
    #[serde(default)]
    thinking: Option<String>,
    #[serde(default)]
    signature: Option<String>,
}

#[derive(Deserialize)]
struct ContentBlockStop {
    index: usize,
}

#[derive(Deserialize)]
struct MessageDelta {
    delta: MessageDeltaInner,
    #[serde(default)]
    usage: Option<AnthropicUsage>,
}
#[derive(Deserialize)]
struct MessageDeltaInner {
    #[serde(default)]
    stop_reason: Option<String>,
}

/// Parse one named SSE frame. `ping`/`message_stop` produce nothing; `error`
/// frames surface as `ProviderError::Api`.
pub fn parse_event(event: &str, data: &str) -> Result<Vec<WireDelta>, ProviderError> {
    match event {
        "message_start" => {
            let m: MessageStart = serde_json::from_str(data).map_err(error::decode)?;
            Ok(m.message
                .usage
                .map(|u| vec![WireDelta::Usage(to_usage(&u))])
                .unwrap_or_default())
        }
        "content_block_start" => {
            let s: ContentBlockStart = serde_json::from_str(data).map_err(error::decode)?;
            if s.content_block.kind == "tool_use" {
                Ok(vec![WireDelta::ToolCallStart {
                    index: Some(s.index),
                    id: s.content_block.id,
                    name: s.content_block.name,
                }])
            } else {
                Ok(Vec::new())
            }
        }
        "content_block_delta" => {
            let d: ContentBlockDelta = serde_json::from_str(data).map_err(error::decode)?;
            let delta = match d.delta.kind.as_str() {
                "text_delta" => d.delta.text.map(WireDelta::Text),
                "input_json_delta" => d.delta.partial_json.map(|f| WireDelta::ToolCallArgs {
                    index: Some(d.index),
                    fragment: f,
                }),
                "thinking_delta" => d.delta.thinking.map(WireDelta::Thinking),
                "signature_delta" => d.delta.signature.map(WireDelta::ThinkingSignature),
                _ => None,
            };
            Ok(delta.into_iter().collect())
        }
        "content_block_stop" => {
            let s: ContentBlockStop = serde_json::from_str(data).map_err(error::decode)?;
            Ok(vec![WireDelta::ToolCallEnd { index: s.index }])
        }
        "message_delta" => {
            let d: MessageDelta = serde_json::from_str(data).map_err(error::decode)?;
            let mut out = Vec::new();
            if let Some(u) = &d.usage {
                out.push(WireDelta::Usage(to_usage(u)));
            }
            if let Some(reason) = &d.delta.stop_reason {
                out.push(WireDelta::Stop(map_stop(reason)));
            }
            Ok(out)
        }
        "error" => {
            let v: Value = serde_json::from_str(data).map_err(error::decode)?;
            let message = v
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("anthropic stream error")
                .to_string();
            Err(ProviderError::Api {
                status: 0,
                code: v
                    .get("error")
                    .and_then(|e| e.get("type"))
                    .and_then(Value::as_str)
                    .map(String::from),
                message,
                retry_after: None,
            })
        }
        _ => Ok(Vec::new()),
    }
}

fn to_usage(u: &AnthropicUsage) -> Usage {
    Usage {
        input: u.input_tokens,
        output: u.output_tokens,
        cache_read: u.cache_read_input_tokens,
        cache_write: u.cache_creation_input_tokens,
    }
}

fn map_stop(reason: &str) -> StopReason {
    match reason {
        "end_turn" => StopReason::EndTurn,
        "tool_use" => StopReason::ToolUse,
        "max_tokens" => StopReason::MaxTokens,
        "stop_sequence" => StopReason::StopSequence,
        "refusal" => StopReason::Refusal,
        other => StopReason::Other(other.to_string()),
    }
}
