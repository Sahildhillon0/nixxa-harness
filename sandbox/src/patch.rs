//! Patches and hashes, shared by every backend (the daemon includes this file too): the hash
//! the agent reads with a file and sends back with a patch, applying search/replace edits, and
//! unified diffs.

use nixxa_contracts::Edit;

/// The longest diff an observation carries; past it the diff is cut and says so.
pub(crate) const DIFF_CAP: usize = 64 * 1024;

/// The lowercase hex SHA-256 of `bytes`.
pub(crate) fn hash(bytes: &[u8]) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push(char::from(b"0123456789abcdef"[usize::from(byte >> 4)]));
        out.push(char::from(b"0123456789abcdef"[usize::from(byte & 15)]));
    }
    out
}

/// Applies `edits` to `old`, whose hash must be `expected`. Each edit applies to the result of
/// the ones before, and its `search` must occur exactly once (overlapping occurrences count).
/// All or nothing: on any failure nothing is returned but the words the agent reads.
pub(crate) fn apply(old: &[u8], edits: &[Edit], expected: &str) -> Result<Vec<u8>, String> {
    let current = hash(old);
    if current != expected.trim().to_ascii_lowercase() {
        return Err(format!(
            "the file changed since you read it, so nothing was changed: its hash is now {current}. Read it again \
             and send the patch against what it holds now"
        ));
    }
    if edits.is_empty() {
        return Err("the patch has no edits".into());
    }
    let total = edits.len();
    let mut text = old.to_vec();
    for (n, edit) in edits.iter().enumerate() {
        let which = format!("edit {} of {total}", n + 1);
        let search = edit.search.as_bytes();
        if search.is_empty() {
            return Err(format!("{which}: the search text is empty, so nothing was changed"));
        }
        let mut found = text.windows(search.len()).enumerate().filter(|(_, window)| *window == search);
        let Some((at, _)) = found.next() else {
            return Err(format!(
                "{which}: the search text was not found, so nothing was changed. It must match the file exactly, \
                 whitespace included"
            ));
        };
        if found.next().is_some() {
            return Err(format!(
                "{which}: the search text matches more than one place, so nothing was changed. Include more of the \
                 surrounding lines to make it unique"
            ));
        }
        text.splice(at..at + search.len(), edit.replace.bytes());
    }
    Ok(text)
}

/// A unified diff of `old` to `new`, labelled with `path`, cut at [`DIFF_CAP`]. Content that
/// isn't UTF-8 is shown lossily.
pub(crate) fn unified(path: &str, old: &[u8], new: &[u8]) -> String {
    let (old, new) = (String::from_utf8_lossy(old), String::from_utf8_lossy(new));
    let diff = similar::TextDiff::from_lines(old.as_ref(), new.as_ref());
    let mut text = diff.unified_diff().context_radius(3).header(&format!("a/{path}"), &format!("b/{path}")).to_string();
    if text.len() > DIFF_CAP {
        let mut cut = DIFF_CAP;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push_str("\n[diff truncated]\n");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(search: &str, replace: &str) -> Edit {
        Edit { search: search.into(), replace: replace.into() }
    }

    #[test]
    fn hashes_are_sha256_hex() {
        assert_eq!(hash(b"hello nixxa\n"), "cb6112f1c4749bb872768a2125c60119597c2a657661a3618dc11c0a1f68e685");
        assert_eq!(hash(b"").len(), 64);
    }

    #[test]
    fn edits_apply_in_order_each_exactly_once() {
        let old = b"fn a() {}\nfn b() {}\n";
        let new = apply(old, &[edit("fn a", "fn alpha"), edit("alpha() {}", "alpha() { b() }")], &hash(old)).unwrap();
        assert_eq!(new, b"fn alpha() { b() }\nfn b() {}\n");
    }

    #[test]
    fn every_rule_fails_with_words_and_changes_nothing() {
        let old = b"aaa\nxyz\nxyz\n";
        let h = hash(old);
        let stale = apply(old, &[edit("aaa", "b")], &hash(b"other")).unwrap_err();
        assert!(stale.contains(&h), "the current hash: {stale}");
        assert!(
            apply(old, &[edit("nope", "b")], &h).unwrap_err().contains("edit 1 of 1: the search text was not found")
        );
        assert!(apply(old, &[edit("xyz", "b")], &h).unwrap_err().contains("matches more than one place"));
        // Overlapping occurrences count as several.
        assert!(apply(old, &[edit("aa", "b")], &h).unwrap_err().contains("more than one place"));
        assert!(apply(old, &[edit("aaa", "b"), edit("", "c")], &h).unwrap_err().contains("edit 2 of 2"));
        assert!(apply(old, &[], &h).is_err());
        // All or nothing: a later edit failing leaves no earlier one applied.
        assert!(apply(old, &[edit("aaa", "b"), edit("nope", "c")], &h).unwrap_err().contains("edit 2 of 2"));
        // The hash compares case-insensitively and ignores surrounding blanks.
        assert!(apply(old, &[edit("aaa", "b")], &format!(" {} ", h.to_uppercase())).is_ok());
    }

    #[test]
    fn diffs_are_unified_and_capped() {
        let diff = unified("src/a.rs", b"one\ntwo\n", b"one\n2\n");
        assert!(diff.contains("--- a/src/a.rs") && diff.contains("+++ b/src/a.rs"), "{diff}");
        assert!(diff.contains("-two") && diff.contains("+2"), "{diff}");
        let big: String = (0..20_000).map(|n| format!("line {n}\n")).collect();
        let cut = unified("big", b"", big.as_bytes());
        assert!(cut.len() < DIFF_CAP + 100 && cut.ends_with("[diff truncated]\n"));
    }
}
