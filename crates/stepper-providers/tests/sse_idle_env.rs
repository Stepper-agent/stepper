//! `STEPPER_SSE_IDLE_TIMEOUT_MS` env override for the SSE idle timeout. Kept in
//! its own test binary: the env var is process-global, and the other SSE tests
//! must keep seeing the 120s default.

use futures::stream::BoxStream;
use futures::StreamExt;
use stepper_providers::sse::{self, SseFrame};
use stepper_providers::ProviderError;

#[tokio::test]
async fn idle_timeout_is_tunable_via_env_and_fires_in_real_time() {
    // SAFETY: single test in this binary setting a process env var it then removes.
    unsafe {
        std::env::set_var("STEPPER_SSE_IDLE_TIMEOUT_MS", "100");
    }

    let stalled: BoxStream<'static, Result<SseFrame, ProviderError>> =
        Box::pin(futures::stream::pending());
    let mut events = sse::drive(stalled, |_frame| Ok(None));

    let start = std::time::Instant::now();
    let item = events.next().await;
    let elapsed = start.elapsed();

    unsafe {
        std::env::remove_var("STEPPER_SSE_IDLE_TIMEOUT_MS");
    }

    match item {
        Some(Err(ProviderError::Transport(m))) => {
            assert!(m.contains("idle timeout"), "got: {m}");
            assert!(m.contains("100ms"), "the message reflects the env value: {m}");
        }
        other => panic!("expected the idle timeout error, got {other:?}"),
    }
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "the 100ms override must fire fast, took {elapsed:?}"
    );
}
