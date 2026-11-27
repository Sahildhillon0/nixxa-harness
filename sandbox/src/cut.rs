//! Where output is cut to fit a cap: at a line break, and never inside a token (#3, D-074). The
//! controller's scrubber runs on what the sandbox returns, after the cut. A cut through a key
//! would leave a piece of it that no detector recognises; a cut between a name and its value
//! (`"password": "…`) would leave a value nothing flags, since some detectors need the name, or
//! a key id, on the same line. So a cut is moved back (or, for a tail, forward) to the nearest
//! line break within [`LINE_WINDOW`] bytes, and failing that to the nearest byte that can't be
//! part of a key, URL or word: whitespace, quotes, brackets, `,`, `;`, `|`, `<`, `>`. What is
//! dropped this way is counted as cut.
//!
//! Shared by every backend (the daemon includes this file too).

/// Whether `byte` can be part of a token: a key, a URL with its credentials, a word, or any
/// non-ASCII byte (so a cut never splits a character either).
fn in_token(byte: u8) -> bool {
    !byte.is_ascii() || byte.is_ascii_alphanumeric() || b"+/=_-.:~%@!$&*#?^".contains(&byte)
}

/// How far a cut looks for a line break before it settles for a token boundary.
pub(crate) const LINE_WINDOW: usize = 4096;

/// How much of `kept`, the first bytes of a longer output, to keep: up to its last line break
/// near the end, or else the end of its last whole token. 0 when `kept` is all one token.
pub(crate) fn end(kept: &[u8]) -> usize {
    let window = kept.len().saturating_sub(LINE_WINDOW);
    if let Some(at) = kept[window..].iter().rposition(|&b| b == b'\n') {
        return window + at + 1;
    }
    kept.iter().rposition(|&b| !in_token(b)).map_or(0, |at| at + 1)
}

/// Where to start in `kept`, the last bytes of a longer output: after its first line break near
/// the start (unless that leaves nothing), or else after its first token, which may be the end of
/// one cut through. `kept.len()` when `kept` is all one token.
pub(crate) fn start(kept: &[u8]) -> usize {
    let window = kept.len().min(LINE_WINDOW);
    if let Some(at) = kept[..window].iter().position(|&b| b == b'\n') {
        if at + 1 < kept.len() {
            return at + 1;
        }
    }
    kept.iter().position(|&b| !in_token(b)).unwrap_or(kept.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cut_keeps_whole_lines() {
        let text = b"line one\nAWS_SECRET_ACCESS_KEY=wJalrXUtnFEMI/K7MDENG/bPxRfiCY";
        // The first bytes of a longer output: the partial line goes.
        assert_eq!(&text[..end(text)], b"line one\n");
        // The last bytes: the partial first line goes, not just its first token, so a value
        // never arrives without its name.
        let tail = br#"word: "Tr0ub4dor&3xK9", "next": 1
done"#;
        assert_eq!(&tail[start(tail)..], b"done");
        // Only a line break close by counts.
        let far = [b"a\n".as_slice(), &[b'x'; LINE_WINDOW], b" end"].concat();
        assert_eq!(&far[..end(&far)], &far[..far.len() - 3]);
        assert_eq!(start(&far), 2);
        let far_tail = [b"key ".as_slice(), &[b'x'; LINE_WINDOW], b"\nz"].concat();
        assert_eq!(start(&far_tail), 3, "past the window: the first token");
        // A tail whose only break ends it keeps its last line's whole tokens.
        assert_eq!(&b"lo world\n"[start(b"lo world\n")..], b" world\n");
    }

    #[test]
    fn without_a_line_break_a_cut_never_splits_a_token() {
        // The last bytes: the partial first token goes.
        let tail = b"MI/K7MDENG/bPxRfiCYEXAMPLEKEY done";
        assert_eq!(&tail[start(tail)..], b" done");
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
