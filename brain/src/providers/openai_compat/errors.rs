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

/// Wordings servers use when a model can't call tools (Ollama: "<model> does not support tools";
/// LM Studio and others vary), compared in lower case.
const NO_TOOLS: &[&str] = &[
    "does not support tools",
    "doesn't support tools",
    "does not support tool",
    "doesn't support tool",
    "tools are not supported",
    "tool use is not supported",
    "tool calling is not supported",
    "does not support function calling",
    "function calling is not supported",
];

/// Turns a server's "this model can't use tools" error, in any of its wordings, into a hint the
/// user can act on. Other errors pass through unchanged.
pub(crate) fn explain_no_tools(err: BrainError, model: &str) -> BrainError {
    let text = err.to_string().to_lowercase();
    if NO_TOOLS.iter().any(|phrase| text.contains(phrase)) {
        BrainError::Provider(format!(
            "`{}` can't use tools. Pick a tool-capable model (e.g. gpt-oss, qwen2.5/3, llama3.1+).",
            model.escape_debug()
        ))
    } else {
        err
    }
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
    fn a_model_without_tools_gets_a_hint_in_every_wording() {
        let ollama = br#"{"error":"registry.ollama.ai/library/gemma:2b does not support tools"}"#;
        let lm_studio = br#"{"error":{"message":"Error rendering prompt with jinja template: Tool use is not supported in this model"}}"#;
        for body in [&ollama[..], &lm_studio[..]] {
            let err = explain_no_tools(from_status(400, None, body, KEY), "gemma:2b");
            assert_eq!(
                err,
                BrainError::Provider(
                    "`gemma:2b` can't use tools. Pick a tool-capable model (e.g. gpt-oss, qwen2.5/3, llama3.1+)."
                        .into()
                ),
                "{}",
                String::from_utf8_lossy(body)
            );
        }
        let streamed = from_stream_error(&serde_json::json!({ "message": "This model DOES NOT SUPPORT TOOLS" }), KEY);
        assert!(explain_no_tools(streamed, "m").to_string().contains("can't use tools"));
        let other = from_status(500, None, b"oops", KEY);
        assert_eq!(explain_no_tools(other.clone(), "m"), other, "other errors pass through");
    }

    #[test]
    fn the_key_never_appears() {
        let body = format!(r#"{{"error":{{"message":"bad key {KEY}"}}}}"#);
        assert!(!from_status(401, None, body.as_bytes(), KEY).to_string().contains(KEY));
        assert!(!from_stream_error(&serde_json::json!({ "message": KEY }), KEY).to_string().contains(KEY));
    }
}
