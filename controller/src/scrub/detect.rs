//! The detectors: known key formats, private-key blocks (also base64-encoded), passwords in
//! URLs, `Authorization` credentials, the AWS secret next to its key id, and high-entropy values
//! assigned to secret-ish names (D-074). Each gives byte
//! ranges of `text` with a kind; the scrubber resolves overlaps and replaces them.
//!
//! Written by hand rather than with a regex engine: every rule is a prefix and a character
//! class, and a byte scan keeps a 64 KiB output to a few milliseconds.

/// A detector's hit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Hit {
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) kind: &'static str,
}

/// Characters of a key's body.
#[derive(Debug, Clone, Copy)]
enum Class {
    /// `A-Z a-z 0-9 _ -`
    Word,
    /// `A-Z a-z 0-9`
    Alnum,
    /// `0-9 a-f`
    Hex,
    /// `A-Z 0-9`
    UpperAlnum,
    /// `A-Z a-z 0-9 _`
    WordNoDash,
    /// `A-Z a-z 0-9 /`
    SlackPath,
    /// `A-Z a-z 0-9 _ - .`
    WordDot,
}

impl Class {
    fn has(self, b: u8) -> bool {
        match self {
            Self::Word => b.is_ascii_alphanumeric() || b == b'_' || b == b'-',
            Self::Alnum => b.is_ascii_alphanumeric(),
            Self::Hex => b.is_ascii_digit() || (b'a'..=b'f').contains(&b),
            Self::UpperAlnum => b.is_ascii_uppercase() || b.is_ascii_digit(),
            Self::WordNoDash => b.is_ascii_alphanumeric() || b == b'_',
            Self::SlackPath => b.is_ascii_alphanumeric() || b == b'/',
            Self::WordDot => b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'),
        }
    }
}

/// A key format: a literal prefix, then at least `min` (and, if set, exactly `exact`)
/// characters of `class`.
struct Format {
    kind: &'static str,
    prefixes: &'static [&'static str],
    class: Class,
    min: usize,
    exact: Option<usize>,
}

/// Longer prefixes first where one starts another (`sk-ant-` before `sk-`).
const FORMATS: &[Format] = &[
    Format { kind: "anthropic-key", prefixes: &["sk-ant-"], class: Class::Word, min: 20, exact: None },
    Format { kind: "openrouter-key", prefixes: &["sk-or-v1-"], class: Class::Hex, min: 32, exact: None },
    Format {
        kind: "openai-key",
        prefixes: &["sk-proj-", "sk-svcacct-", "sk-admin-"],
        class: Class::Word,
        min: 20,
        exact: None,
    },
    Format { kind: "openai-key", prefixes: &["sk-"], class: Class::Alnum, min: 32, exact: None },
    Format {
        kind: "github-token",
        prefixes: &["ghp_", "gho_", "ghu_", "ghs_", "ghr_"],
        class: Class::Alnum,
        min: 36,
        exact: None,
    },
    Format { kind: "github-token", prefixes: &["github_pat_"], class: Class::WordNoDash, min: 22, exact: None },
    Format {
        kind: "aws-access-key",
        prefixes: &["AKIA", "ASIA", "ABIA", "ACCA"],
        class: Class::UpperAlnum,
        min: 16,
        exact: Some(16),
    },
    Format {
        kind: "slack-token",
        prefixes: &["xoxb-", "xoxp-", "xoxa-", "xoxr-", "xoxs-", "xoxe-"],
        class: Class::Word,
        min: 10,
        exact: None,
    },
    Format {
        kind: "slack-webhook",
        prefixes: &["https://hooks.slack.com/services/"],
        class: Class::SlackPath,
        min: 20,
        exact: None,
    },
    Format {
        kind: "stripe-key",
        prefixes: &["sk_live_", "sk_test_", "rk_live_", "rk_test_", "whsec_"],
        class: Class::Alnum,
        min: 16,
        exact: None,
    },
    Format { kind: "google-api-key", prefixes: &["AIza"], class: Class::Word, min: 35, exact: Some(35) },
    Format { kind: "gitlab-token", prefixes: &["glpat-"], class: Class::Word, min: 20, exact: None },
    Format { kind: "huggingface-token", prefixes: &["hf_"], class: Class::Alnum, min: 30, exact: None },
    // `SG.` + 22 + `.` + 43.
    Format { kind: "sendgrid-key", prefixes: &["SG."], class: Class::WordDot, min: 60, exact: None },
];

/// Every detector's hits, unsorted and possibly overlapping.
pub(crate) fn all(text: &str) -> Vec<Hit> {
    let mut hits = Vec::new();
    private_keys(text, &mut hits);
    encoded_private_keys(text, &mut hits);
    formats(text, &mut hits);
    authorizations(text, &mut hits);
    jwts(text, &mut hits);
    url_passwords(text, &mut hits);
    let aws: Vec<Hit> = hits.iter().filter(|h| h.kind == "aws-access-key").cloned().collect();
    for key in aws {
        aws_secret_near(text, &key, &mut hits);
    }
    gcp_key_ids(text, &mut hits);
    assignments(text, &mut hits);
    hits
}

fn boundary_before(bytes: &[u8], at: usize) -> bool {
    at == 0 || !(bytes[at - 1].is_ascii_alphanumeric() || bytes[at - 1] == b'_')
}

/// Every occurrence of `needle` in `text`.
fn occurrences<'t>(text: &'t str, needle: &'t str) -> impl Iterator<Item = usize> + 't {
    text.match_indices(needle).map(|(at, _)| at)
}

fn formats(text: &str, hits: &mut Vec<Hit>) {
    let bytes = text.as_bytes();
    let mut taken: Vec<(usize, usize)> = Vec::new();
    for format in FORMATS {
        for prefix in format.prefixes {
            for at in occurrences(text, prefix) {
                if !boundary_before(bytes, at) || taken.iter().any(|&(s, e)| at >= s && at < e) {
                    continue;
                }
                let body = at + prefix.len();
                let len = bytes[body..].iter().take_while(|&&b| format.class.has(b)).count();
                let ok = match format.exact {
                    Some(exact) => len == exact,
                    None => len >= format.min,
                };
                if ok {
                    taken.push((at, body + len));
                    hits.push(Hit { start: at, end: body + len, kind: format.kind });
                }
            }
        }
    }
}

fn base64url(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

/// `eyJ…` header, payload and signature, each at least 8 characters.
fn jwts(text: &str, hits: &mut Vec<Hit>) {
    let bytes = text.as_bytes();
    for at in occurrences(text, "eyJ") {
        if !boundary_before(bytes, at) {
            continue;
        }
        let mut end = at;
        let mut parts = 0;
        loop {
            let len = bytes[end..].iter().take_while(|&&b| base64url(b)).count();
            if len < 8 {
                break;
            }
            end += len;
            parts += 1;
            if parts == 3 || bytes.get(end) != Some(&b'.') {
                break;
            }
            end += 1;
        }
        if parts == 3 {
            hits.push(Hit { start: at, end, kind: "jwt" });
        }
    }
}

/// PEM private keys, raw or with JSON's `\n` escapes, to their `END` line, or to the end of the
/// text when it was cut. An `END` line with no `BEGIN` before it (the text starts inside a key)
/// takes the key lines just before it.
fn private_keys(text: &str, hits: &mut Vec<Hit>) {
    let bytes = text.as_bytes();
    let mut covered = 0;
    for at in occurrences(text, "-----BEGIN ") {
        if at < covered {
            continue;
        }
        let Some(close) = text[at + 11..].find("-----") else { continue };
        let label = &text[at + 11..at + 11 + close];
        if !label.ends_with("PRIVATE KEY") && !label.ends_with("PRIVATE KEY BLOCK") {
            continue;
        }
        let after = at + 11 + close + 5;
        let end = match text[after..].find("-----END ") {
            Some(rel) => {
                let end_at = after + rel + 9;
                text[end_at..].find("-----").map_or(text.len(), |rel| end_at + rel + 5)
            }
            None => text.len(),
        };
        hits.push(Hit { start: at, end, kind: "private-key" });
        covered = end;
    }
    for at in occurrences(text, "-----END ") {
        let Some(close) = text[at + 9..].find("-----") else { continue };
        if !text[at + 9..at + 9 + close].contains("PRIVATE KEY") || hits.iter().any(|h| at >= h.start && at < h.end) {
            continue;
        }
        let end = at + 9 + close + 5;
        // Back over key lines: base64, or JSON-escaped newlines between them.
        let key_byte = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=' | b'\n' | b'\r' | b'\\');
        let mut start = bytes[..at].iter().rposition(|&b| !key_byte(b)).map_or(0, |p| p + 1);
        // From the first key line, not the line break before it.
        while start < at && matches!(bytes[start], b'\n' | b'\r') {
            start += 1;
        }
        hits.push(Hit { start, end, kind: "private-key" });
    }
}

/// `-----BEGIN ` in base64, as it starts a Kubernetes secret's `tls.key` or any base64-encoded
/// PEM file: always these characters at the start of the encoding.
const ENCODED_BEGIN: &str = "LS0tLS1CRUdJTi";

fn base64_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=')
}

/// A base64-encoded PEM private key, to the end of its encoding (over line breaks, when it is
/// wrapped). One that decodes to another block (a certificate) is not a secret.
fn encoded_private_keys(text: &str, hits: &mut Vec<Hit>) {
    let bytes = text.as_bytes();
    for at in occurrences(text, ENCODED_BEGIN) {
        if !boundary_before(bytes, at) || hits.iter().any(|h| at >= h.start && at < h.end) {
            continue;
        }
        let run = |from: usize| bytes[from..].iter().take_while(|&&b| base64_byte(b)).count();
        let mut line = run(at);
        let mut end = at + line;
        // A wrapped encoding (lines of 64 or 76) goes on while the next line is all base64.
        while line >= 60 {
            let next = if bytes[end..].starts_with(b"\r\n") {
                end + 2
            } else if bytes[end..].starts_with(b"\n") {
                end + 1
            } else {
                break;
            };
            let len = run(next);
            let line_end = next + len;
            if len == 0 || !(line_end == bytes.len() || matches!(bytes[line_end], b'\r' | b'\n')) {
                break;
            }
            end = line_end;
            line = len;
        }
        let head: Vec<u8> = bytes[at..end].iter().copied().filter(|&b| base64_byte(b)).take(64).collect();
        if decode_base64(&head).windows(11).any(|w| w == b"PRIVATE KEY") {
            hits.push(Hit { start: at, end, kind: "private-key" });
        }
    }
}

/// Standard base64, as far as it decodes in whole groups of four.
fn decode_base64(encoded: &[u8]) -> Vec<u8> {
    let value = |b: u8| match b {
        b'A'..=b'Z' => Some(b - b'A'),
        b'a'..=b'z' => Some(b - b'a' + 26),
        b'0'..=b'9' => Some(b - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    };
    let mut out = Vec::with_capacity(encoded.len() / 4 * 3);
    for group in encoded.chunks_exact(4) {
        let Some(v) = group.iter().map(|&b| value(b)).collect::<Option<Vec<u8>>>() else { break };
        let n = (u32::from(v[0]) << 18) | (u32::from(v[1]) << 12) | (u32::from(v[2]) << 6) | u32::from(v[3]);
        out.extend_from_slice(&n.to_be_bytes()[1..]);
    }
    out
}

/// The credentials after `Authorization: Bearer ` or `Basic ` (a curl `-H`, an `.http` file, a
/// log, a JSON header map): `authorization` holds no value of its own, and the value follows a
/// space.
fn authorizations(text: &str, hits: &mut Vec<Hit>) {
    let bytes = text.as_bytes();
    for scheme in ["Bearer ", "bearer ", "Basic ", "basic "] {
        for at in occurrences(text, scheme) {
            // `authorization` earlier on the same line, close by.
            let line = bytes[..at].iter().rposition(|&b| b == b'\n').map_or(0, |p| p + 1);
            let before = &text[line.max(at.saturating_sub(40))..at];
            if !before.to_ascii_lowercase().contains("authorization") {
                continue;
            }
            let from = at + scheme.len();
            // RFC 6750's token characters.
            let len =
                bytes[from..].iter().take_while(|&&b| b.is_ascii_alphanumeric() || b"-._~+/=".contains(&b)).count();
            let value = &text[from..from + len];
            let shouting = value.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_' || c == '-');
            if len >= 12 && !shouting && classes(value) >= 2 {
                hits.push(Hit { start: from, end: from + len, kind: "authorization" });
            }
        }
    }
}

/// Passwords that stand for one in docs and tests: never real, and redacting them would only
/// confuse the agent reading an example.
const DUMMY_PASSWORDS: &[&str] = &[
    "pass", "password", "passwd", "pwd", "secret", "changeme", "example", "test", "user", "username", "foo", "bar",
    "foobar", "baz",
];

/// The password in `scheme://user:password@host`.
fn url_passwords(text: &str, hits: &mut Vec<Hit>) {
    let bytes = text.as_bytes();
    for at in occurrences(text, "://") {
        let rest = &bytes[at + 3..];
        // The user info ends at `@`, before any `/`, whitespace or quote.
        let Some(len) = rest
            .iter()
            .position(|&b| matches!(b, b'@' | b'/' | b'"' | b'\'' | b'`' | b'<' | b'>') || b.is_ascii_whitespace())
        else {
            continue;
        };
        if rest[len] != b'@' {
            continue;
        }
        let info = &text[at + 3..at + 3 + len];
        let Some(colon) = info.find(':') else { continue };
        let password = &info[colon + 1..];
        let user = &info[..colon];
        if password.len() < 3
            || reference(password)
            || password.bytes().all(|b| b == b'*' || b == b'x')
            || password.eq_ignore_ascii_case(user)
            || DUMMY_PASSWORDS.iter().any(|dummy| password.eq_ignore_ascii_case(dummy))
        {
            continue;
        }
        let start = at + 3 + colon + 1;
        hits.push(Hit { start, end: start + password.len(), kind: "url-password" });
    }
}

/// A value that names a secret rather than being one: `${VAR}`, `$VAR`, `<your-key>`, `{{ x }}`,
/// `%(x)s`.
fn reference(value: &str) -> bool {
    value.starts_with('$')
        || value.starts_with("{{")
        || value.starts_with("%(")
        || (value.starts_with('<') && value.ends_with('>'))
}

fn aws_secret_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'/' || b == b'+'
}

/// The 40-character AWS secret key, within a few lines of an access key id.
fn aws_secret_near(text: &str, key: &Hit, hits: &mut Vec<Hit>) {
    let bytes = text.as_bytes();
    let mut from = key.start.saturating_sub(300);
    let to = (key.end + 300).min(bytes.len());
    while from < to {
        if !aws_secret_byte(bytes[from]) || (from > 0 && aws_secret_byte(bytes[from - 1])) {
            from += 1;
            continue;
        }
        let len = bytes[from..].iter().take_while(|&&b| aws_secret_byte(b)).count();
        let candidate = &text[from..from + len];
        if len == 40 && classes(candidate) >= 2 && entropy(candidate) >= 3.5 {
            hits.push(Hit { start: from, end: from + len, kind: "aws-secret-key" });
        }
        from += len.max(1);
    }
}

/// `"private_key_id": "<40 hex>"` in a GCP service-account file.
fn gcp_key_ids(text: &str, hits: &mut Vec<Hit>) {
    for at in occurrences(text, "\"private_key_id\"") {
        let rest = &text[at + 16..];
        let Some(open) = rest.find('"') else { continue };
        if !rest[..open].trim().trim_start_matches(':').trim().is_empty() {
            continue;
        }
        let value = &rest[open + 1..];
        let len = value.bytes().take_while(u8::is_ascii_hexdigit).count();
        if len == 40 && value.as_bytes().get(40) == Some(&b'"') {
            let start = at + 16 + open + 1;
            hits.push(Hit { start, end: start + 40, kind: "gcp-private-key-id" });
        }
    }
}

/// Shannon entropy of `s`, in bits per character.
pub(crate) fn entropy(s: &str) -> f64 {
    let mut counts = [0u32; 256];
    for b in s.bytes() {
        counts[usize::from(b)] += 1;
    }
    let n = s.len() as f64;
    counts.iter().filter(|&&c| c > 0).map(|&c| f64::from(c) / n).map(|p| -p * p.log2()).sum()
}

/// How many of lower case, upper case, digits and other characters `s` has.
fn classes(s: &str) -> usize {
    let has = |f: fn(&u8) -> bool| usize::from(s.bytes().any(|b| f(&b)));
    has(u8::is_ascii_lowercase)
        + has(u8::is_ascii_uppercase)
        + has(u8::is_ascii_digit)
        + has(|b| !b.is_ascii_alphanumeric())
}

/// The kind a secret-ish name gives its value, or `None` for an ordinary name.
fn secret_name(name: &str) -> Option<&'static str> {
    let flat: String = name.chars().filter(char::is_ascii_alphanumeric).map(|c| c.to_ascii_lowercase()).collect();
    // Names that hold hashes of public data, not secrets. `sha` only as a hash (`commit_sha`,
    // `sha256`), not inside a word (`SharedAccessKey`).
    let sha = flat.ends_with("sha") || ["sha1", "sha2", "sha3", "sha5", "shasum"].iter().any(|n| flat.contains(n));
    if sha || ["checksum", "integrity", "hash", "digest", "nonce"].iter().any(|n| flat.contains(n)) {
        return None;
    }
    if ["password", "passwd", "passphrase"].iter().any(|n| flat.contains(n)) || flat.ends_with("pwd") {
        return Some("password");
    }
    if flat.contains("token") || flat.contains("bearer") {
        return Some("token");
    }
    if [
        "apikey",
        "accesskey",
        "accountkey",
        "privatekey",
        "signingkey",
        "encryptionkey",
        "masterkey",
        "authkey",
        "secretkey",
    ]
    .iter()
    .any(|n| flat.contains(n))
    {
        return Some("api-key");
    }
    if flat.contains("secret") || flat.contains("credential") || flat == "auth" {
        return Some("secret");
    }
    None
}

/// Whether an assigned value looks like a secret: long enough, varied, high in entropy, and not
/// code (`e.password`, `Type::new`, an unquoted `TypeName`), a reference (`${KEY}`) or a word.
fn secret_value(value: &str, quoted: bool) -> bool {
    // Unquoted, an identifier without a digit is a name in code; a random secret of this length
    // almost always has a digit.
    let identifier = value.starts_with(|c: char| c.is_ascii_alphabetic())
        && value.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !value.chars().any(|c| c.is_ascii_digit());
    if !quoted && identifier {
        return false;
    }
    // `CLIENT_TRAFFIC_SECRET`, `YOUR-API-TOKEN`: a constant's name or a placeholder.
    let shouting = value.contains(['_', '-'])
        && value.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_' || c == '-');
    if shouting {
        return false;
    }
    let code_path = value.split('.').count() > 1
        && value.split('.').all(|part| {
            part.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && part.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        });
    value.len() >= 12
        && !reference(value)
        && !code_path
        && !value.contains("::")
        && !value.contains('‹')
        && classes(value) >= 2
        && entropy(value) >= 3.3
}

fn name_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.')
}

/// A character of an unquoted value. Not `:`: unquoted, `name: Type::new` is code.
fn value_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"+/=_-.~@%!$&*^".contains(&b)
}

/// High-entropy values assigned to secret-ish names: `KEY=…`, `KEY: …`, `"key": "…"`,
/// `key = '…'`, `key := …`, `'key' => '…'`.
fn assignments(text: &str, hits: &mut Vec<Hit>) {
    let bytes = text.as_bytes();
    for (at, &b) in bytes.iter().enumerate() {
        if b != b'=' && b != b':' {
            continue;
        }
        // Not `==`, `=>` handled below, not `::`, not `://`.
        if b == b':'
            && (bytes.get(at + 1) == Some(&b':')
                || bytes.get(at + 1) == Some(&b'/')
                || (at > 0 && bytes[at - 1] == b':'))
        {
            continue;
        }
        if b == b'='
            && (bytes.get(at + 1) == Some(&b'=') || (at > 0 && matches!(bytes[at - 1], b'=' | b'!' | b'<' | b'>')))
        {
            continue;
        }
        // The name: back over spaces, a closing quote, then name characters.
        let mut end = at;
        if b == b'=' && end > 0 && bytes[end - 1] == b':' {
            end -= 1;
        }
        while end > 0 && bytes[end - 1] == b' ' {
            end -= 1;
        }
        if end > 0 && matches!(bytes[end - 1], b'"' | b'\'') {
            end -= 1;
        }
        let start = bytes[..end].iter().rposition(|&b| !name_byte(b)).map_or(0, |p| p + 1);
        let Some(kind) = secret_name(&text[start..end]) else { continue };
        // The value: past `>` of `=>`, spaces, an opening quote.
        let mut from = at + 1;
        if bytes.get(from) == Some(&b'>') {
            from += 1;
        }
        while bytes.get(from).is_some_and(|&b| b == b' ' || b == b'\t') {
            from += 1;
        }
        let quote = bytes.get(from).copied().filter(|&b| b == b'"' || b == b'\'');
        if quote.is_some() {
            from += 1;
        }
        let len = match quote {
            Some(q) => bytes[from..].iter().position(|&b| b == q || b == b'\n').unwrap_or(0),
            None => bytes[from..].iter().take_while(|&&b| value_byte(b)).count(),
        };
        let value = &text[from..from + len];
        // Unquoted, a config value (`.env`, shell, YAML, ini) is the rest of its line; in code
        // (`token: Type,`, `key = call();`, `x.token = fn_name;`) something follows it.
        let rest_of_line = || {
            let after = &bytes[from + len..];
            let stop = after.iter().position(|&b| b == b'\n').unwrap_or(after.len());
            let raw = String::from_utf8_lossy(&after[..stop]);
            let rest = raw.trim();
            // A comment may follow, after a space: `TOKEN=… # ci`, `key = … ; prod`.
            let comment = raw.starts_with([' ', '\t']) && (rest.starts_with('#') || rest.starts_with(';'));
            // Or the next pair of a connection string: `AccountKey=…;EndpointSuffix=…`.
            let pair = rest.strip_prefix(';').is_some_and(|next| {
                let name = next.bytes().take_while(|&b| b.is_ascii_alphanumeric()).count();
                name > 0 && next.as_bytes().get(name) == Some(&b'=')
            });
            rest.is_empty() || comment || pair
        };
        if value.chars().any(char::is_whitespace) || (quote.is_none() && !rest_of_line()) {
            continue;
        }
        if !secret_value(value, quote.is_some()) {
            continue;
        }
        hits.push(Hit { start: from, end: from + len, kind });
    }
}
