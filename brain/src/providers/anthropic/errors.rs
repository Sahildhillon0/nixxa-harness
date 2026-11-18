//! HTTP statuses and API error types → [`BrainError`]. Error text never contains the API key.

use nixxa_contracts::BrainError;
use serde_json::Value;

use crate::providers::common::scrub;

/// A non-200 response.
pub(crate) fn from_status(status: u16, retry_after: Option<&str>, body: &[u8], key: &str) -> BrainError {
    let (kind, message) = parse_body(body);
    let message = scrub(&message.unwrap_or_else(|| format!("HTTP {status}")), key);
    let retry_after_ms = retry_after.and_then(parse_retry_after);
    match (status, kind.as_deref()) {
        (401 | 403, _) | (_, Some("authentication_error" | "permission_error")) => BrainError::Auth(message),
        (429, _) | (_, Some("rate_limit_error")) => BrainError::RateLimited { retry_after_ms },
        (413, _) | (_, Some("request_too_large")) => BrainError::ContextTooLarge,
        (400, _) if is_context_too_long(&message) => BrainError::ContextTooLarge,
        (_, Some(kind)) => BrainError::Provider(format!("{kind} (HTTP {status}): {message}")),
        _ => BrainError::Provider(format!("HTTP {status}: {message}")),
    }
}

/// An `error` event inside the stream: `{"type":"error","error":{"type":…,"message":…}}`.
pub(crate) fn from_stream_error(data: &Value, key: &str) -> BrainError {
    let error = &data["error"];
    let kind = error["type"].as_str().unwrap_or("error");
    let message = scrub(error["message"].as_str().unwrap_or("no message"), key);
    match kind {
        "authentication_error" | "permission_error" => BrainError::Auth(message),
        "rate_limit_error" => BrainError::RateLimited { retry_after_ms: None },
        "request_too_large" => BrainError::ContextTooLarge,
        _ if is_context_too_long(&message) => BrainError::ContextTooLarge,
        _ => BrainError::Provider(format!("{kind}: {message}")),
    }
}

fn parse_body(body: &[u8]) -> (Option<String>, Option<String>) {
    let Ok(value) = serde_json::from_slice::<Value>(body) else { return (None, None) };
    let kind = value["error"]["type"].as_str().map(str::to_string);
    let message = value["error"]["message"].as_str().map(str::to_string);
    (kind, message)
}

/// A 400 that means the prompt doesn't fit the model's context window.
fn is_context_too_long(message: &str) -> bool {
    let message = message.to_lowercase();
    message.contains("prompt is too long") || message.contains("context window") || message.contains("too many tokens")
}

/// `retry-after` in seconds (the API sends a number; an HTTP date is ignored).
fn parse_retry_after(value: &str) -> Option<u64> {
    let seconds: f64 = value.trim().parse().ok()?;
    (seconds.is_finite() && seconds >= 0.0).then(|| (seconds * 1000.0).round() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "sk-ant-api03-secret-key-1234";

    fn body(kind: &str, message: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({"type": "error", "error": {"type": kind, "message": message}})).unwrap()
    }

    #[test]
    fn maps_every_status() {
        let auth = from_status(401, None, &body("authentication_error", "invalid x-api-key"), KEY);
        assert_eq!(auth, BrainError::Auth("invalid x-api-key".into()));
        assert!(matches!(from_status(403, None, &body("permission_error", "no"), KEY), BrainError::Auth(_)));
        assert_eq!(
            from_status(429, Some("2.5"), &body("rate_limit_error", "slow down"), KEY),
            BrainError::RateLimited { retry_after_ms: Some(2500) }
        );
        assert_eq!(
            from_status(429, Some("Wed, 21 Oct 2026 07:28:00 GMT"), b"", KEY),
            BrainError::RateLimited { retry_after_ms: None }
        );
        assert_eq!(from_status(413, None, &body("request_too_large", "big"), KEY), BrainError::ContextTooLarge);
        assert_eq!(
            from_status(400, None, &body("invalid_request_error", "prompt is too long: 1200000 tokens > 1000000"), KEY),
            BrainError::ContextTooLarge
        );
        assert_eq!(
            from_status(400, None, &body("invalid_request_error", "messages: roles must alternate"), KEY),
            BrainError::Provider("invalid_request_error (HTTP 400): messages: roles must alternate".into())
        );
        assert!(matches!(from_status(500, None, &body("api_error", "oops"), KEY), BrainError::Provider(_)));
        assert!(matches!(
            from_status(529, None, &body("overloaded_error", "Overloaded"), KEY),
            BrainError::Provider(_)
        ));
        assert_eq!(
            from_status(502, None, b"<html>bad gateway</html>", KEY),
            BrainError::Provider("HTTP 502: HTTP 502".into())
        );
    }

    #[test]
    fn the_key_never_appears_in_error_text() {
        let err = from_status(401, None, &body("authentication_error", &format!("bad key {KEY}")), KEY);
        assert!(!err.to_string().contains(KEY), "{err}");
        let err = from_stream_error(
            &serde_json::json!({"type": "error", "error": {"type": "api_error", "message": KEY}}),
            KEY,
        );
        assert!(!err.to_string().contains(KEY), "{err}");
    }

    #[test]
    fn stream_errors_map_by_type() {
        let overloaded =
            serde_json::json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}});
        assert_eq!(from_stream_error(&overloaded, KEY), BrainError::Provider("overloaded_error: Overloaded".into()));
    }
}
