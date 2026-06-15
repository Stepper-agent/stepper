use serde_json::Value;
use stepper_provider::ProviderError;

/// Turn a non-2xx response body into a structured `ProviderError::Api`, pulling
/// the message/code out of the three error envelopes we see: OpenAI
/// `{error:{message,code,type}}`, Anthropic `{error:{type,message}}`, and the
/// Codex backend `{detail:"..."}`.
pub(crate) fn api_error_from_body(status: u16, body: &str) -> ProviderError {
    let (code, message) = extract_error(body);
    ProviderError::Api {
        status,
        code,
        message: message.unwrap_or_else(|| truncate(body, 500)),
        retry_after: None,
    }
}

/// Extract a retry delay from rate-limit headers: `Retry-After` (delta-seconds)
/// first, then `x-ratelimit-reset-requests` / `x-ratelimit-reset` (OpenAI emits
/// leading integer seconds). HTTP-date `Retry-After` is not parsed (rare on the
/// streaming APIs); it falls back to the client's backoff.
pub(crate) fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<std::time::Duration> {
    let leading_secs = |name: &str| -> Option<u64> {
        let raw = headers.get(name)?.to_str().ok()?;
        let digits: String = raw.trim().chars().take_while(|c| c.is_ascii_digit()).collect();
        digits.parse::<u64>().ok()
    };
    leading_secs("retry-after")
        .or_else(|| leading_secs("x-ratelimit-reset-requests"))
        .or_else(|| leading_secs("x-ratelimit-reset"))
        .map(std::time::Duration::from_secs)
}

fn extract_error(body: &str) -> (Option<String>, Option<String>) {
    let Ok(v) = serde_json::from_str::<Value>(body) else {
        return (None, None);
    };
    if let Some(err) = v.get("error") {
        let message = err
            .get("message")
            .and_then(Value::as_str)
            .map(String::from);
        let code = err
            .get("code")
            .and_then(Value::as_str)
            .map(String::from)
            .or_else(|| err.get("type").and_then(Value::as_str).map(String::from));
        return (code, message);
    }
    if let Some(detail) = v.get("detail") {
        let message = detail
            .as_str()
            .map(String::from)
            .or_else(|| Some(detail.to_string()));
        return (None, message);
    }
    (None, None)
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}

pub(crate) fn transport(e: impl std::fmt::Display) -> ProviderError {
    ProviderError::Transport(e.to_string())
}

pub(crate) fn decode(e: impl std::fmt::Display) -> ProviderError {
    ProviderError::Decode(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderMap;

    #[test]
    fn parse_retry_after_reads_delta_seconds() {
        let mut h = HeaderMap::new();
        h.insert("retry-after", "7".parse().unwrap());
        assert_eq!(parse_retry_after(&h), Some(std::time::Duration::from_secs(7)));
    }

    #[test]
    fn parse_retry_after_falls_back_to_ratelimit_reset() {
        let mut h = HeaderMap::new();
        h.insert("x-ratelimit-reset-requests", "12s".parse().unwrap());
        assert_eq!(parse_retry_after(&h), Some(std::time::Duration::from_secs(12)));
    }

    #[test]
    fn parse_retry_after_none_when_absent_or_unparseable() {
        assert_eq!(parse_retry_after(&HeaderMap::new()), None);
        let mut h = HeaderMap::new();
        h.insert("retry-after", "Wed, 21 Oct 2015 07:28:00 GMT".parse().unwrap());
        assert_eq!(parse_retry_after(&h), None, "http-date is not parsed");
    }
}
