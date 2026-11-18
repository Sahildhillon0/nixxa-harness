//! What every provider shares: HTTP transport, SSE parsing, tools and their validation, context
//! trimming, the system prompt and observation text.

pub(crate) mod history;
#[cfg(feature = "replay")]
pub(crate) mod replay;
pub(crate) mod sse;
pub(crate) mod tools;
pub(crate) mod transport;
pub(crate) mod trim;

/// The longest provider message passed on.
const MAX_MESSAGE: usize = 500;

/// Removes the key from text that came back from a provider, and keeps it short.
pub(crate) fn scrub(text: &str, key: &str) -> String {
    let mut text = if key.len() >= 8 { text.replace(key, "***") } else { text.to_string() };
    if text.chars().count() > MAX_MESSAGE {
        text = text.chars().take(MAX_MESSAGE).collect::<String>() + "…";
    }
    text
}

#[cfg(test)]
mod tests {
    use super::scrub;

    #[test]
    fn scrub_removes_the_key_and_truncates() {
        assert_eq!(scrub("bad key sk-test-12345678", "sk-test-12345678"), "bad key ***");
        assert_eq!(scrub("x".repeat(600).as_str(), "").chars().count(), 501);
        assert_eq!(scrub("short key abc", "abc"), "short key abc", "keys under 8 characters aren't scrubbed");
    }
}
