//! OpenAI Chat Completions dialect — shared by OpenAI, Ollama Cloud (`/v1`), and
//! oMLX (`localhost/v1`). Tool-call arguments stream as string fragments keyed by
//! `tool_calls[].index`.

use crate::error;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use stepper_provider::{
    ChatRequest, ContentBlock, ProviderError, Role, StopReason, ToolChoice, ToolContent, WireDelta,
};

pub fn build_request_body(req: &ChatRequest, model: &str, stream: bool) -> Value {
    let mut body = Map::new();
    body.insert("model".into(), json!(model));
    body.insert("messages".into(), json!(map_messages(req)));
    body.insert("stream".into(), json!(stream));

    if !req.tools.is_empty() {
        body.insert("tools".into(), json!(map_tools(req)));
        body.insert("tool_choice".into(), map_tool_choice(&req.tool_choice));
    }
    if let Some(t) = req.temperature {
        body.insert("temperature".into(), json!(t));
    }
    if let Some(p) = req.top_p {
        body.insert("top_p".into(), json!(p));
    }
    if let Some(m) = req.max_tokens {
        body.insert("max_tokens".into(), json!(m));
    }
    if !req.stop.is_empty() {
        body.insert("stop".into(), json!(req.stop));
    }
    if let Some(effort) = &req.reasoning_effort {
        // OpenAI `reasoning_effort` tops out at `high`; `xhigh`/`max` are
        // Anthropic-only levels, so clamp them down rather than 400.
        let effort = clamp_openai_effort(effort);
        body.insert("reasoning_effort".into(), json!(effort));
    }
    if stream {
        body.insert("stream_options".into(), json!({ "include_usage": true }));
    }
    Value::Object(body)
}

/// Map a canonical effort level to an OpenAI `reasoning_effort` value. OpenAI
/// supports `minimal|low|medium|high`; the Anthropic-only `xhigh`/`max` clamp to
/// `high`, and any other value passes through unchanged.
pub(crate) fn clamp_openai_effort(level: &str) -> &str {
    match level {
        "xhigh" | "max" => "high",
        other => other,
    }
}

fn map_messages(req: &ChatRequest) -> Vec<Value> {
    let mut out = Vec::new();

    // Hoist all system text into one leading system message — OpenAI expects
    // system before other messages, so inline `Role::System` entries must not
    // land out of order.
    let mut system = req.system.clone().unwrap_or_default();
    for m in &req.messages {
        if m.role == Role::System {
            let t = m.text();
            if !t.is_empty() {
                if !system.is_empty() {
                    system.push('\n');
                }
                system.push_str(&t);
            }
        }
    }
    if !system.is_empty() {
        out.push(json!({ "role": "system", "content": system }));
    }

    for m in &req.messages {
        match m.role {
            Role::System => {}
            Role::User => {
                // Plain string when text-only (back-compat); the array form with
                // `image_url` data: URLs only when an image is attached.
                let images: Vec<Value> = m
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Image { media_type, data } => Some(json!({
                            "type": "image_url",
                            "image_url": { "url": format!("data:{media_type};base64,{data}") },
                        })),
                        _ => None,
                    })
                    .collect();
                if images.is_empty() {
                    out.push(json!({ "role": "user", "content": m.text() }));
                } else {
                    let mut parts = vec![json!({ "type": "text", "text": m.text() })];
                    parts.extend(images);
                    out.push(json!({ "role": "user", "content": parts }));
                }
            }
            Role::Assistant => {
                let mut text = String::new();
                let mut tool_calls = Vec::new();
                for b in &m.content {
                    match b {
                        ContentBlock::Text(t) => text.push_str(t),
                        ContentBlock::ToolUse { id, name, input } => tool_calls.push(json!({
                            "id": id,
                            "type": "function",
                            "function": {
                                "name": name,
                                "arguments": serde_json::to_string(input).unwrap_or_default(),
                            }
                        })),
                        _ => {}
                    }
                }
                // Thinking blocks are dropped on this dialect; a turn left with
                // neither text nor tool calls must not serialize at all.
                if text.is_empty() && tool_calls.is_empty() {
                    continue;
                }
                let mut msg = Map::new();
                msg.insert("role".into(), json!("assistant"));
                msg.insert(
                    "content".into(),
                    if text.is_empty() {
                        Value::Null
                    } else {
                        json!(text)
                    },
                );
                if !tool_calls.is_empty() {
                    msg.insert("tool_calls".into(), json!(tool_calls));
                }
                out.push(Value::Object(msg));
            }
            Role::Tool => {
                for b in &m.content {
                    if let ContentBlock::ToolResult {
                        tool_call_id,
                        content,
                        is_error,
                    } = b
                    {
                        out.push(json!({
                            "role": "tool",
                            "tool_call_id": tool_call_id,
                            "content": render_content(content, *is_error),
                        }));
                    }
                }
            }
        }
    }
    out
}

fn render_content(content: &[ToolContent], is_error: bool) -> String {
    let body = content
        .iter()
        .map(|c| match c {
            ToolContent::Text { text } => text.clone(),
            ToolContent::Json { json } => json.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n");
    if is_error {
        format!("Error: {body}")
    } else {
        body
    }
}

fn map_tools(req: &ChatRequest) -> Vec<Value> {
    req.tools
        .iter()
        .map(|t| {
            json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.input_schema,
                }
            })
        })
        .collect()
}

fn map_tool_choice(tc: &ToolChoice) -> Value {
    match tc {
        ToolChoice::Auto => json!("auto"),
        ToolChoice::None => json!("none"),
        ToolChoice::Required => json!("required"),
        ToolChoice::Tool(name) => json!({ "type": "function", "function": { "name": name } }),
    }
}

#[derive(Deserialize)]
struct Chunk {
    #[serde(default)]
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<OpenAiUsage>,
}

#[derive(Deserialize)]
struct Choice {
    #[serde(default)]
    delta: Delta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize, Default)]
struct Delta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    reasoning: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ToolCallDelta>,
}

#[derive(Deserialize)]
struct ToolCallDelta {
    /// Optional on purpose: oMLX/Ollama-style servers omit `index`, and a
    /// defaulted 0 would corrupt parallel calls — the accumulator attributes
    /// `None` to the most recently started call instead.
    #[serde(default)]
    index: Option<usize>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<FunctionDelta>,
}

#[derive(Deserialize, Default)]
struct FunctionDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Deserialize)]
struct OpenAiUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
    #[serde(default)]
    prompt_tokens_details: Option<PromptDetails>,
}

#[derive(Deserialize, Default)]
struct PromptDetails {
    #[serde(default)]
    cached_tokens: u64,
}

/// Parse one `data:` payload (already stripped of the `data: ` prefix and never
/// the `[DONE]` sentinel — the adapter filters that).
pub fn parse_chunk(data: &str) -> Result<Vec<WireDelta>, ProviderError> {
    // A 200-OK stream can still carry an in-band error frame (`{"error":{...}}`)
    // instead of choices — common on the OpenAI-compatible path (ollama-cloud,
    // local oMLX) for context-length-exceeded / rate-limit / bad-request. Without
    // this, the frame deserializes to zero choices and is silently dropped, and
    // the turn dies as a generic `UnexpectedEnd` instead of the real message.
    if let Ok(v) = serde_json::from_str::<Value>(data)
        && v.get("error").is_some()
    {
        return Err(error::api_error_from_body(0, data));
    }
    let chunk: Chunk = serde_json::from_str(data).map_err(error::decode)?;
    let mut out = Vec::new();

    for choice in &chunk.choices {
        if let Some(t) = &choice.delta.content {
            out.push(WireDelta::Text(t.clone()));
        }
        if let Some(t) = choice
            .delta
            .reasoning_content
            .as_ref()
            .or(choice.delta.reasoning.as_ref())
        {
            out.push(WireDelta::Thinking(t.clone()));
        }
        for tc in &choice.delta.tool_calls {
            let name = tc.function.as_ref().and_then(|f| f.name.clone());
            if tc.id.is_some() || name.is_some() {
                out.push(WireDelta::ToolCallStart {
                    index: tc.index,
                    id: tc.id.clone(),
                    name,
                });
            }
            if let Some(args) = tc.function.as_ref().and_then(|f| f.arguments.clone())
                && !args.is_empty()
            {
                out.push(WireDelta::ToolCallArgs {
                    index: tc.index,
                    fragment: args,
                });
            }
        }
        if let Some(reason) = &choice.finish_reason {
            out.push(WireDelta::Stop(map_stop(reason)));
        }
    }

    if let Some(u) = &chunk.usage {
        // OpenAI's `prompt_tokens` already includes the cached portion; subtract
        // it so `input` stays the uncached prompt only (the `Usage` invariant:
        // input / cache_read / cache_write are disjoint, so cost prices each once
        // and context measurement doesn't double-count the cached prefix).
        let cached = u
            .prompt_tokens_details
            .as_ref()
            .map(|d| d.cached_tokens)
            .unwrap_or(0);
        out.push(WireDelta::Usage(stepper_provider::Usage {
            input: u.prompt_tokens.saturating_sub(cached),
            output: u.completion_tokens,
            cache_read: cached,
            cache_write: 0,
        }));
    }

    Ok(out)
}

fn map_stop(reason: &str) -> StopReason {
    match reason {
        "stop" => StopReason::EndTurn,
        "tool_calls" | "function_call" => StopReason::ToolUse,
        "length" => StopReason::MaxTokens,
        "content_filter" => StopReason::Refusal,
        other => StopReason::Other(other.to_string()),
    }
}
