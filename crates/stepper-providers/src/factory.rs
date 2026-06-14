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

    /// The streaming client for plain HTTPS GETs that need to FOLLOW redirects —
    /// model discovery (models.dev catalog + provider list endpoints). The auth
    /// `client()` must not be used there: its `redirect: none` policy turns a 3xx
    /// (CDN/host canonicalization) into a non-success response. Callers set their
    /// own per-request `.timeout(...)`.
    pub fn http_client(&self) -> reqwest::Client {
        self.client.clone()
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
                    // Known commercial endpoints require a key — fail closed so a
                    // keyless start surfaces the api-key prompt instead of firing
                    // an unauthorized request (a bare 401). Self-hosted / localhost
                    // openai-compat servers (oMLX, vLLM) stay keyless.
                    None if requires_api_key(&base) => {
                        return Err(ProviderError::Auth(format!(
                            "provider '{}' requires an API key (set it with /login, STEPPER_{}_API_KEY, or the OS keyring)",
                            spec.name,
                            spec.name.to_ascii_uppercase().replace('-', "_"),
                        )))
                    }
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

/// Known commercial OpenAI-compatible hosts that require an API key, so a keyless
/// build fails closed instead of sending an unauthorized request. Self-hosted and
/// localhost servers (oMLX, vLLM, a local Ollama) are intentionally excluded and
/// stay keyless.
fn requires_api_key(base_url: &str) -> bool {
    const KEYED_HOSTS: &[&str] = &["ollama.com", "api.openai.com"];
    KEYED_HOSTS.iter().any(|host| base_url.contains(host))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requires_api_key_for_commercial_hosts_only() {
        assert!(requires_api_key("https://ollama.com/v1"));
        assert!(requires_api_key("https://api.openai.com/v1"));
        assert!(!requires_api_key("http://localhost:8000/v1"));
        assert!(!requires_api_key("http://127.0.0.1:11434/v1"));
        assert!(!requires_api_key("https://my-self-hosted-vllm.internal/v1"));
    }

    #[test]
    fn openai_compat_fails_closed_without_a_key_for_commercial_host() {
        let factory = ProviderFactory::new().unwrap();
        let spec = ProviderSpec::new(ProviderKind::OpenAiCompat, "ollama-cloud", "qwen3-coder")
            .with_base_url("https://ollama.com/v1")
            .with_api_key("none");
        assert!(
            matches!(factory.build(spec), Err(ProviderError::Auth(_))),
            "keyless ollama.com must fail closed so the key prompt fires"
        );
    }

    #[test]
    fn openai_compat_stays_keyless_for_localhost() {
        let factory = ProviderFactory::new().unwrap();
        let spec = ProviderSpec::new(ProviderKind::OpenAiCompat, "omlx", "deepseek")
            .with_base_url("http://localhost:8000/v1")
            .with_api_key("none");
        assert!(factory.build(spec).is_ok(), "local servers stay keyless");
    }
}
