//! Files that usually hold secrets (#3, D-076): reading one asks the user first, even in Auto.
//! The one list of them is [`PATTERNS`]. A command that prints one (`cat .env`) can't be told
//! apart by name; the scrubber is the backstop there.

/// File-name patterns, matched case-insensitively against the path's last component, or, for
/// those with a `/`, against its end. `*` matches any run of characters.
pub(crate) const PATTERNS: &[&str] = &[
    ".env",
    ".env.*",
    "*.pem",
    "*.key",
    "*.p12",
    "*.pfx",
    "*.jks",
    "*.keystore",
    "*.ppk",
    "*.kdbx",
    "*.tfvars",
    "*.tfstate",
    "id_*",
    ".npmrc",
    ".pypirc",
    ".netrc",
    "_netrc",
    ".git-credentials",
    ".pgpass",
    ".htpasswd",
    "credentials",
    "*credentials.json",
    "secrets.*",
    ".secrets",
    "kubeconfig",
    ".kube/config",
    ".docker/config.json",
];

/// Endings that mark a template or a public half, not a secret: `.env.example`, `id_rsa.pub`.
const NOT_SECRET: &[&str] = &[".pub", ".example", ".sample", ".template", ".dist"];

/// The pattern `path` matches, if it names a file that usually holds secrets.
pub(crate) fn pattern(path: &str) -> Option<&'static str> {
    let path = path.replace('\\', "/").to_ascii_lowercase();
    let path = path.trim_end_matches('/');
    let name = path.rsplit('/').next().unwrap_or(path);
    if NOT_SECRET.iter().any(|end| name.ends_with(end)) {
        return None;
    }
    PATTERNS.iter().copied().find(|pattern| {
        if pattern.contains('/') {
            path == *pattern || path.ends_with(&format!("/{pattern}"))
        } else {
            glob(pattern, name)
        }
    })
}

/// `*`-only glob matching.
fn glob(pattern: &str, name: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return name == pattern;
    }
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if !name.starts_with(first) || !name.ends_with(last) || name.len() < first.len() + last.len() {
        return false;
    }
    let mut rest = &name[first.len()..name.len() - last.len()];
    for middle in &parts[1..parts.len() - 1] {
        match rest.find(middle) {
            Some(at) => rest = &rest[at + middle.len()..],
            None => return false,
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_that_hold_secrets_are_recognised() {
        for path in [
            ".env",
            "app/.env.production",
            "./certs/server.PEM",
            "tls/private.key",
            "/home/u/.ssh/id_ed25519",
            "id_rsa",
            ".npmrc",
            "pkg/.pypirc",
            ".netrc",
            "gcp/service-account-credentials.json",
            "credentials.json",
            "aws/credentials",
            "config/secrets.yml",
            "infra/prod.tfvars",
            "terraform.tfstate",
            "home/.kube/config",
            "C:\\Users\\me\\.docker\\config.json",
        ] {
            assert!(pattern(path).is_some(), "{path}");
        }
        assert_eq!(pattern("deploy/.env.local"), Some(".env.*"));
    }

    #[test]
    fn ordinary_files_and_public_halves_are_not() {
        for path in [
            "src/main.rs",
            ".env.example",
            ".env.sample",
            "id_rsa.pub",
            "keys.rs",
            "environment.ts",
            ".envrc.md",
            "monkey.txt",
            "docker/config.json",
            "credentials.rs",
            "Cargo.lock",
        ] {
            assert_eq!(pattern(path), None, "{path}");
        }
    }
}
