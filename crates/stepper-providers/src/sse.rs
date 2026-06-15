use crate::error;
use eventsource_stream::Eventsource;
use futures::stream::BoxStream;
use futures::StreamExt;
use std::time::Duration;
use stepper_provider::{ChatStream, ProviderError, StreamAccumulator, WireDelta};
use tokio_util::sync::CancellationToken;

/// How many malformed frames in a row are tolerated before the stream is
/// declared unrecoverable. One garbled frame from an OpenAI-compat proxy must
/// not abort the turn, but an endless junk stream must not spin forever.
const MAX_CONSECUTIVE_PARSE_ERRORS: u32 = 16;

/// Inactivity deadline between SSE frames, reset on every frame. A provider
/// that accepts the request and then stalls mid-stream would otherwise hang the
/// turn forever. Override with `STEPPER_SSE_IDLE_TIMEOUT_MS`.
fn idle_timeout() -> Duration {
    let ms = std::env::var("STEPPER_SSE_IDLE_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(120_000);
    Duration::from_millis(ms)
}

/// One Server-Sent-Events frame: the named `event:` (defaulting to `message`)
/// and the accumulated `data:` payload.
#[derive(Debug, Clone)]
pub struct SseFrame {
    pub event: String,
    pub data: String,
}

/// Send a prepared request, returning the raw response without consuming it, so
/// callers can inspect the status (e.g. Codex 401 → refresh + retry) before
/// committing to the stream.
pub async fn send(rb: reqwest::RequestBuilder) -> Result<reqwest::Response, ProviderError> {
    rb.send().await.map_err(error::transport)
}

/// Turn a successful streaming response into a frame stream. Non-2xx responses
/// are drained and surfaced as `ProviderError::Api`. The returned stream is
/// cancellation-aware: a fired `CancellationToken` yields a single
/// `ProviderError::Cancelled` and stops.
pub async fn into_frames(
    resp: reqwest::Response,
    cancel: CancellationToken,
) -> Result<BoxStream<'static, Result<SseFrame, ProviderError>>, ProviderError> {
    let status = resp.status();
    if !status.is_success() {
        // Capture rate-limit headers before the body consumes the response, so a
        // 429 can wait the server-specified delay instead of guessing.
        let retry_after = error::parse_retry_after(resp.headers());
        let body = resp
            .text()
            .await
            .unwrap_or_else(|e| format!("[error body could not be decoded: {e}]"));
        let mut err = error::api_error_from_body(status.as_u16(), &body);
        if let ProviderError::Api { retry_after: slot, .. } = &mut err {
            *slot = retry_after;
        }
        return Err(err);
    }

    let events = resp.bytes_stream().eventsource();
    let stream = async_stream::stream! {
        futures::pin_mut!(events);
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    yield Err(ProviderError::Cancelled);
                    break;
                }
                next = events.next() => match next {
                    Some(Ok(ev)) => yield Ok(SseFrame { event: ev.event, data: ev.data }),
                    Some(Err(e)) => {
                        yield Err(ProviderError::Transport(e.to_string()));
                        break;
                    }
                    None => break,
                },
            }
        }
    };
    Ok(Box::pin(stream))
}

/// Run a frame stream through a dialect parser and the shared
/// `StreamAccumulator`, yielding normalized `ChatEvent`s. The parser returns
/// `Ok(None)` to skip a frame (e.g. the OpenAI `[DONE]` sentinel) and
/// `Ok(Some(deltas))` otherwise.
///
/// Robustness: a malformed frame (`ProviderError::Decode`) is skipped rather
/// than aborting the turn — OpenAI-compat servers occasionally emit garbled
/// frames — with a consecutive-error cap so junk streams still fail. Transport
/// and in-band API errors stay fatal. If no frame arrives within the idle
/// timeout, the stream fails with a retryable `Transport` error.
pub fn drive(
    mut frames: BoxStream<'static, Result<SseFrame, ProviderError>>,
    mut parse: impl FnMut(&SseFrame) -> Result<Option<Vec<WireDelta>>, ProviderError>
        + Send
        + 'static,
) -> ChatStream {
    Box::pin(async_stream::stream! {
        let mut acc = StreamAccumulator::new();
        let idle = idle_timeout();
        let mut consecutive_parse_errors = 0u32;
        loop {
            let item = match tokio::time::timeout(idle, frames.next()).await {
                Ok(Some(item)) => item,
                Ok(None) => break,
                Err(_) => {
                    yield Err(ProviderError::Transport(format!(
                        "sse idle timeout: no frame within {}ms",
                        idle.as_millis()
                    )));
                    return;
                }
            };
            let frame = match item {
                Ok(f) => f,
                Err(e) => {
                    yield Err(e);
                    return;
                }
            };
            match parse(&frame) {
                Ok(None) => consecutive_parse_errors = 0,
                Ok(Some(deltas)) => {
                    consecutive_parse_errors = 0;
                    for d in deltas {
                        for ev in acc.push(d) {
                            yield Ok(ev);
                        }
                    }
                }
                Err(ProviderError::Decode(e)) => {
                    consecutive_parse_errors += 1;
                    if consecutive_parse_errors >= MAX_CONSECUTIVE_PARSE_ERRORS {
                        yield Err(ProviderError::Decode(e));
                        return;
                    }
                    tracing::warn!(error = %e, "skipping malformed sse frame");
                }
                Err(e) => {
                    yield Err(e);
                    return;
                }
            }
        }
    })
}
