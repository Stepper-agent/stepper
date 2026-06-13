//! The default `LlmProvider::chat` must not fold a stream that ended without a
//! terminal `Done` event into a fake successful `EndTurn` response: a clean
//! upstream close mid-turn (the most common transient failure) surfaces as
//! `ProviderError::UnexpectedEnd`, which is retryable. These tests drive the
//! REAL default `chat()` implementation through a scripted `chat_stream`.

use async_trait::async_trait;
use futures::stream;
use stepper_provider::{
    ChatEvent, ChatRequest, ChatStream, LlmProvider, ProviderError, StopReason,
};
use tokio_util::sync::CancellationToken;

struct ScriptedProvider {
    events: Vec<Result<ChatEvent, ProviderError>>,
}

impl ScriptedProvider {
    fn new(events: Vec<Result<ChatEvent, ProviderError>>) -> Self {
        ScriptedProvider { events }
    }
}

#[async_trait]
impl LlmProvider for ScriptedProvider {
    fn provider(&self) -> &str {
        "scripted"
    }

    fn model(&self) -> &str {
        "scripted-model"
    }

    async fn chat_stream(
        &self,
        _request: ChatRequest,
        _cancel: CancellationToken,
    ) -> Result<ChatStream, ProviderError> {
        let cloned: Vec<Result<ChatEvent, ProviderError>> = self
            .events
            .iter()
            .map(|item| match item {
                Ok(ev) => Ok(ev.clone()),
                Err(ProviderError::Transport(m)) => Err(ProviderError::Transport(m.clone())),
                Err(_) => unreachable!("tests only script Transport errors"),
            })
            .collect();
        Ok(Box::pin(stream::iter(cloned)))
    }
}

#[tokio::test]
async fn stream_that_ends_without_done_is_an_unexpected_end_error_not_end_turn() {
    let provider = ScriptedProvider::new(vec![
        Ok(ChatEvent::TextDelta("truncated mid-".into())),
        Ok(ChatEvent::TextDelta("sentence".into())),
    ]);
    let err = provider
        .chat(ChatRequest::new("m"), CancellationToken::new())
        .await
        .expect_err("a Done-less stream must not fold into a successful response");
    assert!(
        matches!(err, ProviderError::UnexpectedEnd),
        "expected UnexpectedEnd, got {err:?}"
    );
    assert!(
        err.is_retryable(),
        "a truncated stream is a transient failure and must be retryable"
    );
}

#[tokio::test]
async fn completely_empty_stream_is_also_an_unexpected_end_error() {
    let provider = ScriptedProvider::new(vec![]);
    let err = provider
        .chat(ChatRequest::new("m"), CancellationToken::new())
        .await
        .expect_err("an empty stream carries no terminal event");
    assert!(matches!(err, ProviderError::UnexpectedEnd));
}

#[tokio::test]
async fn stream_with_a_terminal_done_still_folds_into_a_response() {
    let provider = ScriptedProvider::new(vec![
        Ok(ChatEvent::TextDelta("hello".into())),
        Ok(ChatEvent::Done(StopReason::EndTurn)),
    ]);
    let resp = provider
        .chat(ChatRequest::new("m"), CancellationToken::new())
        .await
        .expect("a properly terminated stream folds normally");
    assert_eq!(resp.text(), "hello");
    assert_eq!(resp.stop_reason, StopReason::EndTurn);
}

#[tokio::test]
async fn mid_stream_error_still_propagates_as_that_error() {
    let provider = ScriptedProvider::new(vec![
        Ok(ChatEvent::TextDelta("partial".into())),
        Err(ProviderError::Transport("connection reset".into())),
    ]);
    let err = provider
        .chat(ChatRequest::new("m"), CancellationToken::new())
        .await
        .expect_err("a stream error propagates");
    assert!(
        matches!(err, ProviderError::Transport(ref m) if m == "connection reset"),
        "the original error wins over the missing-Done guard, got {err:?}"
    );
}
