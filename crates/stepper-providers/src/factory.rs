use crate::anthropic::AnthropicAdapter;
use crate::auth::{resolve_key, AuthSource};
use crate::codex::{CodexTokenStore, CODEX_BASE_URL};
use crate::openai_compat::OpenAiCompatAdapter;
use crate::responses::OpenAiResponsesAdapter;
use crate::error;
use std::time::Duration;
use stepper_config::ProxyConfig;
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
        Self::with_proxy(None)
    }

    /// Like `new`, but routes both clients through an explicit `proxy` from
    /// config (`None` keeps reqwest's `HTTP(S)_PROXY`/`NO_PROXY` env default).
    pub fn with_proxy(proxy: Option<&ProxyConfig>) -> Result<Self, ProviderError> {
        // The streaming client deliberately carries no overall `.timeout()` — it
        // would kill long-lived SSE streams. But a `read_timeout` bounds the gap
        // BETWEEN reads (it resets on every chunk), so it never kills an active
        // stream yet caps a provider that accepts the socket and then stalls —
        // before sending response headers, on a non-2xx body, or mid-stream. That
        // stall was otherwise unbounded (the SSE idle timeout in `sse::drive` only
        // runs once frames are flowing) and hung the whole turn with no way out.
        let client = apply_proxy(apply_extra_ca(reqwest::Client::builder()), proxy)
            .user_agent(concat!("stepper/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(30))
            .read_timeout(Duration::from_secs(120))
            .tcp_keepalive(Duration::from_secs(60))
            .pool_idle_timeout(Duration::from_secs(90))
            .build()
            .map_err(error::transport)?;
        let auth_client = apply_proxy(apply_extra_ca(reqwest::Client::builder()), proxy)
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

/// Merge a private CA bundle (pointed at by `STEPPER_EXTRA_CA_CERTS`, falling back
/// to `NODE_EXTRA_CA_CERTS` for opencode/Node parity) into the platform trust
/// store, so corporate-proxy / self-signed TLS endpoints validate. The certs are
/// ADDITIVE — system roots still apply. Fail-open: an unset var, unreadable file,
/// or unparsable bundle leaves the builder untouched (system trust only). Proxy
/// support needs no code here: reqwest already honors `HTTP(S)_PROXY`/`NO_PROXY`.
pub(crate) fn apply_extra_ca(builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    let Some(path) =
        std::env::var_os("STEPPER_EXTRA_CA_CERTS").or_else(|| std::env::var_os("NODE_EXTRA_CA_CERTS"))
    else {
        return builder;
    };
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!("extra CA certificates: cannot read {path:?}: {err}");
            return builder;
        }
    };
    let certs = match reqwest::Certificate::from_pem_bundle(&bytes) {
        Ok(certs) => certs,
        Err(err) => {
            tracing::warn!("extra CA certificates: cannot parse {path:?}: {err}");
            return builder;
        }
    };
    // rustls-platform-verifier can merge EXTRA roots only on these targets; on any
    // other target a non-empty root set makes `.build()` error, so stay on system
    // trust there — keeping the fail-open total (a valid cert never aborts startup).
    #[cfg(any(all(unix, not(target_os = "android")), target_os = "windows"))]
    {
        builder.tls_certs_merge(certs)
    }
    #[cfg(not(any(all(unix, not(target_os = "android")), target_os = "windows")))]
    {
        let _ = certs;
        builder
    }
}

/// Apply an explicit proxy from config. `None`/inactive leaves the builder
/// untouched so reqwest's `HTTP(S)_PROXY`/`NO_PROXY` env default applies. An
/// explicit proxy REPLACES the env proxy (reqwest disables env auto-proxy once a
/// `.proxy()`/`.no_proxy()` is set); `disabled: true` forces a direct connection.
/// Fail-open: a malformed proxy URL is logged and skipped, never aborting startup.
pub(crate) fn apply_proxy(mut builder: reqwest::ClientBuilder, proxy: Option<&ProxyConfig>) -> reqwest::ClientBuilder {
    let Some(proxy) = proxy.filter(|p| p.is_active()) else {
        return builder;
    };
    if proxy.disabled {
        return builder.no_proxy();
    }
    let no_proxy = || proxy.no_proxy.as_deref().and_then(reqwest::NoProxy::from_string);
    // reqwest uses the FIRST added proxy whose scheme matches, so register the
    // scheme-specific ones before the catch-all `all` (which matches everything) —
    // otherwise a configured `all` would shadow `http`/`https` (dead config).
    if let Some(url) = proxy.http.as_deref() {
        match reqwest::Proxy::http(url) {
            Ok(p) => builder = builder.proxy(p.no_proxy(no_proxy())),
            Err(e) => tracing::warn!("proxy: ignoring invalid `http` proxy {url:?}: {e}"),
        }
    }
    if let Some(url) = proxy.https.as_deref() {
        match reqwest::Proxy::https(url) {
            Ok(p) => builder = builder.proxy(p.no_proxy(no_proxy())),
            Err(e) => tracing::warn!("proxy: ignoring invalid `https` proxy {url:?}: {e}"),
        }
    }
    if let Some(url) = proxy.all.as_deref() {
        match reqwest::Proxy::all(url) {
            Ok(p) => builder = builder.proxy(p.no_proxy(no_proxy())),
            Err(e) => tracing::warn!("proxy: ignoring invalid `all` proxy {url:?}: {e}"),
        }
    }
    builder
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
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

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
        // `ProviderFactory::new()` reads the extra-CA env vars (via apply_extra_ca);
        // share ENV_LOCK with the CA tests so their `set_var` never races this read.
        let _guard = ENV_LOCK.lock().unwrap();
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
        let _guard = ENV_LOCK.lock().unwrap();
        let factory = ProviderFactory::new().unwrap();
        let spec = ProviderSpec::new(ProviderKind::OpenAiCompat, "omlx", "deepseek")
            .with_base_url("http://localhost:8000/v1")
            .with_api_key("none");
        assert!(factory.build(spec).is_ok(), "local servers stay keyless");
    }

    const TEST_CA_PEM: &[u8] = b"-----BEGIN CERTIFICATE-----\n\
MIIDFTCCAf2gAwIBAgIUMSUV9547Jfma6vu7vYDogv9TiOMwDQYJKoZIhvcNAQEL\n\
BQAwGjEYMBYGA1UEAwwPc3RlcHBlci10ZXN0LWNhMB4XDTI2MDYyMDA5MzY0MFoX\n\
DTM2MDYxNzA5MzY0MFowGjEYMBYGA1UEAwwPc3RlcHBlci10ZXN0LWNhMIIBIjAN\n\
BgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAqierzTtp4J+rxZUJ3iXGUn+SlEez\n\
3z7QkfQ3h8QAC/PJ7MFvnirRxe3ePI8dWKyiroJhiQM70oCLGzqJHbj/l/fBQc8g\n\
wBc12Auz9m8GmoV9OH5p60Qyiz39q/XeMwspuVffPK4xSI4DExrpikIBAH51nIEq\n\
EyzvtsLnRuPwzjanl6ki5BlWsqaSdTOwZr5q60acnUYhw/mEQvSqxar89Rk8RGxs\n\
VZ6+DjE7lNI+CahOp9c1YoXO+jQCTiYm/q/+VTaYJo3CuhqmAIhh1AGbywcQ1L7I\n\
M6LSUoaTi02zoCsuIj2wYdSTe9ZT0brdUPpLYAzn5wPqeWZEKJlRnAbMzQIDAQAB\n\
o1MwUTAdBgNVHQ4EFgQUyaSIE3OuTwwo1Z0ouIpekZv8HPUwHwYDVR0jBBgwFoAU\n\
yaSIE3OuTwwo1Z0ouIpekZv8HPUwDwYDVR0TAQH/BAUwAwEB/zANBgkqhkiG9w0B\n\
AQsFAAOCAQEAfpfBYCAEV+L457kWTZ7kJpl/Cuw8a+gM5RPOnonw44rtjj3L3F9O\n\
ppAf7UIFVfeniC6gr8IN2xk/+r8BoeekLHGYPLHtPfWqXpB8f17ljH9t5484a4WU\n\
RHrp+fJyaELOaJZnp/wycM+3ExUSaa3eEsfDcQwU2PJXnwZ/uL7SnjsR2dY50XNG\n\
Iuuy1CGORrwI3TMrz+g4qr2ml6R+bZIMT/y9yMLKgJVHwuhPnSd2d61l67w/69sj\n\
FzpnbagaS7HkaUN0ZQ5IY/Jtedf91V10M1Ci5edHmNo5K2ihL3krZ+aFqaAXi/Hk\n\
IASKmoilz1GrAGEFfwEnc0L5PrhPs/eX8Q==\n\
-----END CERTIFICATE-----\n";

    fn build(builder: reqwest::ClientBuilder) -> bool {
        builder.build().is_ok()
    }

    #[test]
    fn apply_proxy_is_noop_disabled_and_explicit_all_build_ok() {
        // None → untouched (env auto-proxy preserved).
        assert!(build(apply_proxy(reqwest::Client::builder(), None)));
        // An all-None config is inactive → also a no-op.
        let inactive = ProxyConfig::default();
        assert!(!inactive.is_active());
        assert!(build(apply_proxy(reqwest::Client::builder(), Some(&inactive))));
        // disabled → forced direct connection.
        let disabled = ProxyConfig { disabled: true, ..Default::default() };
        assert!(disabled.is_active());
        assert!(build(apply_proxy(reqwest::Client::builder(), Some(&disabled))));
        // A valid explicit proxy (with a noProxy bypass) builds.
        let explicit = ProxyConfig {
            all: Some("http://127.0.0.1:8080".into()),
            no_proxy: Some("localhost,127.0.0.1".into()),
            ..Default::default()
        };
        assert!(build(apply_proxy(reqwest::Client::builder(), Some(&explicit))));
        // Separate http/https entries also build.
        let split = ProxyConfig {
            http: Some("http://proxy.internal:3128".into()),
            https: Some("http://proxy.internal:3128".into()),
            ..Default::default()
        };
        assert!(build(apply_proxy(reqwest::Client::builder(), Some(&split))));
    }

    #[test]
    fn apply_proxy_fails_open_on_a_garbage_url() {
        // A malformed proxy URL is skipped (logged), never aborting the build.
        let garbage = ProxyConfig { all: Some("not a url".into()), ..Default::default() };
        assert!(build(apply_proxy(reqwest::Client::builder(), Some(&garbage))));
    }

    #[test]
    fn from_pem_bundle_parses_the_test_ca() {
        let certs = reqwest::Certificate::from_pem_bundle(TEST_CA_PEM).unwrap();
        assert_eq!(certs.len(), 1, "the bundle holds exactly one CA cert");
    }

    #[test]
    fn extra_ca_is_a_noop_without_the_env() {
        let _guard = ENV_LOCK.lock().unwrap();
        let prev = (
            std::env::var_os("STEPPER_EXTRA_CA_CERTS"),
            std::env::var_os("NODE_EXTRA_CA_CERTS"),
        );
        unsafe {
            std::env::remove_var("STEPPER_EXTRA_CA_CERTS");
            std::env::remove_var("NODE_EXTRA_CA_CERTS");
        }
        assert!(build(apply_extra_ca(reqwest::Client::builder())));
        unsafe {
            restore("STEPPER_EXTRA_CA_CERTS", prev.0);
            restore("NODE_EXTRA_CA_CERTS", prev.1);
        }
    }

    #[test]
    fn extra_ca_loads_a_valid_bundle_and_falls_open_on_garbage() {
        let _guard = ENV_LOCK.lock().unwrap();
        let prev = (
            std::env::var_os("STEPPER_EXTRA_CA_CERTS"),
            std::env::var_os("NODE_EXTRA_CA_CERTS"),
        );
        let dir = std::env::temp_dir();
        let good = dir.join("stepper-test-ca.pem");
        std::fs::write(&good, TEST_CA_PEM).unwrap();
        let garbage = dir.join("stepper-test-garbage.pem");
        std::fs::write(&garbage, b"not a certificate").unwrap();
        unsafe {
            std::env::remove_var("NODE_EXTRA_CA_CERTS");
            std::env::set_var("STEPPER_EXTRA_CA_CERTS", &good);
        }
        assert!(build(apply_extra_ca(reqwest::Client::builder())), "valid bundle builds");
        unsafe { std::env::set_var("STEPPER_EXTRA_CA_CERTS", &garbage) };
        assert!(build(apply_extra_ca(reqwest::Client::builder())), "garbage falls open");
        unsafe { std::env::set_var("STEPPER_EXTRA_CA_CERTS", dir.join("does-not-exist.pem")) };
        assert!(build(apply_extra_ca(reqwest::Client::builder())), "missing file falls open");
        let _ = std::fs::remove_file(&good);
        let _ = std::fs::remove_file(&garbage);
        unsafe {
            restore("STEPPER_EXTRA_CA_CERTS", prev.0);
            restore("NODE_EXTRA_CA_CERTS", prev.1);
        }
    }

    unsafe fn restore(key: &str, prev: Option<std::ffi::OsString>) {
        match prev {
            Some(v) => unsafe { std::env::set_var(key, v) },
            None => unsafe { std::env::remove_var(key) },
        }
    }
}
