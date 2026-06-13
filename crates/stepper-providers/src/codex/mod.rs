//! Codex "Sign in with ChatGPT" OAuth. Constants verified against the canonical
//! `openai/codex` Rust source (see `docs/acknowledge/decisions.md`). This is an
//! undocumented private endpoint and single-user own-account only.

pub mod oauth;
pub mod store;

pub use store::CodexTokenStore;

/// Public OAuth client id hardcoded in Codex (`login/src/auth/manager.rs`).
pub(crate) const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub(crate) const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
pub(crate) const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";

/// OpenAI's redirect-URI allow-list only accepts these ports; an arbitrary port
/// will be rejected.
pub(crate) const DEFAULT_PORT: u16 = 1455;
pub(crate) const FALLBACK_PORT: u16 = 1457;

pub(crate) const SCOPE: &str =
    "openid profile email offline_access api.connectors.read api.connectors.invoke";

/// Refresh the access token when it expires within this many seconds, matching
/// Codex's `CHATGPT_ACCESS_TOKEN_REFRESH_WINDOW_MINUTES = 5`.
pub(crate) const REFRESH_WINDOW_SECS: u64 = 300;

/// The Codex backend that ChatGPT-OAuth tokens are allowed to call.
pub const CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";

/// Overall deadline for token exchange/refresh against `auth.openai.com` — a
/// stalled auth endpoint must not hang login or mid-turn refresh forever.
/// Override with `STEPPER_CODEX_AUTH_TIMEOUT_MS`.
pub(crate) fn auth_http_timeout() -> std::time::Duration {
    let ms = std::env::var("STEPPER_CODEX_AUTH_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(30_000);
    std::time::Duration::from_millis(ms)
}
