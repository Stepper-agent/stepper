use crate::anthropic::AnthropicAdapter;
use crate::auth::{resolve_key, AuthSource};
use crate::codex::{CodexTokenStore, CODEX_BASE_URL};
use crate::openai_compat::OpenAiCompatAdapter;
use crate::responses::OpenAiResponsesAdapter;
use crate::error;
use std::time::Duration;
use stepper_provider::{LlmProvider, ProviderError};

/// Which dialect an adapter speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    /// OpenAI / Ollama Cloud / oMLX (`/v1/chat/completions`).
    OpenAiCompat,
    /// Anthropic Messages.
    Anthropic,
    /// OpenAI Responses with an api key.
    OpenAiResponses,
    /// OpenAI Responses via ChatGPT-OAuth (Codex backend).
    Codex,
}

/// Everything needed to instantiate one provider. `base_url` defaults per kind;
/// `api_key` is the explicit key (else env/keyring); `codex_store` is required
/// for `Codex`.
pub struct ProviderSpec {
    pub kind: ProviderKind,
    pub name: String,
    pub model: String,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub codex_store: Option<CodexTokenStore>,
}

impl ProviderSpec {
    pub fn new(kind: ProviderKind, name: impl Into<String>, model: impl Into<String>) -> Self {
        ProviderSpec {
            kind,
            name: name.into(),
            model: model.into(),
            base_url: None,
            api_key: None,
            codex_store: None,
        }
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = Some(base_url.into());
        self
    }

    pub fn with_api_key(mut self, key: impl Into<String>) -> Self {
        self.api_key = Some(key.into());
        self
    }

    pub fn with_codex_store(mut self, store: CodexTokenStore) -> Self {
        self.codex_store = Some(store);
        self
    }
}

/// Owns the shared streaming `reqwest::Client` (connection pool) plus a
/// dedicated auth/token-endpoint client, and builds adapters.
#[derive(Clone)]
pub struct ProviderFactory {
    client: reqwest::Client,
    auth_client: reqwest::Client,
}

impl ProviderFactory {
    pub fn new() -> Result<Self, ProviderError> {
        // The streaming client deliberately carries no overall `.timeout()` —
        // it would kill long-lived SSE streams; mid-stream stalls are covered
        // by the SSE idle timeout in `sse::drive`.
        let client = reqwest::Client::builder()
            .user_agent(concat!("stepper/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(30))
            .tcp_keepalive(Duration::from_secs(60))
            .pool_idle_timeout(Duration::from_secs(90))
            .build()
            .map_err(error::transport)?;
        let auth_client = reqwest::Client::builder()
            .user_agent(concat!("stepper/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(30))
            .timeout(crate::codex::auth_http_timeout())
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(error::transport)?;
        Ok(ProviderFactory { client, auth_client })
    }

    /// The dedicated client for OAuth/token endpoints (Codex login + refresh):
    /// never follows redirects — the fixed https `TOKEN_URL` over TLS is the
    /// sole trust anchor for the decode-only id_token — and carries an overall
    /// deadline. Not for streaming chat requests.
    pub fn client(&self) -> reqwest::Client {
        self.auth_client.clone()
    }

    pub fn build(&self, spec: ProviderSpec) -> Result<Box<dyn LlmProvider>, ProviderError> {
        let client = self.client.clone();
        match spec.kind {
            ProviderKind::OpenAiCompat => {
                let base = spec
                    .base_url
                    .unwrap_or_else(|| "https://api.openai.com/v1".into());
                let auth = match resolve_key(&spec.name, spec.api_key.as_deref()) {
                    Some(key) => AuthSource::ApiKey(key),
                    None => AuthSource::None,
                };
                Ok(Box::new(OpenAiCompatAdapter::new(
                    client, spec.name, base, spec.model, auth,
                )))
            }
            ProviderKind::Anthropic => {
                let base = spec
                    .base_url
                    .unwrap_or_else(|| "https://api.anthropic.com".into());
                let key = resolve_key(&spec.name, spec.api_key.as_deref()).ok_or_else(|| {
                    ProviderError::Auth(format!("no api key for anthropic provider '{}'", spec.name))
                })?;
                Ok(Box::new(AnthropicAdapter::new(
                    client,
                    spec.name,
                    base,
                    spec.model,
                    AuthSource::ApiKey(key),
                )))
            }
            ProviderKind::OpenAiResponses => {
                let base = spec
                    .base_url
                    .unwrap_or_else(|| "https://api.openai.com/v1".into());
                let key = resolve_key(&spec.name, spec.api_key.as_deref()).ok_or_else(|| {
                    ProviderError::Auth(format!("no api key for responses provider '{}'", spec.name))
                })?;
                Ok(Box::new(OpenAiResponsesAdapter::new(
                    client,
                    spec.name,
                    base,
                    spec.model,
                    AuthSource::ApiKey(key),
                )))
            }
            ProviderKind::Codex => {
                let store = spec.codex_store.ok_or_else(|| {
                    ProviderError::Auth(
                        "codex provider needs a token store — run `stepper auth login --codex`"
                            .into(),
                    )
                })?;
                let base = spec.base_url.unwrap_or_else(|| CODEX_BASE_URL.into());
                Ok(Box::new(OpenAiResponsesAdapter::new(
                    client,
                    spec.name,
                    base,
                    spec.model,
                    AuthSource::Codex(store),
                )))
            }
        }
    }
}
