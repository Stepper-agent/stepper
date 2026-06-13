//! End-to-end regression for IMP-24 through the real OpenAI-compat adapter: a
//! server that omits `tool_calls[].index` (the oMLX/Ollama pattern) no longer
//! collapses parallel calls into slot 0, and a single index-less call still
//! assembles. No network — wiremock serves canned SSE bodies.

use futures::StreamExt;
use serde_json::json;
use stepper_providers::{
    AuthSource, ChatEvent, ChatRequest, LlmProvider, Message, OpenAiCompatAdapter, StopReason,
};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn sse(body: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(body.as_bytes().to_vec(), "text/event-stream")
}

fn req() -> ChatRequest {
    ChatRequest::new("test-model").with_messages(vec![Message::user("hi")])
}

async fn collect_completed(server: &MockServer) -> Vec<(String, String, serde_json::Value)> {
    let adapter = OpenAiCompatAdapter::new(
        reqwest::Client::new(),
        "omlx",
        format!("{}/v1", server.uri()),
        "test-model",
        AuthSource::None,
    );
    let mut stream = adapter
        .chat_stream(req(), CancellationToken::new())
        .await
        .expect("stream opened");
    let mut completed = Vec::new();
    let mut done = false;
    while let Some(item) = stream.next().await {
        match item.expect("no stream error") {
            ChatEvent::ToolCallCompleted {
                id, name, input, ..
            } => completed.push((id, name, input)),
            ChatEvent::Done(StopReason::ToolUse) => done = true,
            _ => {}
        }
    }
    assert!(done, "the turn terminates with ToolUse");
    completed
}

#[tokio::test]
async fn parallel_index_less_tool_calls_no_longer_corrupt_each_other() {
    // Two complete calls, no `index` anywhere. With the old `#[serde(default)]
    // index = 0` both landed in slot 0 and the arguments concatenated into junk.
    let body = "\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"id\":\"call_a\",\"type\":\"function\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\\\"a.rs\\\"}\"}}]},\"index\":0}]}\n\n\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"id\":\"call_b\",\"type\":\"function\",\"function\":{\"name\":\"list_dir\",\"arguments\":\"{\\\"path\\\":\\\"/src\\\"}\"}}]},\"index\":0}]}\n\n\
data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
data: [DONE]\n\n";
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(sse(body))
        .mount(&server)
        .await;

    let mut completed = collect_completed(&server).await;
    completed.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(completed.len(), 2, "two distinct calls survive");
    assert_eq!(
        completed[0],
        (
            "call_a".to_string(),
            "read_file".to_string(),
            json!({"path": "a.rs"})
        )
    );
    assert_eq!(
        completed[1],
        (
            "call_b".to_string(),
            "list_dir".to_string(),
            json!({"path": "/src"})
        )
    );
}

#[tokio::test]
async fn a_single_index_less_call_with_fragmented_arguments_assembles() {
    // First frame carries id+name, later frames only argument fragments — all
    // without `index`. The fragments attach to the most recently started call.
    let body = "\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"get_weather\",\"arguments\":\"\"}}]},\"index\":0}]}\n\n\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"function\":{\"arguments\":\"{\\\"city\\\":\"}}]},\"index\":0}]}\n\n\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"function\":{\"arguments\":\"\\\"seoul\\\"}\"}}]},\"index\":0}]}\n\n\
data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
data: [DONE]\n\n";
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(sse(body))
        .mount(&server)
        .await;

    let completed = collect_completed(&server).await;
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].0, "call_1");
    assert_eq!(completed[0].1, "get_weather");
    assert_eq!(completed[0].2, json!({"city": "seoul"}));
}
