//! The [`Credentials`] contract and secret types.

use std::fmt;
use std::str::FromStr;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

use crate::{CredentialsError, ProviderId, Timestamp};

/// Names an account: provider + label, written `provider:account` (e.g. `anthropic:work`).
/// Not a secret.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AccountRef {
    /// The provider.
    pub provider: ProviderId,
    /// The account label, unique per provider.
    pub account: String,
}

impl AccountRef {
    /// Builds a reference.
    pub fn new(provider: impl Into<ProviderId>, account: impl Into<String>) -> Self {
        Self { provider: provider.into(), account: account.into() }
    }
}

impl fmt::Display for AccountRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.provider, self.account)
    }
}

/// Error parsing an [`AccountRef`] from `provider:account`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("expected `provider:account`, got `{0}`")]
pub struct ParseAccountRefError(pub String);

impl FromStr for AccountRef {
    type Err = ParseAccountRefError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.split_once(':') {
            Some((p, a)) if !p.is_empty() && !a.is_empty() => Ok(Self::new(p, a)),
            _ => Err(ParseAccountRefError(s.to_owned())),
        }
    }
}

/// How an account authenticates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMethod {
    /// An API key.
    ApiKey,
    /// A cloud provider profile (Bedrock, Vertex AI, Azure / Foundry).
    Cloud,
    /// OAuth sign-in, only where the provider permits third-party apps.
    SignIn,
    /// A local server, no secret.
    Local,
    /// Read from an environment variable at startup; read-only.
    Env,
}

/// A secret string. Redacted in `Debug`, zeroised on drop, deliberately not serialisable.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// Wraps a secret.
    pub fn new(secret: impl Into<String>) -> Self {
        Self(secret.into())
    }

    /// The secret itself. Call only at the point of use (e.g. building a request header).
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// The last four characters, for display (`…abcd`). Empty for secrets shorter than 8.
    pub fn hint(&self) -> String {
        let n = self.0.chars().count();
        if n < 8 {
            return String::new();
        }
        self.0.chars().skip(n - 4).collect()
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(***)")
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

/// A usable credential, handed out by [`Credentials::get`]. Not serialisable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Credential {
    /// An API key.
    ApiKey(Secret),
    /// An OAuth access token.
    BearerToken {
        /// The token.
        token: Secret,
        /// Expiry, if known.
        expires_at: Option<Timestamp>,
    },
    /// A reference to a cloud profile; Nixxa never copies the cloud's credentials.
    CloudProfile {
        /// Profile name.
        profile: String,
    },
    /// A local server.
    Local {
        /// Base URL.
        base_url: String,
    },
}

impl Credential {
    /// The auth method this credential implies.
    pub fn method(&self) -> AuthMethod {
        match self {
            Self::ApiKey(_) => AuthMethod::ApiKey,
            Self::BearerToken { .. } => AuthMethod::SignIn,
            Self::CloudProfile { .. } => AuthMethod::Cloud,
            Self::Local { .. } => AuthMethod::Local,
        }
    }

    /// The non-secret display hint stored in [`AccountInfo::hint`].
    pub fn display_hint(&self) -> Option<String> {
        match self {
            Self::ApiKey(s) | Self::BearerToken { token: s, .. } => Some(s.hint()).filter(|h| !h.is_empty()),
            Self::CloudProfile { profile } => Some(profile.clone()),
            Self::Local { base_url } => Some(base_url.clone()),
        }
    }
}

/// An account, as listed. Contains no secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountInfo {
    /// Which account.
    pub account: AccountRef,
    /// How it authenticates.
    pub method: AuthMethod,
    /// Whether it is the provider's default.
    pub is_default: bool,
    /// Non-secret display hint: last four characters of a key, profile name or base URL.
    pub hint: Option<String>,
}

/// Holds every account and hands out credentials.
///
/// Contract:
/// * [`Credentials::add`] on an existing account replaces its credential (key rotation);
/// * at most one default per provider; [`Credentials::set_default`] moves it;
/// * the first account added for a provider becomes its default;
/// * unknown accounts return [`CredentialsError::NotFound`];
/// * [`Credentials::validate`] checks presence, shape and expiry locally — a live check
///   against the provider belongs to the brain;
/// * secrets never appear in [`AccountInfo`] or in any `Debug` output.
#[async_trait]
pub trait Credentials: Send + Sync {
    /// Lists accounts, optionally for one provider.
    async fn list(&self, provider: Option<&ProviderId>) -> Result<Vec<AccountInfo>, CredentialsError>;

    /// Returns a usable credential, refreshing it first if needed.
    async fn get(&self, account: &AccountRef) -> Result<Credential, CredentialsError>;

    /// The provider's default account, if any.
    async fn default_account(&self, provider: &ProviderId) -> Result<Option<AccountRef>, CredentialsError>;

    /// Adds or replaces an account.
    async fn add(&self, account: AccountRef, credential: Credential) -> Result<AccountInfo, CredentialsError>;

    /// Removes an account (and revokes sign-in tokens where supported).
    async fn remove(&self, account: &AccountRef) -> Result<(), CredentialsError>;

    /// Makes an account its provider's default.
    async fn set_default(&self, account: &AccountRef) -> Result<(), CredentialsError>;

    /// Local validity check.
    async fn validate(&self, account: &AccountRef) -> Result<(), CredentialsError>;

    /// Runs an interactive sign-in flow. Unsupported unless the implementation and the
    /// provider's terms allow it.
    async fn sign_in(&self, account: AccountRef) -> Result<AccountInfo, CredentialsError> {
        Err(CredentialsError::Unsupported(format!("sign-in for `{account}`")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_ref_round_trips() {
        let r: AccountRef = "anthropic:work".parse().unwrap();
        assert_eq!(r, AccountRef::new("anthropic", "work"));
        assert_eq!(r.to_string(), "anthropic:work");
        assert!("anthropic".parse::<AccountRef>().is_err());
        assert!(":work".parse::<AccountRef>().is_err());
        assert!("anthropic:".parse::<AccountRef>().is_err());
    }

    #[test]
    fn secret_is_redacted() {
        let c = Credential::ApiKey(Secret::new("sk-very-secret-1234"));
        let shown = format!("{c:?}");
        assert!(!shown.contains("very-secret"), "{shown}");
        assert_eq!(c.display_hint().as_deref(), Some("1234"));
        assert_eq!(Secret::new("short").hint(), "");
    }

    #[test]
    fn hint_is_char_safe() {
        assert_eq!(Secret::new("ключ-доступа-éé12").hint(), "éé12");
    }
}
