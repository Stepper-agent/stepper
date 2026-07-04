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
/// first, then OpenAI's `x-ratelimit-reset-*` headers. Those reset headers use a
/// Go-style duration (`6m0s`, `88ms`, `2m59.56s`), NOT plain seconds — reading
/// only the leading integer turned `6m0s` into 6s and `88ms` into 88s. Plain
/// integer/`Ns` values still parse. HTTP-date `Retry-After` is not parsed (rare
/// on the streaming APIs); it falls back to the client's backoff.
pub(crate) fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<std::time::Duration> {
    let header = |name: &str| headers.get(name)?.to_str().ok().map(str::to_string);
    header("retry-after")
        .and_then(|v| parse_delay(&v))
        .or_else(|| header("x-ratelimit-reset-requests").and_then(|v| parse_delay(&v)))
        .or_else(|| header("x-ratelimit-reset-tokens").and_then(|v| parse_delay(&v)))
        .or_else(|| header("x-ratelimit-reset").and_then(|v| parse_delay(&v)))
}

/// Parse a rate-limit delay: a plain number is seconds (`12`, `12.5`); otherwise
/// a Go-style duration of `<number><unit>` parts with units `h`/`m`/`s`/`ms`
/// (`6m0s`, `2m59.56s`, `88ms`). Returns `None` on anything unrecognized. The
/// result is clamped to a sane ceiling by the caller's backoff, so overflow is
/// not a concern here.
fn parse_delay(raw: &str) -> Option<std::time::Duration> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    // Plain seconds (possibly fractional).
    if let Ok(secs) = s.parse::<f64>() {
        return (secs >= 0.0).then(|| std::time::Duration::from_secs_f64(secs));
    }
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut total = 0.0f64;
    let mut matched = false;
    while i < bytes.len() {
        let start = i;
        while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
            i += 1;
        }
        if i == start {
            return None; // a unit with no preceding number
        }
        let value: f64 = s[start..i].parse().ok()?;
        let seconds = match &bytes[i..] {
            [b'm', b's', ..] => {
                i += 2;
                value / 1000.0
            }
            [b'h', ..] => {
                i += 1;
                value * 3600.0
            }
            [b'm', ..] => {
                i += 1;
                value * 60.0
            }
            [b's', ..] => {
                i += 1;
                value
            }
            _ => return None,
        };
        total += seconds;
        matched = true;
    }
    matched.then(|| std::time::Duration::from_secs_f64(total))
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

    #[test]
    fn parse_delay_reads_go_style_durations_not_just_leading_digits() {
        // OpenAI reset headers are Go durations: `6m0s` is 360s, `88ms` ~0s —
        // the old leading-digit parse said 6s and 88s respectively.
        assert_eq!(parse_delay("6m0s"), Some(std::time::Duration::from_secs(360)));
        assert_eq!(parse_delay("88ms"), Some(std::time::Duration::from_millis(88)));
        assert_eq!(parse_delay("2m59.5s"), Some(std::time::Duration::from_secs_f64(179.5)));
        assert_eq!(parse_delay("1h1m1s"), Some(std::time::Duration::from_secs(3661)));
        // Plain numbers stay seconds.
        assert_eq!(parse_delay("12"), Some(std::time::Duration::from_secs(12)));
        assert_eq!(parse_delay("0.5"), Some(std::time::Duration::from_millis(500)));
        assert_eq!(parse_delay("nonsense"), None);
    }

    #[test]
    fn parse_retry_after_reads_the_reset_tokens_header_too() {
        let mut h = HeaderMap::new();
        h.insert("x-ratelimit-reset-tokens", "6m0s".parse().unwrap());
        assert_eq!(parse_retry_after(&h), Some(std::time::Duration::from_secs(360)));
    }
}
