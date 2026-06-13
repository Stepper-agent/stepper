use crate::auth::AuthSource;
use crate::sse;
use crate::wire;
use async_trait::async_trait;
use secrecy::ExposeSecret;
use stepper_provider::{ChatRequest, ChatStream, LlmProvider, ProviderError};
use tokio_util::sync::CancellationToken;

/// OpenAI Chat Completions adapter, reused for OpenAI, Ollama Cloud, and oMLX —
/// they differ only by `base_url` and auth. `base_url` must already include the
/// `/v1` segment (e.g. `https://ollama.com/v1`, `http://localhost:8000/v1`).
pub struct OpenAiCompatAdapter {
    client: reqwest::Client,
    provider_name: String,
    base_url: String,
    model: String,
    auth: AuthSource,
}

impl OpenAiCompatAdapter {
    pub fn new(
        client: reqwest::Client,
        provider_name: impl Into<String>,
        base_url: impl Into<String>,
        model: impl Into<String>,
        auth: AuthSource,
    ) -> Self {
        OpenAiCompatAdapter {
            client,
            provider_name: provider_name.into(),
            base_url: base_url.into(),
            model: model.into(),
            auth,
        }
    }
}

#[async_trait]
impl LlmProvider for OpenAiCompatAdapter {
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
        let body = wire::openai::build_request_body(&request, &self.model, true);
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));

        let mut rb = self.client.post(url).json(&body);
        if let AuthSource::ApiKey(key) = &self.auth {
            rb = rb.bearer_auth(key.expose_secret());
        }

        let resp = sse::send(rb).await?;
        let frames = sse::into_frames(resp, cancel).await?;
        Ok(sse::drive(frames, |frame| {
            if frame.data.trim() == "[DONE]" {
                Ok(None)
            } else {
                wire::openai::parse_chunk(&frame.data).map(Some)
            }
        }))
    }
}
