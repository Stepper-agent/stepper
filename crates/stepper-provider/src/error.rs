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
            ProviderError::Api { status, .. } => *status == 429 || *status >= 500,
            _ => false,
        }
    }
}
