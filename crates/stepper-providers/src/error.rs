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
    }
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
