//! The secret scrubber (#3, D-074): every text that enters the agent's context, and everything
//! the session stores, passes through it. A secret becomes a placeholder,
//! `‹secret:aws-access-key#3f2a›`, the same for the same value all session.
//!
//! What it finds: the exact secrets Nixxa holds (from `accounts`' fingerprints), known key
//! formats, private-key blocks, passwords in URLs, the AWS secret key next to its key id, and
//! high-entropy values assigned to secret-ish names. It is defence in depth: an encoded, split
//! or computed secret gets through.

mod detect;
#[cfg(test)]
mod tests;

use std::sync::RwLock;

use hmac::{Hmac, Mac};
use nixxa_contracts::SecretFingerprints;
use sha2::Sha256;

/// What opens and closes a placeholder: characters no key, URL or code uses.
const OPEN: &str = "‹secret:";
const CLOSE: char = '›';

/// A secret found in a text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// Where it starts (a byte offset).
    pub start: usize,
    /// Where it ends (exclusive).
    pub end: usize,
    /// What kind it is (`aws-access-key`, `anthropic-key`, `password`, …).
    pub kind: String,
    /// What stands in for it.
    pub placeholder: String,
}

/// A text with its secrets replaced, and what was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scrubbed {
    /// The text, with placeholders.
    pub text: String,
    /// What was replaced, in order (offsets into the original text).
    pub found: Vec<Found>,
}

/// Why a scrubber couldn't be made.
#[derive(Debug, thiserror::Error)]
#[error("no secret scrubber: the system's random source is unavailable ({0})")]
pub struct ScrubberError(String);

/// Finds secrets and replaces them with placeholders whose tags are keyed hashes: an unkeyed
/// tag of a short password could be brute-forced from the placeholder.
pub struct Scrubber {
    key: [u8; 32],
    known: RwLock<SecretFingerprints>,
}

impl std::fmt::Debug for Scrubber {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Scrubber").finish_non_exhaustive()
    }
}

impl Scrubber {
    /// A scrubber with a fresh random key: one per session, so placeholders are stable within
    /// it and unlinkable across sessions.
    pub fn new() -> Result<Self, ScrubberError> {
        let mut key = [0u8; 32];
        getrandom::getrandom(&mut key).map_err(|err| ScrubberError(err.to_string()))?;
        Ok(Self::with_key(key))
    }

    /// A scrubber with a given key (tests, or a session that must keep its placeholders).
    pub fn with_key(key: [u8; 32]) -> Self {
        Self { key, known: RwLock::new(SecretFingerprints::new(key)) }
    }

    /// The fingerprints of the secrets Nixxa holds (from `Credentials::fingerprints`): found
    /// anywhere, exactly. Replaces any given before.
    pub fn set_known(&self, known: SecretFingerprints) {
        *self.known.write().unwrap_or_else(std::sync::PoisonError::into_inner) = known;
    }

    /// The secrets in `text`, in order, without overlaps. A placeholder already in it is left
    /// alone, but a secret that holds one (a key block or a URL password with a placeholder
    /// written into it) is still found, the placeholder with it: placeholders get into files.
    pub fn find(&self, text: &str) -> Vec<Found> {
        let placeholders = placeholder_spans(text);
        let mut hits: Vec<(usize, usize, String, u8)> = Vec::new();
        let known = self.known.read().unwrap_or_else(std::sync::PoisonError::into_inner);
        // Exact secrets first: they win an overlap.
        for k in known.find(text) {
            hits.push((k.start, k.end, k.kind, 0));
        }
        drop(known);
        for hit in detect::all(text) {
            hits.push((hit.start, hit.end, hit.kind.to_string(), 1));
        }
        // A hit inside a placeholder is that placeholder; one that only overlaps a placeholder
        // takes all of it, so no piece of a placeholder is left behind.
        hits.retain(|&(start, end, ..)| !placeholders.iter().any(|&(s, e)| start >= s && end <= e));
        for hit in &mut hits {
            for &(s, e) in &placeholders {
                if hit.0 < e && hit.1 > s {
                    hit.0 = hit.0.min(s);
                    hit.1 = hit.1.max(e);
                }
            }
        }
        // Earliest first; at the same start, exact before detected, then the longest.
        hits.sort_by(|a, b| a.0.cmp(&b.0).then(a.3.cmp(&b.3)).then(b.1.cmp(&a.1)));
        let mut found: Vec<Found> = Vec::new();
        for (start, end, kind, _) in hits {
            let overlaps_found = found.last().is_some_and(|last| start < last.end);
            if start >= end || overlaps_found {
                continue;
            }
            let placeholder = self.placeholder(&kind, &text[start..end]);
            found.push(Found { start, end, kind, placeholder });
        }
        found
    }

    /// `text` with every secret replaced.
    pub fn scrub(&self, text: &str) -> Scrubbed {
        self.scrub_except(text, &[])
    }

    /// `text` with every secret replaced but those whose placeholders are in `keep` (the ones
    /// the user chose to send as they are). `found` lists them all, kept ones included.
    pub fn scrub_except(&self, text: &str, keep: &[String]) -> Scrubbed {
        let found = self.find(text);
        let mut out = String::with_capacity(text.len());
        let mut at = 0;
        for f in &found {
            out.push_str(&text[at..f.start]);
            if keep.contains(&f.placeholder) {
                out.push_str(&text[f.start..f.end]);
            } else {
                out.push_str(&f.placeholder);
            }
            at = f.end;
        }
        out.push_str(&text[at..]);
        Scrubbed { text: out, found }
    }

    fn placeholder(&self, kind: &str, value: &str) -> String {
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&self.key).expect("HMAC takes a key of any length");
        mac.update(value.as_bytes());
        let tag = mac.finalize().into_bytes();
        format!("{OPEN}{kind}#{:02x}{:02x}{CLOSE}", tag[0], tag[1])
    }
}

/// The byte ranges of placeholders in `text`.
fn placeholder_spans(text: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    for (at, _) in text.match_indices(OPEN) {
        let rest = &text[at + OPEN.len()..];
        let Some(close) = rest.find(CLOSE) else { continue };
        let inner = &rest[..close];
        let well_formed = inner.len() <= 64
            && inner.split_once('#').is_some_and(|(kind, tag)| {
                !kind.is_empty()
                    && kind.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                    && tag.len() == 4
                    && tag.bytes().all(|b| b.is_ascii_hexdigit())
            });
        if well_formed {
            spans.push((at, at + OPEN.len() + close + CLOSE.len_utf8()));
        }
    }
    spans
}

/// How many placeholders `text` holds.
pub(crate) fn placeholders_in(text: &str) -> usize {
    placeholder_spans(text).len()
}
