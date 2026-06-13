//! SSE drive-loop robustness: a malformed data frame is skipped (the turn
//! survives), an unbroken run of junk frames hits the consecutive cap and turns
//! fatal, in-band API error frames stay fatal, and a stalled stream fails with
//! a retryable idle-timeout error instead of hanging forever.

use futures::stream::BoxStream;
use futures::StreamExt;
use stepper_providers::sse::{self, SseFrame};
use stepper_providers::{
    AuthSource, ChatEvent, ChatRequest, LlmProvider, Message, OpenAiCompatAdapter, ProviderError,
    StopReason,
};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn sse_body(body: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(body.as_bytes().to_vec(), "text/event-stream")
}

fn req() -> ChatRequest {
    ChatRequest::new("test-model").with_messages(vec![Message::user("hi")])
}

fn openai_parse(frame: &SseFrame) -> Result<Option<Vec<stepper_provider::WireDelta>>, ProviderError> {
    if frame.data.trim() == "[DONE]" {
        Ok(None)
    } else {
        stepper_providers::wire::openai::parse_chunk(&frame.data).map(Some)
    }
}

fn frames_from(
    items: Vec<Result<SseFrame, ProviderError>>,
) -> BoxStream<'static, Result<SseFrame, ProviderError>> {
    Box::pin(futures::stream::iter(items))
}

fn data_frame(data: &str) -> Result<SseFrame, ProviderError> {
    Ok(SseFrame {
        event: "message".into(),
        data: data.into(),
    })
}

#[tokio::test]
async fn one_junk_frame_between_two_good_deltas_still_yields_both_deltas() {
    let server = MockServer::start().await;
    let body = "\
data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"},\"index\":0}]}\n\n\
data: this is not json at all {{{\n\n\
data: {\"choices\":[{\"delta\":{\"content\":\" world\"},\"index\":0}]}\n\n\
data: {\"choices\":[{\"delta\":{},\"index\":0,\"finish_reason\":\"stop\"}]}\n\n\
data: [DONE]\n\n";
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(sse_body(body))
        .mount(&server)
        .await;

    let adapter = OpenAiCompatAdapter::new(
        reqwest::Client::new(),
        "openai",
        format!("{}/v1", server.uri()),
        "test-model",
        AuthSource::None,
    );
    let mut stream = adapter
        .chat_stream(req(), CancellationToken::new())
        .await
        .expect("stream opened");

    let mut text = String::new();
    let mut done = false;
    while let Some(item) = stream.next().await {
        match item.expect("the junk frame is skipped, never fatal") {
            ChatEvent::TextDelta(t) => text.push_str(&t),
            ChatEvent::Done(StopReason::EndTurn) => done = true,
            _ => {}
        }
    }
    assert_eq!(text, "Hello world", "both deltas around the junk frame survive");
    assert!(done, "the turn still terminates normally");
}

#[tokio::test]
async fn sixteen_consecutive_junk_frames_turn_fatal_before_any_later_delta() {
    let mut junk = String::new();
    for i in 0..16 {
        junk.push_str(&format!("data: garbled frame number {i} ]]]\n\n"));
    }
    junk.push_str("data: {\"choices\":[{\"delta\":{\"content\":\"late\"},\"index\":0}]}\n\n");
    junk.push_str("data: [DONE]\n\n");

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(sse_body(&junk))
        .mount(&server)
        .await;

    let adapter = OpenAiCompatAdapter::new(
        reqwest::Client::new(),
        "openai",
        format!("{}/v1", server.uri()),
        "test-model",
        AuthSource::None,
    );
    let mut stream = adapter
        .chat_stream(req(), CancellationToken::new())
        .await
        .expect("stream opened");

    let mut saw_late_delta = false;
    let mut fatal = None;
    while let Some(item) = stream.next().await {
        match item {
            Ok(ChatEvent::TextDelta(t)) if t == "late" => saw_late_delta = true,
            Ok(_) => {}
            Err(e) => {
                fatal = Some(e);
                break;
            }
        }
    }
    let fatal = fatal.expect("an unbroken junk run must become fatal");
    assert!(
        matches!(fatal, ProviderError::Decode(_)),
        "the cap surfaces the parse failure, got {fatal:?}"
    );
    assert!(
        !saw_late_delta,
        "the stream is dead before the late delta arrives"
    );
}

#[tokio::test]
async fn good_frames_reset_the_consecutive_parse_error_counter() {
    // 15 junk + 1 good + 15 junk stays under the cap of 16 consecutive errors.
    let mut body = String::new();
    for _ in 0..15 {
        body.push_str("data: junk %%%\n\n");
    }
    body.push_str("data: {\"choices\":[{\"delta\":{\"content\":\"mid\"},\"index\":0}]}\n\n");
    for _ in 0..15 {
        body.push_str("data: junk %%%\n\n");
    }
    body.push_str("data: {\"choices\":[{\"delta\":{},\"index\":0,\"finish_reason\":\"stop\"}]}\n\n");
    body.push_str("data: [DONE]\n\n");

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(sse_body(&body))
        .mount(&server)
        .await;

    let adapter = OpenAiCompatAdapter::new(
        reqwest::Client::new(),
        "openai",
        format!("{}/v1", server.uri()),
        "test-model",
        AuthSource::None,
    );
    let mut stream = adapter
        .chat_stream(req(), CancellationToken::new())
        .await
        .expect("stream opened");

    let mut text = String::new();
    let mut done = false;
    while let Some(item) = stream.next().await {
        match item.expect("interleaved junk never reaches the consecutive cap") {
            ChatEvent::TextDelta(t) => text.push_str(&t),
            ChatEvent::Done(_) => done = true,
            _ => {}
        }
    }
    assert_eq!(text, "mid");
    assert!(done);
}

#[tokio::test(start_paused = true)]
async fn a_stalled_frame_stream_fails_with_a_retryable_idle_timeout() {
    // The frame source pends forever after one good frame; with paused time the
    // 120s default idle deadline fires immediately instead of hanging the test.
    let stalled: BoxStream<'static, Result<SseFrame, ProviderError>> =
        Box::pin(async_stream::stream! {
            yield data_frame("{\"choices\":[{\"delta\":{\"content\":\"first\"},\"index\":0}]}");
            futures::future::pending::<()>().await;
        });

    let mut events = sse::drive(stalled, openai_parse);
    match events.next().await {
        Some(Ok(ChatEvent::TextDelta(t))) => assert_eq!(t, "first"),
        other => panic!("expected the first delta, got {other:?}"),
    }
    let err = match events.next().await {
        Some(Err(e)) => e,
        other => panic!("expected the idle timeout error, got {other:?}"),
    };
    assert!(
        matches!(err, ProviderError::Transport(ref m) if m.contains("idle timeout")),
        "a stall surfaces as a Transport idle timeout, got {err:?}"
    );
    assert!(err.is_retryable(), "a mid-stream stall must be retryable");
    assert!(events.next().await.is_none(), "the stream is finished");
}

#[tokio::test(start_paused = true)]
async fn the_idle_timer_resets_on_every_frame() {
    // Three frames 80s apart: total elapsed (160s) exceeds the 120s deadline,
    // but no single gap does — the stream must complete without a timeout.
    let spaced: BoxStream<'static, Result<SseFrame, ProviderError>> =
        Box::pin(async_stream::stream! {
            yield data_frame("{\"choices\":[{\"delta\":{\"content\":\"a\"},\"index\":0}]}");
            tokio::time::sleep(std::time::Duration::from_secs(80)).await;
            yield data_frame("{\"choices\":[{\"delta\":{\"content\":\"b\"},\"index\":0}]}");
            tokio::time::sleep(std::time::Duration::from_secs(80)).await;
            yield data_frame("{\"choices\":[{\"delta\":{},\"index\":0,\"finish_reason\":\"stop\"}]}");
        });

    let mut events = sse::drive(spaced, openai_parse);
    let mut text = String::new();
    let mut done = false;
    while let Some(item) = events.next().await {
        match item.expect("per-frame resets keep the stream alive") {
            ChatEvent::TextDelta(t) => text.push_str(&t),
            ChatEvent::Done(_) => done = true,
            _ => {}
        }
    }
    assert_eq!(text, "ab");
    assert!(done);
}

#[tokio::test]
async fn in_band_api_error_frames_are_still_fatal_not_skipped() {
    // The malformed-frame tolerance must never swallow a real in-band error.
    let frames = frames_from(vec![
        data_frame("{\"choices\":[{\"delta\":{\"content\":\"ok\"},\"index\":0}]}"),
        Ok(SseFrame {
            event: "error".into(),
            data: "{\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"overloaded\"}}".into(),
        }),
        data_frame("{\"choices\":[{\"delta\":{\"content\":\"never\"},\"index\":0}]}"),
    ]);
    let mut events = sse::drive(frames, |frame| {
        if frame.event == "error" {
            stepper_providers::wire::anthropic::parse_event(&frame.event, &frame.data).map(Some)
        } else {
            openai_parse(frame)
        }
    });

    match events.next().await {
        Some(Ok(ChatEvent::TextDelta(t))) => assert_eq!(t, "ok"),
        other => panic!("expected the leading delta, got {other:?}"),
    }
    match events.next().await {
        Some(Err(ProviderError::Api { code, .. })) => {
            assert_eq!(code.as_deref(), Some("overloaded_error"));
        }
        other => panic!("the in-band error stays fatal, got {other:?}"),
    }
    assert!(events.next().await.is_none(), "nothing streams after a fatal error");
}

#[tokio::test]
async fn transport_errors_from_the_frame_source_are_still_fatal() {
    let frames = frames_from(vec![
        data_frame("{\"choices\":[{\"delta\":{\"content\":\"ok\"},\"index\":0}]}"),
        Err(ProviderError::Transport("connection reset".into())),
    ]);
    let mut events = sse::drive(frames, openai_parse);
    assert!(matches!(
        events.next().await,
        Some(Ok(ChatEvent::TextDelta(_)))
    ));
    assert!(matches!(
        events.next().await,
        Some(Err(ProviderError::Transport(_)))
    ));
    assert!(events.next().await.is_none());
}
