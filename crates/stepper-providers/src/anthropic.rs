use crate::auth::AuthSource;
use crate::sse;
use crate::wire;
use async_trait::async_trait;
use secrecy::ExposeSecret;
use stepper_provider::{ChatRequest, ChatStream, LlmProvider, ProviderError};
use tokio_util::sync::CancellationToken;

/// Anthropic Messages adapter. `base_url` is the host only (no `/v1`); the
/// adapter appends `/v1/messages`.
pub struct AnthropicAdapter {
    client: reqwest::Client,
    provider_name: String,
    base_url: String,
    model: String,
    auth: AuthSource,
}

impl AnthropicAdapter {
    pub fn new(
        client: reqwest::Client,
        provider_name: impl Into<String>,
        base_url: impl Into<String>,
        model: impl Into<String>,
        auth: AuthSource,
    ) -> Self {
        AnthropicAdapter {
            client,
            provider_name: provider_name.into(),
            base_url: base_url.into(),
            model: model.into(),
            auth,
        }
    }
}

#[async_trait]
impl LlmProvider for AnthropicAdapter {
    fn provider(&self) -> &str {
        &self.provider_name
    }

    fn model(&self) -> &str {
        &self.model
    }

    async fn chat_stream(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<ChatStream, ProviderError> {
        let body = wire::anthropic::build_request_body(&request, &self.model, true);
        let url = format!("{}/v1/messages", self.base_url.trim_end_matches('/'));

        let mut rb = self
            .client
            .post(url)
            .header("anthropic-version", wire::anthropic::version())
            .json(&body);
        match &self.auth {
            AuthSource::ApiKey(key) => rb = rb.header("x-api-key", key.expose_secret()),
            AuthSource::None => {}
            AuthSource::Codex(_) => {
                return Err(ProviderError::Unsupported(
                    "codex auth is not valid for the anthropic adapter".into(),
                ));
            }
        }

        let resp = sse::send(rb).await?;
        let frames = sse::into_frames(resp, cancel).await?;
        Ok(sse::drive(frames, |frame| {
            wire::anthropic::parse_event(&frame.event, &frame.data).map(Some)
        }))
    }
}
