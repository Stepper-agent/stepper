use crate::auth::AuthSource;
use crate::sse;
use crate::wire;
use async_trait::async_trait;
use secrecy::ExposeSecret;
use serde_json::Value;
use stepper_provider::{ChatRequest, ChatStream, LlmProvider, ProviderError};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const ORIGINATOR: &str = "codex_cli_rs";
const USER_AGENT: &str = "codex_cli_rs/0.0.0";

/// Minimal Codex-style system prompt. The Codex backend rejects requests whose
/// `instructions` are missing or not Codex-shaped, so this is used only when the
/// caller supplies no system prompt of its own.
const DEFAULT_CODEX_INSTRUCTIONS: &str =
    "You are Codex, based on GPT-5, a coding agent running in the stepper CLI. \
You help the user with software engineering tasks by reading and editing files \
and running commands through the provided tools.";

/// OpenAI Responses adapter. With `AuthSource::ApiKey` it talks to the public
/// Responses endpoint; with `AuthSource::Codex` it injects the ChatGPT-OAuth
/// headers, forces `store:false`, and retries once on a 401 after refreshing.
pub struct OpenAiResponsesAdapter {
    client: reqwest::Client,
    provider_name: String,
    base_url: String,
    model: String,
    auth: AuthSource,
}

impl OpenAiResponsesAdapter {
    pub fn new(
        client: reqwest::Client,
        provider_name: impl Into<String>,
        base_url: impl Into<String>,
        model: impl Into<String>,
        auth: AuthSource,
    ) -> Self {
        OpenAiResponsesAdapter {
            client,
            provider_name: provider_name.into(),
            base_url: base_url.into(),
            model: model.into(),
            auth,
        }
    }

    fn is_codex(&self) -> bool {
        matches!(self.auth, AuthSource::Codex(_))
    }

    fn url(&self) -> String {
        format!("{}/responses", self.base_url.trim_end_matches('/'))
    }

    async fn build_request(&self, body: &Value) -> Result<reqwest::RequestBuilder, ProviderError> {
        let mut rb = self
            .client
            .post(self.url())
            .header("accept", "text/event-stream")
            .json(body);
        match &self.auth {
            AuthSource::ApiKey(key) => rb = rb.bearer_auth(key.expose_secret()),
            AuthSource::None => {
                return Err(ProviderError::Auth(
                    "the responses adapter requires an api key or codex auth".into(),
                ));
            }
            AuthSource::Codex(store) => {
                let (access, account_id) = store.bearer().await?;
                rb = rb
                    .bearer_auth(access)
                    .header("chatgpt-account-id", account_id)
                    .header("openai-beta", "responses=experimental")
                    .header("originator", ORIGINATOR)
                    .header("user-agent", USER_AGENT)
                    .header("session-id", Uuid::new_v4().to_string());
            }
        }
        Ok(rb)
    }
}

#[async_trait]
impl LlmProvider for OpenAiResponsesAdapter {
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
        let mut request = request;
        let codex = self.is_codex();
        if codex && request.system.as_deref().unwrap_or("").trim().is_empty() {
            request.system = Some(DEFAULT_CODEX_INSTRUCTIONS.to_string());
        }

        let body = wire::responses::build_request_body(&request, &self.model, true, !codex);

        let rb = self.build_request(&body).await?;
        let mut resp = sse::send(rb).await?;

        if codex && resp.status().as_u16() == 401 {
            if let AuthSource::Codex(store) = &self.auth {
                store.force_refresh().await?;
            }
            let rb = self.build_request(&body).await?;
            resp = sse::send(rb).await?;
        }

        let frames = sse::into_frames(resp, cancel).await?;
        Ok(sse::drive(frames, |frame| {
            wire::responses::parse_event(&frame.event, &frame.data).map(Some)
        }))
    }
}
