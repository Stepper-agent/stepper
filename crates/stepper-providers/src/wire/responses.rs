//! OpenAI Responses dialect — used by the api-key Responses path and the Codex
//! ChatGPT-OAuth backend. Named SSE events (`response.output_text.delta`,
//! `response.function_call_arguments.delta`, `response.completed`).

use crate::error;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use stepper_provider::{
    ChatRequest, ContentBlock, ProviderError, Role, StopReason, ToolChoice, ToolContent, Usage,
    WireDelta,
};

pub fn build_request_body(req: &ChatRequest, model: &str, stream: bool, store: bool) -> Value {
    let mut body = Map::new();
    body.insert("model".into(), json!(model));
    body.insert("input".into(), json!(map_input(req)));
    body.insert("stream".into(), json!(stream));
    body.insert("store".into(), json!(store));

    if let Some(sys) = &req.system {
        body.insert("instructions".into(), json!(sys));
    }
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
        body.insert("max_output_tokens".into(), json!(m));
    }
    if !req.stop.is_empty() {
        body.insert("stop".into(), json!(req.stop));
    }
    if let Some(effort) = &req.reasoning_effort {
        body.insert("reasoning".into(), json!({ "effort": effort }));
    }
    Value::Object(body)
}

fn map_input(req: &ChatRequest) -> Vec<Value> {
    let mut out = Vec::new();
    for m in &req.messages {
        match m.role {
            Role::System => {}
            Role::User => {
                let mut content: Vec<Value> =
                    vec![json!({ "type": "input_text", "text": m.text() })];
                for b in &m.content {
                    if let ContentBlock::Image { media_type, data } = b {
                        content.push(json!({
                            "type": "input_image",
                            "image_url": format!("data:{media_type};base64,{data}"),
                        }));
                    }
                }
                out.push(json!({ "type": "message", "role": "user", "content": content }));
            }
            Role::Assistant => {
                // The model emits its text before issuing tool calls, so the
                // assistant message item must precede the function_call items.
                let mut text = String::new();
                let mut calls = Vec::new();
                for b in &m.content {
                    match b {
                        ContentBlock::Text(t) => text.push_str(t),
                        ContentBlock::ToolUse { id, name, input } => calls.push(json!({
                            "type": "function_call",
                            "call_id": id,
                            "name": name,
                            "arguments": serde_json::to_string(input).unwrap_or_default(),
                        })),
                        _ => {}
                    }
                }
                if !text.is_empty() {
                    out.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [json!({ "type": "output_text", "text": text })],
                    }));
                }
                out.extend(calls);
            }
            Role::Tool => {
                for b in &m.content {
                    if let ContentBlock::ToolResult {
                        tool_call_id,
                        content,
                        ..
                    } = b
                    {
                        out.push(json!({
                            "type": "function_call_output",
                            "call_id": tool_call_id,
                            "output": render_content(content),
                        }));
                    }
                }
            }
        }
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
    req.tools
        .iter()
        .map(|t| {
            json!({
                "type": "function",
                "name": t.name,
                "description": t.description,
                "parameters": t.input_schema,
            })
        })
        .collect()
}

fn map_tool_choice(tc: &ToolChoice) -> Value {
    match tc {
        ToolChoice::Auto => json!("auto"),
        ToolChoice::None => json!("none"),
        ToolChoice::Required => json!("required"),
        ToolChoice::Tool(name) => json!({ "type": "function", "name": name }),
    }
}

#[derive(Deserialize)]
struct Delta {
    #[serde(default)]
    delta: Option<String>,
}

#[derive(Deserialize)]
struct OutputItemAdded {
    output_index: usize,
    item: OutputItem,
}
#[derive(Deserialize)]
struct OutputItem {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    call_id: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Deserialize)]
struct FnArgsDelta {
    output_index: usize,
    #[serde(default)]
    delta: Option<String>,
}
#[derive(Deserialize)]
struct FnArgsDone {
    output_index: usize,
}

#[derive(Deserialize)]
struct Completed {
    response: CompletedResponse,
}
#[derive(Deserialize)]
struct CompletedResponse {
    #[serde(default)]
    usage: Option<ResponsesUsage>,
    #[serde(default)]
    output: Vec<OutputItem>,
}
#[derive(Deserialize, Default)]
struct ResponsesUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    input_tokens_details: Option<InputDetails>,
}
#[derive(Deserialize, Default)]
struct InputDetails {
    #[serde(default)]
    cached_tokens: u64,
}

/// Parse one named Responses SSE frame.
pub fn parse_event(event: &str, data: &str) -> Result<Vec<WireDelta>, ProviderError> {
    match event {
        "response.output_text.delta" => {
            let d: Delta = serde_json::from_str(data).map_err(error::decode)?;
            Ok(d.delta.map(WireDelta::Text).into_iter().collect())
        }
        "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
            let d: Delta = serde_json::from_str(data).map_err(error::decode)?;
            Ok(d.delta.map(WireDelta::Thinking).into_iter().collect())
        }
        "response.output_item.added" => {
            let a: OutputItemAdded = serde_json::from_str(data).map_err(error::decode)?;
            if a.item.kind == "function_call" {
                Ok(vec![WireDelta::ToolCallStart {
                    index: Some(a.output_index),
                    id: a.item.call_id.or(a.item.id),
                    name: a.item.name,
                }])
            } else {
                Ok(Vec::new())
            }
        }
        "response.function_call_arguments.delta" => {
            let d: FnArgsDelta = serde_json::from_str(data).map_err(error::decode)?;
            Ok(d.delta
                .map(|f| WireDelta::ToolCallArgs {
                    index: Some(d.output_index),
                    fragment: f,
                })
                .into_iter()
                .collect())
        }
        "response.function_call_arguments.done" => {
            let d: FnArgsDone = serde_json::from_str(data).map_err(error::decode)?;
            Ok(vec![WireDelta::ToolCallEnd {
                index: d.output_index,
            }])
        }
        "response.completed" => {
            let c: Completed = serde_json::from_str(data).map_err(error::decode)?;
            let mut out = Vec::new();
            if let Some(u) = &c.response.usage {
                // `input_tokens` includes the cached portion; subtract it so
                // `input` stays the uncached prompt only (disjoint from cache_read).
                let cached = u
                    .input_tokens_details
                    .as_ref()
                    .map(|d| d.cached_tokens)
                    .unwrap_or(0);
                out.push(WireDelta::Usage(Usage {
                    input: u.input_tokens.saturating_sub(cached),
                    output: u.output_tokens,
                    cache_read: cached,
                    cache_write: 0,
                }));
            }
            let used_tools = c
                .response
                .output
                .iter()
                .any(|item| item.kind == "function_call");
            out.push(WireDelta::Stop(if used_tools {
                StopReason::ToolUse
            } else {
                StopReason::EndTurn
            }));
            Ok(out)
        }
        "response.incomplete" => {
            // The output-token cap truncated the response. Map it to MaxTokens (with
            // usage) so the agent issues a bounded continuation, instead of letting
            // the missing terminal event surface as a retryable UnexpectedEnd that
            // re-sends the same over-long request 3× at full cost before failing.
            let v: Value = serde_json::from_str(data).map_err(error::decode)?;
            let mut out = Vec::new();
            if let Some(u) = v.pointer("/response/usage") {
                let input = u.get("input_tokens").and_then(Value::as_u64).unwrap_or(0);
                let cached = u
                    .pointer("/input_tokens_details/cached_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                out.push(WireDelta::Usage(Usage {
                    input: input.saturating_sub(cached),
                    output: u.get("output_tokens").and_then(Value::as_u64).unwrap_or(0),
                    cache_read: cached,
                    cache_write: 0,
                }));
            }
            let reason = v.pointer("/response/incomplete_details/reason").and_then(Value::as_str);
            out.push(WireDelta::Stop(if reason == Some("max_output_tokens") {
                StopReason::MaxTokens
            } else {
                StopReason::EndTurn
            }));
            Ok(out)
        }
        "response.failed" | "error" => {
            let v: Value = serde_json::from_str(data).map_err(error::decode)?;
            let message = v
                .pointer("/response/error/message")
                .or_else(|| v.pointer("/error/message"))
                .or_else(|| v.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("responses stream error")
                .to_string();
            // Carry the error's own code/type (NOT the top-level event "type") so a
            // transient in-band error (server_error / rate_limit) is retryable
            // despite the status-less frame.
            let code = v
                .pointer("/response/error/code")
                .or_else(|| v.pointer("/response/error/type"))
                .or_else(|| v.pointer("/error/code"))
                .or_else(|| v.pointer("/error/type"))
                .and_then(Value::as_str)
                .map(String::from);
            Err(ProviderError::Api {
                status: 0,
                code,
                message,
                retry_after: None,
            })
        }
        _ => Ok(Vec::new()),
    }
}
