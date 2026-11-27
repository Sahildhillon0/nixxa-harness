//! The scrubber against real key shapes, and against what a coding agent reads all day that
//! must pass untouched.

use nixxa_contracts::{Secret, SecretFingerprints};

use super::*;

fn scrubber() -> Scrubber {
    Scrubber::with_key([1; 32])
}

/// The kinds found in `text`, in order.
fn kinds(text: &str) -> Vec<String> {
    scrubber().find(text).into_iter().map(|f| f.kind).collect()
}

/// What each finding covers.
fn covered(text: &str) -> Vec<String> {
    scrubber().find(text).into_iter().map(|f| text[f.start..f.end].to_string()).collect()
}

// Fake keys in each format, built so no scanner mistakes this file for one holding real ones.
fn fake(prefix: &str, body: &str) -> String {
    format!("{prefix}{body}")
}

const BODY40: &str = "Xk29fQpL7mZ0aB3cD4eF5gH6iJ8kL1mN2oP3qR4s";

#[test]
fn known_formats_are_found_whole() {
    let cases = [
        (fake("sk-ant-api03-", BODY40), "anthropic-key"),
        (fake("sk-or-v1-", &"0123456789abcdef".repeat(4)), "openrouter-key"),
        (fake("sk-proj-", BODY40), "openai-key"),
        (fake("sk-", &BODY40[..36].replace('-', "x")), "openai-key"),
        (fake("ghp_", &BODY40[..36]), "github-token"),
        (fake("github_pat_", "11ABCDEFG0123456789_abcdefghijklmnopqrstuvwxyz"), "github-token"),
        (fake("AKIA", "IOSFODNN7EXAMPLE"), "aws-access-key"),
        (fake("xoxb-", "123456789012-1234567890123-AbCdEfGhIjKlMnOp"), "slack-token"),
        (fake("https://hooks.slack.com/services/", "T00000000/B00000000/XXXXXXXXXXXXXXXXXXXXXXXX"), "slack-webhook"),
        (fake("sk_live_", "4eC39HqLyjWDarjtT1zdp7dc"), "stripe-key"),
        (fake("whsec_", "MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw"), "stripe-key"),
        (fake("AIza", "SyD-9tSrke72PouQMnMX-a7eZSW0jkFMBWY"), "google-api-key"),
        (fake("glpat-", "xK29fQpL7mZ0aB3cD4eF"), "gitlab-token"),
        (fake("hf_", &BODY40[..34]), "huggingface-token"),
        (fake("SG.", "ngeVfQFYQlKU0ufo8x5d1A.TwL2iGABf9DHoTf-09kqeF8tAmbihYzrnopKc-1s5cr"), "sendgrid-key"),
        (
            fake("eyJhbGciOiJIUzI1NiJ9.", "eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U"),
            "jwt",
        ),
    ];
    for (key, kind) in cases {
        let text = format!("value: {key}\n");
        assert_eq!(kinds(&text), [kind], "{key}");
        assert_eq!(covered(&text), std::slice::from_ref(&key), "the whole key, nothing around it");
        let scrubbed = scrubber().scrub(&text).text;
        assert!(!scrubbed.contains(&key[4..]), "{scrubbed}");
        assert!(scrubbed.starts_with(&format!("value: ‹secret:{kind}#")), "{scrubbed}");
    }
}

#[test]
fn private_keys_are_found_raw_escaped_and_cut() {
    let body = "MIIEvQIBADANBgkqhkiG9w0BAQEFAASC\nBKcwggSjAgEAAoIBAQC7VJTUt9Us8cKj";
    let raw = format!("x\n-----BEGIN RSA PRIVATE KEY-----\n{body}\n-----END RSA PRIVATE KEY-----\ny");
    assert_eq!(covered(&raw), [format!("-----BEGIN RSA PRIVATE KEY-----\n{body}\n-----END RSA PRIVATE KEY-----")]);
    // A GCP service-account file: escaped newlines in JSON, and the key id.
    let json = format!(
        r#"{{"type": "service_account", "private_key_id": "{}", "private_key": "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n"}}"#,
        "a1b2c3d4e5".repeat(4),
        body.replace('\n', "\\n")
    );
    assert_eq!(kinds(&json), ["gcp-private-key-id", "private-key"]);
    assert!(!scrubber().scrub(&json).text.contains("MIIEvQ"));
    // Cut after BEGIN (the end of an output was dropped): to the end of the text.
    let head = format!("-----BEGIN OPENSSH PRIVATE KEY-----\n{body}");
    assert_eq!(covered(&head), std::slice::from_ref(&head));
    // Cut before BEGIN (a job's tail): the key lines before END.
    let tail = format!("--- stdout ---\n{body}\n-----END OPENSSH PRIVATE KEY-----\ndone");
    let scrubbed = scrubber().scrub(&tail).text;
    assert!(!scrubbed.contains("MIIEvQ") && !scrubbed.contains("BKcwgg"), "{scrubbed}");
    assert!(
        scrubbed.starts_with("--- stdout ---\n‹secret:private-key#") && scrubbed.ends_with("›\ndone"),
        "{scrubbed}"
    );
    // A public key is not a secret.
    assert!(kinds("-----BEGIN PUBLIC KEY-----\nMIIBIjANBgkq\n-----END PUBLIC KEY-----").is_empty());
}

/// `-----BEGIN PRIVATE KEY-----⏎MIIEvQ…⏎-----END PRIVATE KEY-----⏎` in base64.
const ENCODED_KEY: &str = "LS0tLS1CRUdJTiBQUklWQVRFIEtFWS0tLS0tCk1JSUV2UUlCQURBTkJna3Foa2lHOXcwQkFRRUZBQVNDQktjd2dnU2pBZ0VBQW9JQkFRQzdWSlRVdDlVczhjS2oKLS0tLS1FTkQgUFJJVkFURSBLRVktLS0tLQo=";

#[test]
fn a_base64_encoded_private_key() {
    // A Kubernetes secret.
    let secret = format!("data:\n  tls.crt: LS0tLS1CRUdJTiBDRVJU\n  tls.key: {ENCODED_KEY}\n");
    assert_eq!(covered(&secret), [ENCODED_KEY]);
    assert_eq!(kinds(&secret), ["private-key"]);
    // Wrapped at 64, as `base64` prints it: every line.
    let lines: Vec<&str> = ENCODED_KEY.as_bytes().chunks(64).map(|c| std::str::from_utf8(c).unwrap()).collect();
    let wrapped = format!("{}\ndone", lines.join("\n"));
    assert_eq!(covered(&wrapped), [lines.join("\n")]);
    // A certificate is not a secret.
    let certificate = "LS0tLS1CRUdJTiBDRVJUSUZJQ0FURS0tLS0tCk1JSURkekNDQWwrZ0F3SUJBZ0lFQWdBQXVUQU5CZ2txaGtpRzl3MEJBUVVGQURCYU1Rc3dDUVlEVlFRR0V3SkoKLS0tLS1FTkQgQ0VSVElGSUNBVEUtLS0tLQo=";
    assert!(kinds(certificate).is_empty());
}

#[test]
fn credentials_after_an_authorization_header() {
    let token = "8f3kD92jLq0vXz7RtY5wPb4N";
    for text in [
        format!("curl -H \"Authorization: Bearer {token}\" https://api.example.com"),
        format!("authorization: bearer {token}"),
        format!("{{\"headers\": {{\"Authorization\": \"Bearer {token}\"}}}}"),
        format!("Authorization: Basic {token}=="),
    ] {
        let found = covered(&text);
        assert_eq!(found.len(), 1, "{text}");
        assert!(found[0].starts_with(token), "{text}: {found:?}");
    }
    for clean in [
        "Authorization: Bearer $TOKEN",
        "Authorization: Bearer ${API_TOKEN}",
        "-H 'Authorization: Bearer <token>'",
        "Authorization: Bearer YOUR_API_TOKEN",
        "the Bearer token8f3kD92jLq0vXz7 goes in a header",
    ] {
        assert!(kinds(clean).is_empty(), "{clean}");
    }
}

#[test]
fn an_azure_connection_string() {
    let key = "Zm9vYmFyYmF6cXV4MTIzNDU2Nzg5MGFiY2RlZg==";
    let text = format!(
        "\"AzureWebJobsStorage\": \"DefaultEndpointsProtocol=https;AccountName=app;AccountKey={key};EndpointSuffix=core.windows.net\""
    );
    assert_eq!(covered(&text), [key]);
    let bus = format!(
        "Endpoint=sb://ns.servicebus.windows.net/;SharedAccessKeyName=RootManageSharedAccessKey;SharedAccessKey={key}"
    );
    assert_eq!(covered(&bus), [key]);
}

#[test]
fn a_placeholder_inside_a_secret_never_turns_it_off() {
    // Placeholders get into files (the agent writes them); a key block or a URL password that
    // holds one is still a secret, and goes whole, placeholder and all.
    let block =
        "-----BEGIN PRIVATE KEY-----\nMIIEvQIBADANBgkqhkiG9w0BAQEFAASCQC7\n‹secret:x#0000›\n-----END PRIVATE KEY-----";
    let scrubbed = scrubber().scrub(&format!("a\n{block}\nb")).text;
    assert!(!scrubbed.contains("MIIEvQ") && !scrubbed.contains("‹secret:x#0000›"), "{scrubbed}");
    assert!(scrubbed.starts_with("a\n‹secret:private-key#") && scrubbed.ends_with("›\nb"), "{scrubbed}");
    let url = "postgres://app:‹secret:x#0000›Xk29vLq8Rt@db/app";
    let scrubbed = scrubber().scrub(url).text;
    assert!(!scrubbed.contains("Xk29vLq8Rt"), "{scrubbed}");
    assert_eq!(placeholders_in(&scrubbed), 1, "one placeholder, not one inside another: {scrubbed}");
    // A placeholder on its own is still left alone.
    assert!(kinds("postgres://app:‹secret:url-password#ab12›@db/app").is_empty());
}

#[test]
fn passwords_in_urls_and_connection_strings() {
    let text = "DATABASE_URL=postgres://app:hunter2@db.internal:5432/main\nredis://:s3cr3t@cache:6379";
    assert_eq!(covered(text), ["hunter2", "s3cr3t"]);
    let scrubbed = scrubber().scrub(text).text;
    assert!(scrubbed.starts_with("DATABASE_URL=postgres://app:‹secret:url-password#"), "{scrubbed}");
    // No password, or a reference to one: untouched.
    for clean in [
        "https://example.com/a:b@c",
        "postgres://app@db/main",
        "postgres://app:${DB_PASSWORD}@db/x",
        "http://user:pass@host/v1",
        "scheme://user:password@host",
    ] {
        assert!(kinds(clean).is_empty(), "{clean}");
    }
}

#[test]
fn the_aws_secret_next_to_its_key_id() {
    let id = fake("AKIA", "IOSFODNN7EXAMPLE");
    let secret = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
    // As `aws configure` writes it, and as a CSV line.
    let ini = format!("[default]\naws_access_key_id = {id}\naws_secret_access_key = {secret}\n");
    assert_eq!(covered(&ini), [id.clone(), secret.to_string()]);
    let csv = format!("User,Key,Secret\nci,{id},{secret}\n");
    assert_eq!(kinds(&csv), ["aws-access-key", "aws-secret-key"]);
}

#[test]
fn high_entropy_values_assigned_to_secret_names() {
    let value = "q7Vb9xLm2Pz4Rt8Kd1Wf";
    for text in [
        format!("API_KEY={value}"),
        format!("export SECRET_TOKEN=\"{value}\""),
        format!("\"client_secret\": \"{value}\""),
        format!("password: '{value}'"),
        format!("db_password := \"{value}\""),
        format!("'auth_token' => '{value}'"),
        format!("export GITHUB_TOKEN={value}  # for CI"),
        format!("  api_key: {value}\r"),
        format!("api_key = {value} ; prod"),
    ] {
        assert_eq!(covered(&text), [value], "{text}");
    }
}

#[test]
fn what_a_coding_agent_reads_passes_untouched() {
    let texts = [
        // Cargo.lock.
        "[[package]]\nname = \"serde\"\nversion = \"1.0.219\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"5f0e2c6ed6606019b4e29e69dbaba95b11854410e5347d525002456dbbb786b6\"\n",
        // package-lock.json.
        r#""node_modules/lodash": {"version": "4.17.21", "resolved": "https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz", "integrity": "sha512-v2kDEe57lecTulaDIuNTPy3Ry4gLGJ6Z1O3vE1krgXZNrsQ+LFTGHVxVjcXPs17LhbZVGedAJv8XZ1tvj5FvSg=="}"#,
        // git log.
        "commit 9fceb02d0ae598e95dc970b74767f19372d61af8\nAuthor: A Dev <dev@example.com>\nDate:   Mon Nov 24 10:00:00 2026 +0530\n\n    fix: tokens are counted once\n",
        // UUIDs.
        "id = 3f2b8c1e-9d4a-4e7b-8f6c-2a1d0e9b7c5f\nsession_id: 7c9e6679-7425-40de-944b-e07fc1f90ae7",
        // A base64 test fixture.
        "const FIXTURE: &str = \"SGVsbG8sIFdvcmxkISBUaGlzIGlzIGEgdGVzdCBmaXh0dXJlIGZvciBiYXNlNjQgZGVjb2Rpbmcu\";",
        // Code that names secrets without holding them.
        "let password: String = read_password()?;\nconst token = req.headers.authorization;\nself.api_key = config.api_key.clone();\nmax_tokens = 4096\ntoken_count: 12345678",
        "let secret: KeychainCredentials = KeychainCredentials::in_memory(config);\nlet token = Credential::BearerToken { token };",
        // Code with secret-ish names: field types, calls, constants, placeholders.
        "fn f(password: &windows::core::PCWSTR, token: D3DVSD_TOKEN_STREAM2) {}\nlet secret = \"CLIENT_HANDSHAKE_TRAFFIC_SECRET\";\nGITHUB_TOKEN=YOUR-GITHUB-TOKEN-2",
        // References and placeholders in config.
        "API_KEY=${API_KEY}\nSECRET_TOKEN=<your-token>\npassword: {{ vault.password }}",
        // C: an assignment ends with `;`.
        "pNew->x1.xTokenize = fts5V1toV2Tokenize;\npGlobal->api.xCreateTokenizer = fts5CreateTokenizer;",
        // Hashes under their names.
        "commit_sha: \"9fceb02d0ae598e95dc970b74767f19372d61af8\"\nsha256sum = \"5f0e2c6ed6606019b4e29e69dbaba95b\"",
        // A file hash from the patch action.
        "--- end of file; hash: cb6112f1c4749bb872768a2125c60119597c2a657661a3618dc11c0a1f68e685 ---",
    ];
    for text in texts {
        assert!(kinds(text).is_empty(), "false positive {:?} in {text}", covered(text));
    }
}

#[test]
fn a_long_minified_line_passes_untouched_and_quickly() {
    let chunk = "function(e,t){var n=e.password,r=t.token;return n&&r?{auth:n,key:r.apiKey}:null},";
    let line = chunk.repeat(64 * 1024 / chunk.len());
    let started = std::time::Instant::now();
    assert!(kinds(&line).is_empty(), "{:?}", covered(&line).first());
    assert!(started.elapsed() < std::time::Duration::from_secs(1), "{:?}", started.elapsed());
}

#[test]
fn a_secret_nixxa_holds_is_found_anywhere_exactly() {
    let scrubber = scrubber();
    let mut known = SecretFingerprints::new([5; 32]);
    // An odd one no format knows.
    known.add("local-key", &Secret::new("plainpassphrase-for-my-proxy"));
    scrubber.set_known(known);
    let text = "curl -u me:plainpassphrase-for-my-proxy https://proxy; echo plainpassphrase-for-my-proxy";
    let found = scrubber.find(text);
    assert_eq!(found.len(), 2);
    assert!(found.iter().all(|f| f.kind == "local-key"), "the exact match wins over url-password: {found:?}");
    assert!(!scrubber.scrub(text).text.contains("plainpassphrase"));
}

#[test]
fn placeholders_are_stable_keyed_and_never_scrubbed_again() {
    let key = fake("AKIA", "IOSFODNN7EXAMPLE");
    let one = scrubber().scrub(&format!("{key} and {key}"));
    let [first, second] = &one.found[..] else { panic!("{one:?}") };
    assert_eq!(first.placeholder, second.placeholder, "the same value, the same placeholder");
    let other = fake("AKIA", "IOSFODNN7EXAMPLF");
    assert_ne!(scrubber().find(&other)[0].placeholder, first.placeholder, "another value, another tag");
    // Keyed: another session's key gives another tag for the same value.
    assert_ne!(Scrubber::with_key([2; 32]).find(&key)[0].placeholder, first.placeholder);
    // Scrubbing the scrubbed text changes nothing.
    assert_eq!(scrubber().scrub(&one.text), Scrubbed { text: one.text.clone(), found: Vec::new() });
    assert_eq!(placeholders_in(&one.text), 2);
    // Even after a secret-ish name.
    let assigned = scrubber().scrub("SECRET_KEY=q7Vb9xLm2Pz4Rt8Kd1Wf").text;
    assert!(scrubber().find(&assigned).is_empty(), "{assigned}");
}

#[test]
fn chosen_items_can_be_sent_as_they_are() {
    let (a, b) = (fake("AKIA", "IOSFODNN7EXAMPLE"), fake("ghp_", &BODY40[..36]));
    let text = format!("{a} {b}");
    let found = scrubber().find(&text);
    let keep = vec![found[1].placeholder.clone()];
    let sent = scrubber().scrub_except(&text, &keep);
    assert_eq!(sent.text, format!("{} {b}", found[0].placeholder));
    assert_eq!(sent.found.len(), 2, "every finding is reported, kept ones too");
}

#[test]
fn non_ascii_text_around_secrets_is_kept_intact() {
    let key = fake("AKIA", "IOSFODNN7EXAMPLE");
    let text = format!("clé: «{key}» — ok\nmot_de_passe=« fine »");
    let scrubbed = scrubber().scrub(&text).text;
    assert!(
        scrubbed.starts_with("clé: «‹secret:aws-access-key#") && scrubbed.ends_with("» — ok\nmot_de_passe=« fine »")
    );
}

/// False positives on a real tree, for the PR: `NIXXA_SCRUB_DIR=<dir> cargo test -p
/// nixxa-controller --release -- --ignored --nocapture what_a_real_tree_would_have_redacted`.
/// Prints each finding's kind and file, and the first characters of the value only.
#[test]
#[ignore = "a survey, run by hand"]
fn what_a_real_tree_would_have_redacted() {
    let Some(root) = std::env::var_os("NIXXA_SCRUB_DIR") else { return };
    let scrubber = scrubber();
    let (mut files, mut bytes, mut found) = (0usize, 0usize, std::collections::BTreeMap::<String, usize>::new());
    let mut stack = vec![std::path::PathBuf::from(root)];
    let started = std::time::Instant::now();
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_dir() {
                if !matches!(entry.file_name().to_str(), Some("target" | ".git")) {
                    stack.push(path);
                }
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            files += 1;
            bytes += text.len();
            for f in scrubber.find(&text) {
                *found.entry(f.kind.clone()).or_default() += 1;
                let value = &text[f.start..f.end];
                let shown: String = value.chars().take(6).collect();
                println!("{:<20} {}  {shown}… ({} chars)", f.kind, path.display(), value.len());
            }
        }
    }
    println!("{files} files, {} MiB in {:?}; found: {found:?}", bytes / (1024 * 1024), started.elapsed());
}
