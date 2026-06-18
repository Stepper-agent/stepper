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
    /// Ollama truncates the prompt to its default `num_ctx` (~4096) and silently
    /// drops the oldest tokens — system prompt + prior turns — unless `num_ctx` is
    /// sent in the request. Set for Ollama endpoints so the request carries it.
    is_ollama: bool,
}

impl OpenAiCompatAdapter {
    pub fn new(
        client: reqwest::Client,
        provider_name: impl Into<String>,
        base_url: impl Into<String>,
        model: impl Into<String>,
        auth: AuthSource,
    ) -> Self {
        let provider_name = provider_name.into();
        let base_url = base_url.into();
        let is_ollama = is_ollama_endpoint(&provider_name, &base_url);
        OpenAiCompatAdapter {
            client,
            provider_name,
            base_url,
            model: model.into(),
            auth,
            is_ollama,
        }
    }
}

/// Whether this endpoint is Ollama (local or cloud), by provider name or URL.
/// Only Ollama reads `num_ctx`; oMLX/vLLM/OpenAI must not receive it.
fn is_ollama_endpoint(provider_name: &str, base_url: &str) -> bool {
    provider_name.contains("ollama")
        || base_url.contains("ollama")
        || base_url.contains(":11434")
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
        let mut body = wire::openai::build_request_body(&request, &self.model, true);
        // Ollama caps the runtime context at its default (~4096) and silently
        // truncates the oldest tokens unless `num_ctx` is sent — without this the
        // system prompt and prior turns fall out of the window and the agent
        // forgets the conversation. Align it with the window stepper plans against.
        if self.is_ollama
            && let Some(ctx) = request.context_window
            && let Some(obj) = body.as_object_mut()
        {
            obj.insert("num_ctx".into(), serde_json::json!(ctx));
        }
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

#[cfg(test)]
mod tests {
    use super::is_ollama_endpoint;

    #[test]
    fn detects_ollama_by_name_or_url_but_not_other_openai_compat() {
        assert!(is_ollama_endpoint("ollama", "http://localhost:11434/v1"));
        assert!(is_ollama_endpoint("my-llm", "http://localhost:11434/v1"));
        assert!(is_ollama_endpoint("ollama-cloud", "https://ollama.com/v1"));
        // oMLX / vLLM / OpenAI must NOT be treated as Ollama (they reject num_ctx).
        assert!(!is_ollama_endpoint("omlx", "http://localhost:8000/v1"));
        assert!(!is_ollama_endpoint("openai", "https://api.openai.com/v1"));
        assert!(!is_ollama_endpoint("vllm", "https://vllm.internal/v1"));
    }
}
