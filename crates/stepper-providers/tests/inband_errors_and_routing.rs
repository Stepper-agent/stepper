//! In-band SSE error frames (a 200 body that carries a named `error` /
//! `response.failed` event), the Codex reactive 401 → force_refresh → retry
//! branch, and the proof that oMLX shares the OpenAI-compat request path. A
//! wiremock server stands in for every upstream — no real network, no real keys.

use futures::StreamExt;
use serde_json::Value;
use std::net::TcpListener;
use stepper_providers::{
    AuthSource, ChatEvent, ChatRequest, LlmProvider, Message, OpenAiResponsesAdapter,
    ProviderError, ProviderFactory, ProviderKind, ProviderSpec,
};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn sse(body: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(body.as_bytes().to_vec(), "text/event-stream")
}

fn req() -> ChatRequest {
    ChatRequest::new("test-model")
        .with_system("you are helpful")
        .with_messages(vec![Message::user("hi")])
}

async fn drain_err<P: LlmProvider>(adapter: &P, request: ChatRequest) -> ProviderError {
    let mut stream = adapter
        .chat_stream(request, CancellationToken::new())
        .await
        .expect("stream opened on 200");
    let mut last_err = None;
    while let Some(item) = stream.next().await {
        if let Err(e) = item {
            last_err = Some(e);
            break;
        }
    }
    last_err.expect("the in-band error frame must surface a stream error")
}

fn codex_store_refresh_to(
    refresh_addr: std::net::SocketAddr,
) -> (stepper_providers::CodexTokenStore, tempfile::TempDir) {
    use stepper_providers::codex::store::CodexCredentials;
    // Point the store's refresh client at a loopback address for `auth.openai.com`
    // so the (hardcoded https) TOKEN_URL never touches the real network.
    let client = reqwest::Client::builder()
        .resolve("auth.openai.com", refresh_addr)
        .build()
        .expect("client builds");
    let creds = CodexCredentials {
        access_token: "stale-access".to_string().into(),
        refresh_token: "refresh".to_string().into(),
        id_token: "id".to_string().into(),
        account_id: "acct".to_string(),
        // u64::MAX so `bearer()` never refreshes proactively; only the reactive
        // 401 path can trigger a refresh.
        expires_at: u64::MAX,
        last_refresh: 0,
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let store = stepper_providers::CodexTokenStore::create(
        dir.path().join("codex-auth.json"),
        client,
        creds,
    )
    .expect("create store");
    (store, dir)
}

#[tokio::test]
async fn anthropic_inband_error_event_maps_to_api_error() {
    let server = MockServer::start().await;
    // A 200 OK stream that begins normally then emits a named `error` frame —
    // exercises `wire::anthropic::parse_event("error", ..)`, which must yield
    // ProviderError::Api { status: 0, code: <error type> }.
    let body = "\
event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":3}}}\n\n\
event: error\n\
data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"the server is overloaded\"}}\n\n";
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(sse(body))
        .mount(&server)
        .await;

    let adapter = stepper_providers::AnthropicAdapter::new(
        reqwest::Client::new(),
        "anthropic",
        server.uri(),
        "claude-x",
        AuthSource::ApiKey("secret".into()),
    );
    let err = drain_err(&adapter, req()).await;
    match err {
        ProviderError::Api {
            status,
            code,
            message,
            ..
        } => {
            assert_eq!(status, 0, "in-band frame has no HTTP status");
            assert_eq!(code.as_deref(), Some("overloaded_error"));
            assert_eq!(message, "the server is overloaded");
        }
        other => panic!("expected in-band Api error, got {other:?}"),
    }
}

#[tokio::test]
async fn anthropic_inband_error_event_without_message_uses_default() {
    let server = MockServer::start().await;
    let body = "\
event: error\n\
data: {\"type\":\"error\",\"error\":{\"type\":\"api_error\"}}\n\n";
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(sse(body))
        .mount(&server)
        .await;

    let adapter = stepper_providers::AnthropicAdapter::new(
        reqwest::Client::new(),
        "anthropic",
        server.uri(),
        "claude-x",
        AuthSource::ApiKey("secret".into()),
    );
    let err = drain_err(&adapter, req()).await;
    match err {
        ProviderError::Api {
            status,
            code,
            message,
            ..
        } => {
            assert_eq!(status, 0);
            assert_eq!(code.as_deref(), Some("api_error"));
            assert_eq!(message, "anthropic stream error");
        }
        other => panic!("expected in-band Api error, got {other:?}"),
    }
}

#[tokio::test]
async fn responses_inband_response_failed_maps_to_api_error() {
    let server = MockServer::start().await;
    // A 200 OK stream carrying `response.failed` — exercises the
    // `wire::responses::parse_event("response.failed", ..)` branch which pulls
    // the message from `/response/error/message`.
    let body = "\
event: response.created\n\
data: {\"type\":\"response.created\",\"response\":{}}\n\n\
event: response.failed\n\
data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"server_error\",\"message\":\"the model crashed\"}}}\n\n";
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
        AuthSource::ApiKey("sk".into()),
    );
    let err = drain_err(&adapter, req()).await;
    match err {
        ProviderError::Api {
            status,
            code,
            message,
            ..
        } => {
            assert_eq!(status, 0);
            assert_eq!(code, None, "responses in-band frames carry no code");
            assert_eq!(message, "the model crashed");
        }
        other => panic!("expected in-band Api error, got {other:?}"),
    }
}

#[tokio::test]
async fn responses_inband_error_event_maps_to_api_error() {
    let server = MockServer::start().await;
    // The other half of the `"response.failed" | "error"` arm: a bare `error`
    // event whose message lives under `/error/message`.
    let body = "\
event: error\n\
data: {\"type\":\"error\",\"error\":{\"message\":\"rate limited mid-stream\"}}\n\n";
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
        AuthSource::ApiKey("sk".into()),
    );
    let err = drain_err(&adapter, req()).await;
    match err {
        ProviderError::Api {
            status, message, ..
        } => {
            assert_eq!(status, 0);
            assert_eq!(message, "rate limited mid-stream");
        }
        other => panic!("expected in-band Api error, got {other:?}"),
    }
}

#[tokio::test]
async fn codex_401_triggers_exactly_one_reactive_refresh_attempt() {
    let server = MockServer::start().await;
    // The Codex backend rejects the first call with 401. The adapter must enter
    // the `status == 401` branch and call `store.force_refresh()`. We point the
    // store's refresh at a closed loopback port so the (hardcoded https)
    // TOKEN_URL never reaches the network; the refresh fails fast, the `?`
    // propagates, and the retry never fires — so the /responses endpoint is hit
    // exactly once. Without the 401 branch, the raw 401 would surface as an Api
    // error and this test's `Auth` assertion would fail.
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(401).set_body_string(
            "{\"error\":{\"message\":\"token expired\",\"type\":\"invalid_request_error\"}}",
        ))
        .expect(1)
        .mount(&server)
        .await;

    // A loopback port that is bound then immediately released → connection
    // refused, deterministic and hermetic (no egress).
    let closed = TcpListener::bind("127.0.0.1:0").expect("bind");
    let closed_addr = closed.local_addr().expect("addr");
    drop(closed);

    let (store, _dir) = codex_store_refresh_to(closed_addr);
    let adapter = OpenAiResponsesAdapter::new(
        reqwest::Client::new(),
        "codex",
        server.uri(),
        "gpt-5",
        AuthSource::Codex(store),
    );

    let err = adapter
        .chat_stream(req(), CancellationToken::new())
        .await
        .err()
        .expect("refresh failure must surface as an error");
    assert!(
        matches!(err, ProviderError::Auth(_)),
        "the reactive refresh failure surfaces as Auth, not a raw 401 Api error; got {err:?}"
    );

    // Exactly one hit on /responses proves the retry did not fire (the refresh
    // erred first) yet the 401 branch was reached.
    let requests = server.received_requests().await.expect("recording enabled");
    let responses_hits = requests
        .iter()
        .filter(|r| r.url.path() == "/responses")
        .count();
    assert_eq!(responses_hits, 1, "exactly one /responses request");
}

#[tokio::test]
async fn codex_non_401_status_does_not_attempt_refresh() {
    // Pins the negative of the 401 branch: a 500 must NOT trigger a refresh; it
    // surfaces directly as an Api error from `into_frames`, and /responses is hit
    // exactly once.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(500).set_body_string("{\"detail\":\"boom\"}"))
        .expect(1)
        .mount(&server)
        .await;

    let closed = TcpListener::bind("127.0.0.1:0").expect("bind");
    let closed_addr = closed.local_addr().expect("addr");
    drop(closed);

    let (store, _dir) = codex_store_refresh_to(closed_addr);
    let adapter = OpenAiResponsesAdapter::new(
        reqwest::Client::new(),
        "codex",
        server.uri(),
        "gpt-5",
        AuthSource::Codex(store),
    );

    let err = adapter
        .chat_stream(req(), CancellationToken::new())
        .await
        .err()
        .expect("500 surfaces as error");
    match err {
        ProviderError::Api { status, .. } => assert_eq!(status, 500),
        other => panic!("a non-401 must pass through as Api, got {other:?}"),
    }
}

#[tokio::test]
async fn omlx_routes_through_openai_compat_like_ollama_cloud() {
    // oMLX is wired as ProviderKind::OpenAiCompat (factory.rs). Building it via
    // the factory and asserting it hits the same `/v1/chat/completions` path with
    // the same OpenAI-compat body proves it shares the OpenAiCompatAdapter path
    // that ollama-cloud uses — not a bespoke encoder.
    let server = MockServer::start().await;
    let body = "\
data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"index\":0}]}\n\n\
data: {\"choices\":[{\"delta\":{},\"index\":0,\"finish_reason\":\"stop\"}]}\n\n\
data: [DONE]\n\n";
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(sse(body))
        .expect(1)
        .mount(&server)
        .await;

    let factory = ProviderFactory::new().expect("factory");
    let spec = ProviderSpec::new(ProviderKind::OpenAiCompat, "omlx", "qwen3")
        .with_base_url(format!("{}/v1", server.uri()))
        // Local oMLX needs no credentials → AuthSource::None.
        .with_api_key("none");
    let adapter = factory.build(spec).expect("omlx adapter builds");

    let mut stream = adapter
        .chat_stream(req(), CancellationToken::new())
        .await
        .expect("stream opened");
    let mut text = String::new();
    while let Some(item) = stream.next().await {
        if let ChatEvent::TextDelta(t) = item.expect("no error") {
            text.push_str(&t);
        }
    }
    assert_eq!(text, "ok");

    let requests = server.received_requests().await.expect("recording enabled");
    assert_eq!(requests.len(), 1);
    let request_body: Value = requests[0].body_json().expect("json body");
    // Same OpenAI-compat shape ollama-cloud emits: model, stream + include_usage,
    // and a leading system message — the shared OpenAiCompatAdapter encoding.
    assert_eq!(request_body["model"], "qwen3");
    assert_eq!(request_body["stream"], Value::Bool(true));
    assert_eq!(request_body["stream_options"]["include_usage"], true);
    assert_eq!(request_body["messages"][0]["role"], "system");
    assert_eq!(request_body["messages"][0]["content"], "you are helpful");
    assert_eq!(request_body["messages"][1]["role"], "user");

    // No api key for a local oMLX server → no Authorization header (AuthSource::None).
    assert!(
        !requests[0].headers.contains_key("authorization"),
        "local oMLX sends no bearer auth, matching AuthSource::None"
    );
}
