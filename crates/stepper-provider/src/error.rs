use thiserror::Error;

/// Failure surface common to every adapter. Kept dialect-agnostic so the core
/// agent loop can react to `Cancelled` / `Auth` / rate-limit without knowing
/// which provider produced it.
#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("http transport error: {0}")]
    Transport(String),

    #[error("api error (status {status}{}): {message}", code.as_deref().map(|c| format!(", code {c}")).unwrap_or_default())]
    Api {
        status: u16,
        code: Option<String>,
        message: String,
        /// Server-specified wait before retrying (from `Retry-After` /
        /// `x-ratelimit-reset`), when present on a 429/5xx. `None` falls back to
        /// the client's exponential backoff.
        retry_after: Option<std::time::Duration>,
    },

    #[error("failed to decode provider payload: {0}")]
    Decode(String),

    #[error("authentication error: {0}")]
    Auth(String),

    #[error("request cancelled")]
    Cancelled,

    #[error("stream ended before a terminal event")]
    UnexpectedEnd,

    #[error("unsupported by this provider: {0}")]
    Unsupported(String),
}

impl ProviderError {
    /// Whether retrying the same request might succeed (429 / 5xx / transport).
    pub fn is_retryable(&self) -> bool {
        match self {
            ProviderError::Transport(_) | ProviderError::UnexpectedEnd => true,
            // 429/5xx by status, OR a transient code on an in-band stream error
            // (those arrive on a 200 with status 0, so the status check misses them).
            ProviderError::Api { status, code, .. } => {
                *status == 429
                    || *status >= 500
                    || code.as_deref().is_some_and(is_transient_api_code)
            }
            _ => false,
        }
    }
}

/// In-band stream error codes (delivered on a 200, no HTTP status) that mark a
/// transient provider condition — retry these like a 429/5xx rather than aborting
/// the turn (and needlessly burning the fallback model).
fn is_transient_api_code(code: &str) -> bool {
    matches!(
        code,
        "overloaded_error"
            | "rate_limit_error"
            | "rate_limit_exceeded"
            | "api_error"
            | "server_error"
            | "service_unavailable"
    )
}
