//! Live provider smoke tests for the two e2e backends (ollama-cloud, oMLX).
//! `#[ignore]` + env-gated: they hit REAL endpoints and only run under
//! `--ignored` with `STEPPER_E2E=1` and the relevant key/URL set, so the default
//! `cargo test` is untouched (they report as ignored). Run:
//!   STEPPER_E2E=1 STEPPER_OLLAMA_CLOUD_API_KEY=... \
//!   cargo test -p stepper-providers --test live_smoke -- --ignored --nocapture
//!   STEPPER_E2E=1 STEPPER_OMLX_BASE_URL=http://localhost:8000/v1 \
//!   cargo test -p stepper-providers --test live_smoke -- --ignored --nocapture

use futures::StreamExt;
use stepper_provider::LlmProvider;
use stepper_providers::{ChatEvent, ChatRequest, Message, ProviderFactory, ProviderKind, ProviderSpec};
use tokio_util::sync::CancellationToken;

fn e2e_on() -> bool {
    std::env::var("STEPPER_E2E").as_deref() == Ok("1")
}

async fn stream_text(provider: Box<dyn LlmProvider>, model: &str) -> (String, bool) {
    let req = ChatRequest::new(model)
        .with_messages(vec![Message::user("Reply with exactly one word: pong")]);
    let mut stream = provider
        .chat_stream(req, CancellationToken::new())
        .await
        .expect("provider opens a stream");
    let mut text = String::new();
    let mut done = false;
    while let Some(item) = stream.next().await {
        match item.expect("no stream error from the live endpoint") {
            ChatEvent::TextDelta(t) => text.push_str(&t),
            ChatEvent::Done(_) => done = true,
            _ => {}
        }
    }
    (text, done)
}

#[tokio::test]
#[ignore = "live: STEPPER_E2E=1 + STEPPER_OLLAMA_CLOUD_API_KEY"]
async fn ollama_cloud_streams_text() {
    if !e2e_on() {
        eprintln!("skip ollama_cloud_streams_text: set STEPPER_E2E=1");
        return;
    }
    let Ok(key) = std::env::var("STEPPER_OLLAMA_CLOUD_API_KEY") else {
        eprintln!("skip ollama_cloud_streams_text: STEPPER_OLLAMA_CLOUD_API_KEY unset");
        return;
    };
    let model = std::env::var("STEPPER_E2E_OLLAMA_MODEL").unwrap_or_else(|_| "qwen3-coder".into());
    let factory = ProviderFactory::new().expect("reqwest client builds");
    let provider = factory
        .build(
            ProviderSpec::new(ProviderKind::OpenAiCompat, "ollama-cloud", model.clone())
                .with_base_url("https://ollama.com/v1")
                .with_api_key(key),
        )
        .expect("build ollama-cloud provider");

    let (text, done) = stream_text(provider, &model).await;
    assert!(done, "stream must reach a Done event");
    assert!(!text.trim().is_empty(), "ollama-cloud returned no text");
}

#[tokio::test]
#[ignore = "live: STEPPER_E2E=1 + oMLX server at STEPPER_OMLX_BASE_URL"]
async fn omlx_streams_text() {
    if !e2e_on() {
        eprintln!("skip omlx_streams_text: set STEPPER_E2E=1");
        return;
    }
    let base = std::env::var("STEPPER_OMLX_BASE_URL")
        .unwrap_or_else(|_| "http://localhost:8000/v1".into());
    let model = std::env::var("STEPPER_E2E_OMLX_MODEL").unwrap_or_else(|_| "deepseek-coder".into());
    let factory = ProviderFactory::new().expect("reqwest client builds");
    let mut spec = ProviderSpec::new(ProviderKind::OpenAiCompat, "omlx", model.clone())
        .with_base_url(base);
    if let Ok(key) = std::env::var("STEPPER_OMLX_API_KEY") {
        spec = spec.with_api_key(key);
    }
    let provider = factory.build(spec).expect("build oMLX provider");

    let (text, done) = stream_text(provider, &model).await;
    assert!(done, "stream must reach a Done event");
    assert!(!text.trim().is_empty(), "oMLX returned no text");
}
