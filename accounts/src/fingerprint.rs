//! Fingerprints of the secrets held, for the controller's scrubber (#3, D-075): it finds them in
//! text without ever holding them.

use nixxa_contracts::{AccountRef, Credential, CredentialsError, SecretFingerprints};
use ring::rand::{SecureRandom, SystemRandom};

/// A fresh random salt: each call's fingerprints are unlinkable to another's.
pub(crate) fn salt() -> Result<[u8; 32], CredentialsError> {
    let mut salt = [0u8; 32];
    SystemRandom::new()
        .fill(&mut salt)
        .map_err(|_| CredentialsError::Storage("the system's random source is unavailable".into()))?;
    Ok(salt)
}

/// Adds `credential`'s secret, if it has one, labelled after its provider
/// (`anthropic-key`, `openrouter-token`).
pub(crate) fn add(prints: &mut SecretFingerprints, account: &AccountRef, credential: &Credential) {
    let provider: String = account
        .provider
        .as_str()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
        .collect();
    match credential {
        Credential::ApiKey(secret) => prints.add(&format!("{provider}-key"), secret),
        Credential::BearerToken { token, .. } => prints.add(&format!("{provider}-token"), token),
        // A profile name and a base URL aren't secrets.
        Credential::CloudProfile { .. } | Credential::Local { .. } => {}
    }
}

#[cfg(test)]
mod tests {
    use nixxa_contracts::{Credentials, Secret};

    use crate::{EnvKey, KeychainConfig, KeychainCredentials};

    use super::*;

    #[tokio::test]
    async fn every_secret_held_is_fingerprinted_with_its_provider_and_nothing_else_is() {
        let credentials = KeychainCredentials::in_memory(KeychainConfig {
            env_keys: vec![EnvKey { provider: "anthropic".into(), secret: Secret::new("sk-ant-env-0123456789abcdef") }],
        });
        credentials
            .add(
                AccountRef::new("openrouter", "me"),
                Credential::BearerToken { token: Secret::new("sk-or-v1-0123456789abcdef"), expires_at: None },
            )
            .await
            .unwrap();
        credentials
            .add(AccountRef::new("local", "lm"), Credential::Local { base_url: "http://127.0.0.1:1234/v1".into() })
            .await
            .unwrap();
        let prints = credentials.fingerprints().await.unwrap();
        assert_eq!(prints.len(), 2, "the env key and the token; a base URL isn't a secret");
        let text = "a=sk-ant-env-0123456789abcdef b=sk-or-v1-0123456789abcdef c=http://127.0.0.1:1234/v1";
        let kinds: Vec<String> = prints.find(text).into_iter().map(|k| k.kind).collect();
        assert_eq!(kinds, ["anthropic-key", "openrouter-token"]);
    }

    #[test]
    fn salts_differ() {
        assert_ne!(salt().unwrap(), salt().unwrap());
    }
}
