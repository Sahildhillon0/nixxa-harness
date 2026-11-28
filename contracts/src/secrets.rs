//! Secret redaction (#3): fingerprints of the secrets Nixxa holds, and where a redaction came
//! from.
//!
//! `accounts` builds [`SecretFingerprints`] from every key and token it holds; the controller's
//! scrubber asks them where a text contains one of those secrets, exactly. The fingerprints hold
//! no secret: each is a salted SHA-256 of the secret and a keyed rolling hash, both under a
//! random salt chosen per run. The matching lives here, with the type, because the side that
//! builds the fingerprints and the side that matches them must hash identically.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{ActionId, JobId, Secret, SourceId};

/// The modulus of the rolling hash: the Mersenne prime 2^61 - 1.
const MODULUS: u64 = (1 << 61) - 1;

/// One secret's fingerprint.
#[derive(Clone)]
struct Print {
    rolling: u64,
    digest: [u8; 32],
    kind: String,
}

/// Salted fingerprints of secrets, for finding them in text without holding them.
///
/// Finding is a Rabin–Karp scan: for each distinct secret length, a rolling hash (keyed by the
/// salt) over every window of that length; a window whose rolling hash is a secret's is
/// confirmed by its salted SHA-256. On 64 KiB of text with a few known lengths this is a few
/// milliseconds. Secrets shorter than [`SecretFingerprints::MIN_LEN`] bytes are not kept: they
/// would match ordinary text.
#[derive(Clone)]
pub struct SecretFingerprints {
    salt: [u8; 32],
    base: u64,
    /// By secret length: the prints of that length.
    by_len: BTreeMap<usize, Vec<Print>>,
}

impl fmt::Debug for SecretFingerprints {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecretFingerprints").field("secrets", &self.len()).finish_non_exhaustive()
    }
}

/// A known secret found in a text: its byte range and what kind it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnownSecret {
    /// Where it starts (a byte offset, on a character boundary).
    pub start: usize,
    /// Where it ends (exclusive).
    pub end: usize,
    /// What it is, as given when it was added (e.g. `anthropic-key`).
    pub kind: String,
}

fn mul(a: u64, b: u64) -> u64 {
    let product = u128::from(a) * u128::from(b);
    let folded = (product & u128::from(MODULUS)) + (product >> 61);
    let folded = u64::try_from(folded).unwrap_or(u64::MAX);
    if folded >= MODULUS { folded - MODULUS } else { folded }
}

fn add(a: u64, b: u64) -> u64 {
    let sum = a + b;
    if sum >= MODULUS { sum - MODULUS } else { sum }
}

impl SecretFingerprints {
    /// The shortest secret kept, in bytes.
    pub const MIN_LEN: usize = 8;

    /// Empty fingerprints under `salt`, which should be random and new for each run.
    pub fn new(salt: [u8; 32]) -> Self {
        let digest = Sha256::digest([b"nixxa-rolling-base".as_slice(), &salt].concat());
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&digest[..8]);
        // A base well away from 0 and 1.
        let base = u64::from_le_bytes(bytes) % (MODULUS - 1024) + 512;
        Self { salt, base, by_len: BTreeMap::new() }
    }

    /// Adds `secret`, labelled `kind` (e.g. `anthropic-key`). One shorter than
    /// [`Self::MIN_LEN`] bytes is skipped.
    pub fn add(&mut self, kind: &str, secret: &Secret) {
        let bytes = secret.expose().as_bytes();
        if bytes.len() < Self::MIN_LEN {
            return;
        }
        let print = Print { rolling: self.rolling(bytes), digest: self.digest(bytes), kind: kind.to_string() };
        let prints = self.by_len.entry(bytes.len()).or_default();
        if !prints.iter().any(|p| p.digest == print.digest) {
            prints.push(print);
        }
    }

    /// How many secrets are kept.
    pub fn len(&self) -> usize {
        self.by_len.values().map(Vec::len).sum()
    }

    /// Whether none are.
    pub fn is_empty(&self) -> bool {
        self.by_len.is_empty()
    }

    /// Every occurrence of a known secret in `text`, by start; overlapping ones included.
    pub fn find(&self, text: &str) -> Vec<KnownSecret> {
        let bytes = text.as_bytes();
        let mut found = Vec::new();
        for (&len, prints) in &self.by_len {
            if bytes.len() < len {
                continue;
            }
            // base^(len-1), to take the leaving byte out.
            let mut top = 1u64;
            for _ in 1..len {
                top = mul(top, self.base);
            }
            let mut hash = self.rolling(&bytes[..len]);
            let mut start = 0;
            loop {
                if prints.iter().any(|p| p.rolling == hash) {
                    let window = &bytes[start..start + len];
                    let digest = self.digest(window);
                    if let Some(print) = prints.iter().find(|p| p.digest == digest) {
                        found.push(KnownSecret { start, end: start + len, kind: print.kind.clone() });
                    }
                }
                if start + len == bytes.len() {
                    break;
                }
                let leaving = mul(u64::from(bytes[start]), top);
                hash = add(hash, MODULUS - leaving);
                hash = add(mul(hash, self.base), u64::from(bytes[start + len]));
                start += 1;
            }
        }
        found.sort_by_key(|k| (k.start, std::cmp::Reverse(k.end)));
        found
    }

    fn rolling(&self, bytes: &[u8]) -> u64 {
        bytes.iter().fold(0, |hash, &b| add(mul(hash, self.base), u64::from(b)))
    }

    fn digest(&self, bytes: &[u8]) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(self.salt);
        hasher.update(bytes);
        hasher.finalize().into()
    }
}

/// Where a redacted secret was about to come from. Never says what the secret was.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "from", rename_all = "snake_case")]
pub enum RedactionSource {
    /// The user's message.
    UserMessage,
    /// Lines the user shared from the `ctrl+t` terminal, in their message.
    TerminalShare,
    /// A file the agent read.
    FileRead {
        /// The path, as the agent gave it.
        path: String,
    },
    /// The diff of a file the agent edited.
    FileEdit {
        /// The path, as the agent gave it.
        path: String,
    },
    /// What a command printed.
    CommandOutput {
        /// The action.
        action: ActionId,
    },
    /// A background job's output or status.
    JobOutput {
        /// The job.
        job: JobId,
    },
    /// Nixxa's note to the agent that a job ended.
    JobNote,
    /// A URL a command requested (a token in a query string).
    NetworkRequest {
        /// The action.
        action: ActionId,
    },
    /// What a knowledge source returned, or put in the session's primer.
    Knowledge {
        /// The source.
        source: SourceId,
    },
    /// Any other result of an action (a listing, an error message).
    ActionResult {
        /// The action.
        action: ActionId,
    },
    /// A notice from Nixxa to the agent (an undo, the network policy, an error).
    Notice,
    /// A stored session being resumed: what it holds is sent again, so it is scrubbed again.
    ResumedHistory,
    /// The prose of a project's `AGENTS.md`, given to the agent as project rules (#5).
    ProjectRules {
        /// The file, workspace-relative.
        file: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prints(secrets: &[(&str, &str)]) -> SecretFingerprints {
        let mut prints = SecretFingerprints::new([7; 32]);
        for (kind, secret) in secrets {
            prints.add(kind, &Secret::new(*secret));
        }
        prints
    }

    #[test]
    fn a_known_secret_is_found_anywhere_exactly() {
        let prints = prints(&[("test-key", "sk-test-0123456789abcdef"), ("other-key", "tok_abcdefgh")]);
        let text = "a sk-test-0123456789abcdef b\nsk-test-0123456789abcdef";
        let found = prints.find(text);
        assert_eq!(found.len(), 2, "{found:?}");
        assert_eq!(&text[found[0].start..found[0].end], "sk-test-0123456789abcdef");
        assert_eq!(found[0].kind, "test-key");
        assert_eq!(found[1].start, text.len() - 24, "at the very end too");
        assert_eq!(prints.find("tok_abcdefgh")[0].kind, "other-key", "the whole text");
        // One byte different is not the secret.
        assert!(prints.find("sk-test-0123456789abcdeF").is_empty());
        assert!(prints.find("sk-test-0123456789abcde").is_empty());
        assert!(prints.find("").is_empty());
    }

    #[test]
    fn nothing_of_the_secret_is_kept_or_shown() {
        let kept = prints(&[("test-key", "sk-test-0123456789abcdef")]);
        let shown = format!("{kept:?}");
        assert!(!shown.contains("0123456789"), "{shown}");
        assert_eq!(kept.len(), 1);
        // Short secrets would match ordinary text: not kept.
        assert!(prints(&[("pin", "1234567")]).is_empty());
    }

    #[test]
    fn another_salt_finds_the_same_secrets() {
        let mut other = SecretFingerprints::new([9; 32]);
        other.add("test-key", &Secret::new("sk-test-0123456789abcdef"));
        assert_eq!(other.find("x sk-test-0123456789abcdef").len(), 1);
    }

    #[test]
    fn finding_is_fast_on_a_large_output() {
        let prints = prints(&[("a", "sk-test-0123456789abcdef"), ("b", "0123456789abcdef0123456789abcdef0123")]);
        let text = "lorem ipsum dolor sit amet ".repeat(64 * 1024 / 27);
        let started = std::time::Instant::now();
        assert!(prints.find(&text).is_empty());
        assert!(started.elapsed() < std::time::Duration::from_millis(500), "{:?}", started.elapsed());
    }
}
