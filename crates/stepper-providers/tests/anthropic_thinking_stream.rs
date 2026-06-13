//! Anthropic thinking round trip through the real adapter: `thinking_delta`
//! frames stream as `ThinkingDelta`, `signature_delta` surfaces as
//! `ThinkingSignature`, and the folded response carries a signed Thinking block
//! that the encoder can replay into history.

use futures::StreamExt;
use stepper_provider::ContentBlock;
use stepper_providers::{
    AnthropicAdapter, AuthSource, ChatEvent, ChatRequest, ChatResponse, LlmProvider, Message,
    Role, StopReason,
};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn sse(body: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(body.as_bytes().to_vec(), "text/event-stream")
}

#[tokio::test]
async fn thinking_and_signature_deltas_fold_into_a_signed_thinking_block() {
    let server = MockServer::start().await;
    let body = "\
event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":10}}}\n\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"step by step\"}}\n\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig-xyz\"}}\n\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"answer\"}}\n\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\",\"index\":1}\n\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":7}}\n\n\
event: message_stop\n\
data: {\"type\":\"message_stop\"}\n\n";
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
    let mut stream = adapter
        .chat_stream(
            ChatRequest::new("claude-x").with_messages(vec![Message::user("hi")]),
            CancellationToken::new(),
        )
        .await
        .expect("stream opened");

    let mut events = Vec::new();
    while let Some(item) = stream.next().await {
        events.push(item.expect("no stream error"));
    }
    assert!(events
        .iter()
        .any(|e| matches!(e, ChatEvent::ThinkingDelta(t) if t == "step by step")));
    assert!(events
        .iter()
        .any(|e| matches!(e, ChatEvent::ThinkingSignature(s) if s == "sig-xyz")));
    assert!(events
        .iter()
        .any(|e| matches!(e, ChatEvent::Done(StopReason::EndTurn))));

    // Fold and replay: the signed thinking block survives a history re-encode.
    let resp = ChatResponse::from_events(events);
    match &resp.content[0] {
        ContentBlock::Thinking { text, signature } => {
            assert_eq!(text, "step by step");
            assert_eq!(signature.as_deref(), Some("sig-xyz"));
        }
        other => panic!("expected the signed thinking block first, got {other:?}"),
    }
    assert_eq!(resp.text(), "answer");

    let history = ChatRequest::new("claude-x").with_messages(vec![
        Message::user("hi"),
        Message {
            role: Role::Assistant,
            content: resp.content.clone(),
        },
        Message::user("continue"),
    ]);
    let encoded = stepper_providers::wire::anthropic::build_request_body(&history, "claude-x", true);
    let assistant = &encoded["messages"][1];
    assert_eq!(assistant["content"][0]["type"], "thinking");
    assert_eq!(assistant["content"][0]["thinking"], "step by step");
    assert_eq!(assistant["content"][0]["signature"], "sig-xyz");
}
