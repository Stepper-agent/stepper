//! End-to-end adapter tests: a wiremock server serves canned `text/event-stream`
//! bodies, and we assert the adapter normalizes them into the right `ChatEvent`
//! sequence — no real credentials, no network egress.

use futures::StreamExt;
use stepper_providers::{
    AnthropicAdapter, AuthSource, ChatEvent, ChatRequest, Message, OpenAiCompatAdapter,
    OpenAiResponsesAdapter, StopReason,
};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn sse(body: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(body.as_bytes().to_vec(), "text/event-stream")
}

async fn collect(
    stream: stepper_provider::ChatStream,
) -> Vec<ChatEvent> {
    let mut stream = stream;
    let mut out = Vec::new();
    while let Some(item) = stream.next().await {
        out.push(item.expect("no stream error"));
    }
    out
}

fn req() -> ChatRequest {
    ChatRequest::new("test-model").with_messages(vec![Message::user("hi")])
}

#[tokio::test]
async fn openai_compat_streams_text_and_usage() {
    let server = MockServer::start().await;
    let body = "\
data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"\"},\"index\":0}]}\n\n\
data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"},\"index\":0}]}\n\n\
data: {\"choices\":[{\"delta\":{\"content\":\" world\"},\"index\":0}]}\n\n\
data: {\"choices\":[{\"delta\":{},\"index\":0,\"finish_reason\":\"stop\"}]}\n\n\
data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5}}\n\n\
data: [DONE]\n\n";
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(sse(body))
        .mount(&server)
        .await;

    let adapter = OpenAiCompatAdapter::new(
        reqwest::Client::new(),
        "openai",
        format!("{}/v1", server.uri()),
        "test-model",
        AuthSource::None,
    );
    let events = collect(adapter_stream(&adapter, req()).await).await;

    let text: String = events
        .iter()
        .filter_map(|e| match e {
            ChatEvent::TextDelta(t) => Some(t.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "Hello world");
    assert!(events
        .iter()
        .any(|e| matches!(e, ChatEvent::Done(StopReason::EndTurn))));
    assert!(events.iter().any(|e| matches!(
        e,
        ChatEvent::Usage(u) if u.input == 10 && u.output == 5
    )));
}

#[tokio::test]
async fn openai_compat_excludes_cached_tokens_from_input() {
    // OpenAI reports cached tokens as a SUBSET of prompt_tokens; the adapter must
    // split them so `input` is the uncached prompt only (prompt 100, cached 80 =>
    // input 20, cache_read 80) — otherwise cached tokens are double-counted in
    // both cost and the context gauge.
    let server = MockServer::start().await;
    let body = "\
data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"index\":0}]}\n\n\
data: {\"choices\":[{\"delta\":{},\"index\":0,\"finish_reason\":\"stop\"}]}\n\n\
data: {\"choices\":[],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":5,\"prompt_tokens_details\":{\"cached_tokens\":80}}}\n\n\
data: [DONE]\n\n";
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(sse(body))
        .mount(&server)
        .await;

    let adapter = OpenAiCompatAdapter::new(
        reqwest::Client::new(),
        "openai",
        format!("{}/v1", server.uri()),
        "test-model",
        AuthSource::None,
    );
    let events = collect(adapter_stream(&adapter, req()).await).await;
    assert!(
        events.iter().any(|e| matches!(
            e,
            ChatEvent::Usage(u) if u.input == 20 && u.cache_read == 80 && u.output == 5
        )),
        "input is the uncached remainder, cache_read holds the cached subset: {events:#?}"
    );
}

#[tokio::test]
async fn openai_compat_accumulates_fragmented_tool_call() {
    let server = MockServer::start().await;
    let body = "\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"get_weather\",\"arguments\":\"\"}}]},\"index\":0}]}\n\n\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"city\\\":\"}}]},\"index\":0}]}\n\n\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"seoul\\\"}\"}}]},\"index\":0}]}\n\n\
data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
data: [DONE]\n\n";
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(sse(body))
        .mount(&server)
        .await;

    let adapter = OpenAiCompatAdapter::new(
        reqwest::Client::new(),
        "openai",
        format!("{}/v1", server.uri()),
        "test-model",
        AuthSource::None,
    );
    let events = collect(adapter_stream(&adapter, req()).await).await;

    let completed = events
        .iter()
        .find_map(|e| match e {
            ChatEvent::ToolCallCompleted { name, input, .. } => Some((name.clone(), input.clone())),
            _ => None,
        })
        .expect("tool call completed");
    assert_eq!(completed.0, "get_weather");
    assert_eq!(completed.1, serde_json::json!({"city": "seoul"}));
    assert!(events
        .iter()
        .any(|e| matches!(e, ChatEvent::Done(StopReason::ToolUse))));
}

#[tokio::test]
async fn anthropic_streams_text_and_split_usage() {
    let server = MockServer::start().await;
    let body = "\
event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":12,\"cache_read_input_tokens\":4}}}\n\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":7}}\n\n\
event: message_stop\n\
data: {\"type\":\"message_stop\"}\n\n";
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "secret"))
        .respond_with(sse(body))
        .mount(&server)
        .await;

    let adapter = AnthropicAdapter::new(
        reqwest::Client::new(),
        "anthropic",
        server.uri(),
        "claude-x",
        AuthSource::ApiKey("secret".into()),
    );
    let events = collect(adapter_stream(&adapter, req()).await).await;

    assert!(events
        .iter()
        .any(|e| matches!(e, ChatEvent::TextDelta(t) if t == "Hi")));
    assert!(events
        .iter()
        .any(|e| matches!(e, ChatEvent::Done(StopReason::EndTurn))));
    let final_usage = events
        .iter()
        .filter_map(|e| match e {
            ChatEvent::Usage(u) => Some(*u),
            _ => None,
        })
        .next_back()
        .expect("usage");
    assert_eq!(final_usage.input, 12);
    assert_eq!(final_usage.output, 7);
    assert_eq!(final_usage.cache_read, 4);
}

#[tokio::test]
async fn responses_streams_tool_call() {
    let server = MockServer::start().await;
    let body = "\
event: response.output_item.added\n\
data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"call_9\",\"name\":\"do_it\"}}\n\n\
event: response.function_call_arguments.delta\n\
data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\"{\\\"k\\\":1}\"}\n\n\
event: response.function_call_arguments.done\n\
data: {\"type\":\"response.function_call_arguments.done\",\"output_index\":0}\n\n\
event: response.completed\n\
data: {\"type\":\"response.completed\",\"response\":{\"output\":[{\"type\":\"function_call\"}],\"usage\":{\"input_tokens\":3,\"output_tokens\":2}}}\n\n";
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(sse(body))
        .mount(&server)
        .await;

    let adapter = OpenAiResponsesAdapter::new(
        reqwest::Client::new(),
        "openai",
        format!("{}/v1", server.uri()),
        "gpt-x",
        AuthSource::ApiKey("secret".into()),
    );
    let events = collect(adapter_stream(&adapter, req()).await).await;

    let completed = events
        .iter()
        .find_map(|e| match e {
            ChatEvent::ToolCallCompleted { name, input, .. } => Some((name.clone(), input.clone())),
            _ => None,
        })
        .expect("tool call completed");
    assert_eq!(completed.0, "do_it");
    assert_eq!(completed.1, serde_json::json!({"k": 1}));
    assert!(events
        .iter()
        .any(|e| matches!(e, ChatEvent::Done(StopReason::ToolUse))));
}

#[tokio::test]
async fn responses_excludes_cached_tokens_from_input() {
    // The Responses dialect (like Chat Completions) reports cached tokens as a
    // SUBSET of input_tokens; the adapter must split them so `input` is the
    // uncached prompt only (input_tokens 100, cached 80 => input 20, cache_read 80).
    let server = MockServer::start().await;
    let body = "\
event: response.completed\n\
data: {\"type\":\"response.completed\",\"response\":{\"output\":[],\"usage\":{\"input_tokens\":100,\"output_tokens\":5,\"input_tokens_details\":{\"cached_tokens\":80}}}}\n\n";
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(sse(body))
        .mount(&server)
        .await;

    let adapter = OpenAiResponsesAdapter::new(
        reqwest::Client::new(),
        "openai",
        format!("{}/v1", server.uri()),
        "gpt-x",
        AuthSource::ApiKey("secret".into()),
    );
    let events = collect(adapter_stream(&adapter, req()).await).await;
    assert!(
        events.iter().any(|e| matches!(
            e,
            ChatEvent::Usage(u) if u.input == 20 && u.cache_read == 80 && u.output == 5
        )),
        "input is the uncached remainder, cache_read holds the cached subset: {events:#?}"
    );
}

#[tokio::test]
async fn api_error_body_surfaces_as_provider_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(429).set_body_string(
            "{\"error\":{\"message\":\"rate limited\",\"type\":\"rate_limit_error\"}}",
        ))
        .mount(&server)
        .await;

    let adapter = OpenAiCompatAdapter::new(
        reqwest::Client::new(),
        "openai",
        format!("{}/v1", server.uri()),
        "m",
        AuthSource::None,
    );
    let err = adapter_stream_result(&adapter, req()).await.err().expect("error");
    match err {
        stepper_provider::ProviderError::Api { status, message, .. } => {
            assert_eq!(status, 429);
            assert_eq!(message, "rate limited");
        }
        other => panic!("expected Api error, got {other:?}"),
    }
}

// helpers ------------------------------------------------------------------

async fn adapter_stream<P: stepper_provider::LlmProvider>(
    adapter: &P,
    request: ChatRequest,
) -> stepper_provider::ChatStream {
    adapter
        .chat_stream(request, CancellationToken::new())
        .await
        .expect("stream opened")
}

async fn adapter_stream_result<P: stepper_provider::LlmProvider>(
    adapter: &P,
    request: ChatRequest,
) -> Result<stepper_provider::ChatStream, stepper_provider::ProviderError> {
    adapter.chat_stream(request, CancellationToken::new()).await
}
