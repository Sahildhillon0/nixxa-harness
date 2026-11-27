//! Where output is cut to fit a cap: never inside a token (#3, D-074). The controller's scrubber
//! runs on what the sandbox returns, after the cut; a cut through a key would leave a piece of
//! it that no detector recognises. So a cut is moved back (or, for a tail, forward) to the
//! nearest byte that can't be part of a key, URL or word: whitespace, quotes, brackets, `,`, `;`,
//! `|`, `<`, `>`. The piece of a token dropped this way is counted as cut.
//!
//! Shared by every backend (the daemon includes this file too).

/// Whether `byte` can be part of a token: a key, a URL with its credentials, a word, or any
/// non-ASCII byte (so a cut never splits a character either).
fn in_token(byte: u8) -> bool {
    !byte.is_ascii() || byte.is_ascii_alphanumeric() || b"+/=_-.:~%@!$&*#?^".contains(&byte)
}

/// How much of `kept`, the first bytes of a longer output, to keep: up to the end of the last
/// whole token, so the cut doesn't split one. 0 when `kept` is all one token.
pub(crate) fn end(kept: &[u8]) -> usize {
    kept.iter().rposition(|&b| !in_token(b)).map_or(0, |at| at + 1)
}

/// Where to start in `kept`, the last bytes of a longer output: after its first token, which
/// may be the end of one cut through. `kept.len()` when `kept` is all one token.
pub(crate) fn start(kept: &[u8]) -> usize {
    kept.iter().position(|&b| !in_token(b)).unwrap_or(kept.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cut_never_splits_a_token() {
        let text = b"line one\nAWS_SECRET_ACCESS_KEY=wJalrXUtnFEMI/K7MDENG/bPxRfiCY";
        // The first bytes of a longer output: the partial assignment goes.
        assert_eq!(&text[..end(text)], b"line one\n");
        // The last bytes: the partial first token goes.
        let tail = b"MI/K7MDENG/bPxRfiCYEXAMPLEKEY\ndone";
        assert_eq!(&tail[start(tail)..], b"\ndone");
        // A URL with a password is one token.
        let url = b"connect postgres://app:hunter2@db:5432/x";
        assert_eq!(&url[..end(url)], b"connect ");
        // Quotes and brackets end a token.
        let json = br#"{"token": "eyJhbGciOiJIUzI1NiJ9.eyJzd"#;
        assert_eq!(&json[..end(json)], br#"{"token": ""#);
        // All one token: nothing is kept.
        assert_eq!(end(b"sk-ant-api03-abcdef"), 0);
        assert_eq!(start(b"sk-ant-api03-abcdef"), 19);
        // Nor is a character split.
        let accented = "ok é".as_bytes();
        assert_eq!(&accented[..end(&accented[..4])], b"ok ");
    }
}
