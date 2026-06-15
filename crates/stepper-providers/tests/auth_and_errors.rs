//! Request-shape, auth-header, failure-path, and cancellation tests for the SSE
//! adapters. A wiremock server matches per-provider routing/auth and serves
//! canned bodies (success, malformed, error envelopes); no real network or keys.

use futures::StreamExt;
use serde_json::Value;
use stepper_providers::{
    AnthropicAdapter, AuthSource, ChatEvent, ChatRequest, LlmProvider, Message,
    OpenAiCompatAdapter, OpenAiResponsesAdapter, ProviderError, StopReason,
};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{header, header_exists, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn sse(body: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(body.as_bytes().to_vec(), "text/event-stream")
}

async fn collect(stream: stepper_provider::ChatStream) -> Vec<ChatEvent> {
    let mut stream = stream;
    let mut out = Vec::new();
    while let Some(item) = stream.next().await {
        out.push(item.expect("no stream error"));
    }
    out
}

fn req() -> ChatRequest {
    ChatRequest::new("test-model")
        .with_system("you are helpful")
        .with_messages(vec![Message::user("hi")])
}

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
) -> Result<stepper_provider::ChatStream, ProviderError> {
    adapter.chat_stream(request, CancellationToken::new()).await
}

fn fake_codex_store() -> stepper_providers::CodexTokenStore {
    use stepper_providers::codex::store::CodexCredentials;
    let dir = tempfile::tempdir().expect("tempdir");
    let creds = CodexCredentials {
        access_token: "access".to_string().into(),
        refresh_token: "refresh".to_string().into(),
        id_token: "id".to_string().into(),
        account_id: "acct".to_string(),
        expires_at: u64::MAX,
        last_refresh: 0,
    };
    stepper_providers::CodexTokenStore::create(
        dir.path().join("codex-auth.json"),
        reqwest::Client::new(),
        creds,
    )
    .expect("create store")
}

async fn only_request_body(server: &MockServer) -> Value {
    let requests = server
        .received_requests()
        .await
        .expect("recording enabled");
    assert_eq!(requests.len(), 1, "exactly one request expected");
    requests[0].body_json().expect("request body is json")
}

#[tokio::test]
async fn openai_compat_sends_bearer_auth_and_hits_chat_completions_path() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", "Bearer sk-test"))
        .respond_with(sse("data: [DONE]\n\n"))
        .expect(1)
        .mount(&server)
        .await;

    let adapter = OpenAiCompatAdapter::new(
        reqwest::Client::new(),
        "openai",
        format!("{}/v1", server.uri()),
        "test-model",
        AuthSource::ApiKey("sk-test".into()),
    );
    let events = collect(adapter_stream(&adapter, req()).await).await;
    assert!(events.is_empty());

    let body = only_request_body(&server).await;
    assert_eq!(body["model"], "test-model");
    assert_eq!(body["stream"], Value::Bool(true));
}

#[tokio::test]
async fn openai_compat_without_key_omits_authorization_header() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(sse("data: [DONE]\n\n"))
        .mount(&server)
        .await;

    let adapter = OpenAiCompatAdapter::new(
        reqwest::Client::new(),
        "openai",
        format!("{}/v1", server.uri()),
        "test-model",
        AuthSource::None,
    );
    collect(adapter_stream(&adapter, req()).await).await;

    let requests = server.received_requests().await.expect("recording enabled");
    assert!(!requests[0].headers.contains_key("authorization"));
}

#[tokio::test]
async fn anthropic_sends_api_key_version_and_hoists_system() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "secret"))
        .and(header("anthropic-version", "2023-06-01"))
        .and(header_exists("x-api-key"))
        .respond_with(sse("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"))
        .expect(1)
        .mount(&server)
        .await;

    let adapter = AnthropicAdapter::new(
        reqwest::Client::new(),
        "anthropic",
        server.uri(),
        "claude-x",
        AuthSource::ApiKey("secret".into()),
    );
    collect(adapter_stream(&adapter, req()).await).await;

    let body = only_request_body(&server).await;
    assert_eq!(body["system"], "you are helpful");
    assert_eq!(body["max_tokens"], 8192);
    let messages = body["messages"].as_array().unwrap();
    assert!(messages.iter().all(|m| m["role"] != "system"));
    assert_eq!(messages[0]["role"], "user");
}

#[tokio::test]
async fn anthropic_codex_auth_is_rejected_before_any_request() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(sse("data: [DONE]\n\n"))
        .expect(0)
        .mount(&server)
        .await;

    let store = fake_codex_store();
    let adapter = AnthropicAdapter::new(
        reqwest::Client::new(),
        "anthropic",
        server.uri(),
        "claude-x",
        AuthSource::Codex(store),
    );
    let err = adapter_stream_result(&adapter, req())
        .await
        .err()
        .expect("error");
    assert!(matches!(err, ProviderError::Unsupported(_)));
}

#[tokio::test]
async fn responses_sends_bearer_auth_and_hits_responses_path() {
    let server = MockServer::start().await;
    let body = "\
event: response.completed\n\
data: {\"type\":\"response.completed\",\"response\":{\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n";
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(header("authorization", "Bearer sk-resp"))
        .respond_with(sse(body))
        .expect(1)
        .mount(&server)
        .await;

    let adapter = OpenAiResponsesAdapter::new(
        reqwest::Client::new(),
        "openai",
        format!("{}/v1", server.uri()),
        "gpt-x",
        AuthSource::ApiKey("sk-resp".into()),
    );
    collect(adapter_stream(&adapter, req()).await).await;

    let request_body = only_request_body(&server).await;
    assert_eq!(request_body["instructions"], "you are helpful");
}

#[tokio::test]
async fn responses_without_auth_errors_before_request() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(sse("data: [DONE]\n\n"))
        .expect(0)
        .mount(&server)
        .await;

    let adapter = OpenAiResponsesAdapter::new(
        reqwest::Client::new(),
        "openai",
        format!("{}/v1", server.uri()),
        "gpt-x",
        AuthSource::None,
    );
    let err = adapter_stream_result(&adapter, req())
        .await
        .err()
        .expect("error");
    assert!(matches!(err, ProviderError::Auth(_)));
}

#[tokio::test]
async fn openai_compat_single_malformed_sse_frame_is_skipped_and_the_turn_survives() {
    // The old behavior — one garbled frame aborting the whole turn — was the
    // IMP-10 bug; a lone bad frame is now skipped and later deltas still arrive.
    let server = MockServer::start().await;
    let body = "\
data: {not valid json}\n\n\
data: {\"choices\":[{\"delta\":{\"content\":\"still here\"},\"index\":0}]}\n\n\
data: {\"choices\":[{\"delta\":{},\"index\":0,\"finish_reason\":\"stop\"}]}\n\n\
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
        "m",
        AuthSource::None,
    );
    let events = collect(adapter_stream(&adapter, req()).await).await;
    assert!(events
        .iter()
        .any(|e| matches!(e, ChatEvent::TextDelta(t) if t == "still here")));
    assert!(events
        .iter()
        .any(|e| matches!(e, ChatEvent::Done(StopReason::EndTurn))));
}

#[tokio::test]
async fn anthropic_single_malformed_sse_frame_is_skipped_and_the_turn_survives() {
    let server = MockServer::start().await;
    let body = "\
event: content_block_delta\n\
data: {bogus]\n\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n";
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
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
        .any(|e| matches!(e, ChatEvent::TextDelta(t) if t == "ok")));
    assert!(events
        .iter()
        .any(|e| matches!(e, ChatEvent::Done(StopReason::EndTurn))));
}

#[tokio::test]
async fn anthropic_unknown_event_is_ignored_and_stream_ends_cleanly() {
    let server = MockServer::start().await;
    let body = "event: ping\ndata: {}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
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
    assert!(events.is_empty());
}

#[tokio::test]
async fn openai_error_envelope_message_and_code_extracted() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(400).set_body_string(
            "{\"error\":{\"message\":\"invalid model\",\"code\":\"model_not_found\",\"type\":\"invalid_request_error\"}}",
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
    let err = adapter_stream_result(&adapter, req())
        .await
        .err()
        .expect("error");
    match err {
        ProviderError::Api {
            status,
            code,
            message,
            ..
        } => {
            assert_eq!(status, 400);
            assert_eq!(message, "invalid model");
            assert_eq!(code.as_deref(), Some("model_not_found"));
        }
        other => panic!("expected Api error, got {other:?}"),
    }
}

#[tokio::test]
async fn anthropic_error_envelope_message_and_type_extracted() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(529).set_body_string(
            "{\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}",
        ))
        .mount(&server)
        .await;

    let adapter = AnthropicAdapter::new(
        reqwest::Client::new(),
        "anthropic",
        server.uri(),
        "claude-x",
        AuthSource::ApiKey("secret".into()),
    );
    let err = adapter_stream_result(&adapter, req())
        .await
        .err()
        .expect("error");
    match err {
        ProviderError::Api {
            status,
            code,
            message,
            ..
        } => {
            assert_eq!(status, 529);
            assert_eq!(message, "Overloaded");
            assert_eq!(code.as_deref(), Some("overloaded_error"));
        }
        other => panic!("expected Api error, got {other:?}"),
    }
}

#[tokio::test]
async fn non_json_error_body_falls_back_to_raw_message() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(502).set_body_string("upstream is down"))
        .mount(&server)
        .await;

    let adapter = OpenAiCompatAdapter::new(
        reqwest::Client::new(),
        "openai",
        format!("{}/v1", server.uri()),
        "m",
        AuthSource::None,
    );
    let err = adapter_stream_result(&adapter, req())
        .await
        .err()
        .expect("error");
    match err {
        ProviderError::Api {
            status,
            code,
            message,
            ..
        } => {
            assert_eq!(status, 502);
            assert_eq!(code, None);
            assert_eq!(message, "upstream is down");
        }
        other => panic!("expected Api error, got {other:?}"),
    }
}

#[tokio::test]
async fn rate_limit_429_carries_retry_after_header() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "7")
                .set_body_string("{\"error\":{\"message\":\"slow down\"}}"),
        )
        .mount(&server)
        .await;

    let adapter = OpenAiCompatAdapter::new(
        reqwest::Client::new(),
        "openai",
        format!("{}/v1", server.uri()),
        "m",
        AuthSource::None,
    );
    let err = adapter_stream_result(&adapter, req())
        .await
        .err()
        .expect("error");
    match err {
        ProviderError::Api { status, retry_after, .. } => {
            assert_eq!(status, 429);
            assert_eq!(retry_after, Some(std::time::Duration::from_secs(7)));
        }
        other => panic!("expected Api error, got {other:?}"),
    }
}

#[tokio::test]
async fn cancel_mid_stream_terminates_with_cancelled() {
    let server = MockServer::start().await;
    let body = "\
data: {\"choices\":[{\"delta\":{\"content\":\"one\"},\"index\":0}]}\n\n\
data: {\"choices\":[{\"delta\":{\"content\":\"two\"},\"index\":0}]}\n\n\
data: {\"choices\":[{\"delta\":{},\"index\":0,\"finish_reason\":\"stop\"}]}\n\n\
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
        "m",
        AuthSource::None,
    );
    let cancel = CancellationToken::new();
    let mut stream = adapter
        .chat_stream(req(), cancel.clone())
        .await
        .expect("stream opened");

    let first = stream.next().await.expect("first event").expect("ok");
    assert!(matches!(first, ChatEvent::TextDelta(t) if t == "one"));

    cancel.cancel();

    let next = tokio::time::timeout(std::time::Duration::from_secs(1), stream.next())
        .await
        .expect("stream reacts promptly to cancellation")
        .expect("an item");
    assert!(matches!(next, Err(ProviderError::Cancelled)));

    let after = tokio::time::timeout(std::time::Duration::from_secs(1), stream.next())
        .await
        .expect("stream ends promptly after cancellation");
    assert!(after.is_none(), "stream stops after Cancelled");
}

#[tokio::test]
async fn ollama_cloud_uses_openai_compat_encoding_and_path() {
    let server = MockServer::start().await;
    let body = "\
data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"index\":0}]}\n\n\
data: {\"choices\":[{\"delta\":{},\"index\":0,\"finish_reason\":\"stop\"}]}\n\n\
data: [DONE]\n\n";
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", "Bearer ollama-key"))
        .respond_with(sse(body))
        .expect(1)
        .mount(&server)
        .await;

    let adapter = OpenAiCompatAdapter::new(
        reqwest::Client::new(),
        "ollama-cloud",
        format!("{}/v1", server.uri()),
        "gpt-oss",
        AuthSource::ApiKey("ollama-key".into()),
    );
    let events = collect(adapter_stream(&adapter, req()).await).await;
    let text: String = events
        .iter()
        .filter_map(|e| match e {
            ChatEvent::TextDelta(t) => Some(t.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "hi");

    let request_body = only_request_body(&server).await;
    assert_eq!(request_body["model"], "gpt-oss");
    assert_eq!(request_body["stream_options"]["include_usage"], true);
    assert_eq!(request_body["messages"][0]["role"], "system");
    assert_eq!(request_body["messages"][0]["content"], "you are helpful");
    assert_eq!(request_body["messages"][1]["role"], "user");
    assert_eq!(request_body["messages"][1]["content"], "hi");
}
