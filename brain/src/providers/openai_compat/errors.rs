//! HTTP statuses and error bodies → [`BrainError`]. Error text never contains the key.
//!
//! OpenAI-style bodies are `{"error": {"message", "type", "code"}}`; some servers send
//! `{"error": "message"}` instead.

use nixxa_contracts::BrainError;
use serde_json::Value;

use crate::providers::common::scrub;

/// A non-200 response.
pub(crate) fn from_status(status: u16, retry_after: Option<&str>, body: &[u8], key: &str) -> BrainError {
    let error = serde_json::from_slice::<Value>(body).map(|v| v["error"].clone()).unwrap_or(Value::Null);
    let message = scrub(&message_of(&error).unwrap_or_else(|| format!("HTTP {status}")), key);
    match status {
        401 | 403 => BrainError::Auth(message),
        429 => BrainError::RateLimited { retry_after_ms: retry_after.and_then(parse_retry_after) },
        413 => BrainError::ContextTooLarge,
        400 if is_context_too_long(&message) => BrainError::ContextTooLarge,
        404 => BrainError::Provider(format!("not found (HTTP 404): {message}")),
        _ => BrainError::Provider(format!("HTTP {status}: {message}")),
    }
}

/// An `error` object inside the stream.
pub(crate) fn from_stream_error(error: &Value, key: &str) -> BrainError {
    let message = scrub(&message_of(error).unwrap_or_else(|| "no message".into()), key);
    if is_context_too_long(&message) { BrainError::ContextTooLarge } else { BrainError::Provider(message) }
}

fn message_of(error: &Value) -> Option<String> {
    error.as_str().or_else(|| error["message"].as_str()).map(str::to_string)
}

/// A 400 that means the prompt doesn't fit the model's context window.
fn is_context_too_long(message: &str) -> bool {
    let message = message.to_lowercase();
    ["context length", "context window", "maximum context", "too many tokens", "prompt is too long"]
        .iter()
        .any(|phrase| message.contains(phrase))
}

fn parse_retry_after(value: &str) -> Option<u64> {
    let seconds: f64 = value.trim().parse().ok()?;
    (seconds.is_finite() && seconds >= 0.0).then(|| (seconds * 1000.0).round() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "sk-or-test-key-1234567890";

    #[test]
    fn both_error_shapes_and_every_status_map() {
        let openai =
            br#"{"error":{"message":"Incorrect API key","type":"invalid_request_error","code":"invalid_api_key"}}"#;
        assert_eq!(from_status(401, None, openai, KEY), BrainError::Auth("Incorrect API key".into()));
        let ollama = br#"{"error":"model \"qwen9\" not found, try pulling it first"}"#;
        assert_eq!(
            from_status(404, None, ollama, KEY),
            BrainError::Provider("not found (HTTP 404): model \"qwen9\" not found, try pulling it first".into())
        );
        assert_eq!(from_status(429, Some("3"), b"", KEY), BrainError::RateLimited { retry_after_ms: Some(3000) });
        assert_eq!(from_status(413, None, b"", KEY), BrainError::ContextTooLarge);
        let too_long = br#"{"error":{"message":"This model's maximum context length is 8192 tokens"}}"#;
        assert_eq!(from_status(400, None, too_long, KEY), BrainError::ContextTooLarge);
        assert_eq!(from_status(500, None, b"oops", KEY), BrainError::Provider("HTTP 500: HTTP 500".into()));
    }

    #[test]
    fn the_key_never_appears() {
        let body = format!(r#"{{"error":{{"message":"bad key {KEY}"}}}}"#);
        assert!(!from_status(401, None, body.as_bytes(), KEY).to_string().contains(KEY));
        assert!(!from_stream_error(&serde_json::json!({ "message": KEY }), KEY).to_string().contains(KEY));
    }
}
