//! [`KeychainCredentials`]: API-key accounts kept in the OS keychain, plus read-only accounts
//! from environment variables.
//!
//! Layout in the keychain (no file is ever written):
//! * `nixxa/<provider>/<account>` holds one account's secret;
//! * `nixxa/_registry` holds the non-secret account list as JSON (provider, label, method,
//!   default, hint), so nothing besides the keychain is needed (D-022).

mod store;

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use nixxa_contracts::{
    AccountInfo, AccountRef, AuthMethod, Credential, Credentials, CredentialsError, ProviderId, Secret,
};

use store::{MemoryStore, OsKeychain, SecretStore, StoreError};

/// The keychain entry that holds the account registry.
const REGISTRY: &str = "nixxa/_registry";
/// The registry format written by this version.
const REGISTRY_VERSION: u32 = 1;
/// The label every environment-variable account gets: `<provider>:env`.
pub const ENV_ACCOUNT: &str = "env";

/// An API key the harness read from an environment variable (e.g. `ANTHROPIC_API_KEY`).
/// The accounts module never reads the environment itself (D-007).
pub struct EnvKey {
    /// The provider the key is for.
    pub provider: ProviderId,
    /// The key.
    pub secret: Secret,
}

impl fmt::Debug for EnvKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EnvKey").field("provider", &self.provider).field("secret", &self.secret).finish()
    }
}

/// Settings for [`KeychainCredentials`].
#[derive(Debug, Default)]
pub struct KeychainConfig {
    /// Keys from environment variables, exposed as read-only `<provider>:env` accounts. They
    /// are never written to the keychain.
    pub env_keys: Vec<EnvKey>,
}

/// The non-secret account list, as stored in [`REGISTRY`].
#[derive(Debug, Default, Serialize, Deserialize)]
struct Registry {
    version: u32,
    accounts: Vec<AccountInfo>,
    /// Non-secret settings the harness keeps with the accounts (e.g. which account to start
    /// with, a local account's model). Absent in registries written before it existed.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    settings: BTreeMap<String, String>,
}

/// API-key accounts in the OS keychain, plus read-only environment accounts.
///
/// * Stage 3 stores API keys only; other credential kinds arrive with their sign-in methods
///   (Stage 8) and are refused with [`CredentialsError::Unsupported`].
/// * An environment account is its provider's default whenever no stored account is.
/// * A keychain that cannot be reached (e.g. headless Linux without a Secret Service) gives
///   [`CredentialsError::Storage`]; environment accounts keep working.
/// * Blocking keychain calls run on tokio's blocking pool, so this must be used from inside a
///   tokio runtime.
pub struct KeychainCredentials {
    inner: Arc<Inner>,
}

struct Inner {
    store: Arc<dyn SecretStore>,
    env: Vec<(AccountRef, Secret)>,
    /// Serialises read-modify-write of the registry.
    lock: Mutex<()>,
}

impl fmt::Debug for KeychainCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let env: Vec<String> = self.inner.env.iter().map(|(account, _)| account.to_string()).collect();
        f.debug_struct("KeychainCredentials").field("env_accounts", &env).finish_non_exhaustive()
    }
}

impl KeychainCredentials {
    /// Accounts in the OS keychain.
    pub fn new(config: KeychainConfig) -> Self {
        Self::with_store(Arc::new(OsKeychain), config)
    }

    /// Accounts kept in memory only, for tests and offline demos: behaves like the keychain
    /// but nothing outlives the process and the OS keychain is never touched.
    pub fn in_memory(config: KeychainConfig) -> Self {
        Self::with_store(Arc::new(MemoryStore::default()), config)
    }

    fn with_store(store: Arc<dyn SecretStore>, config: KeychainConfig) -> Self {
        let mut env: Vec<(AccountRef, Secret)> = Vec::new();
        for key in config.env_keys {
            let account = AccountRef::new(key.provider, ENV_ACCOUNT);
            if !env.iter().any(|(existing, _)| existing == &account) {
                env.push((account, key.secret));
            }
        }
        Self { inner: Arc::new(Inner { store, env, lock: Mutex::new(()) }) }
    }

    /// A non-secret setting kept with the accounts, e.g. `active_account`.
    pub async fn setting(&self, key: &str) -> Result<Option<String>, CredentialsError> {
        check_setting_key(key)?;
        let key = key.to_string();
        self.blocking(move |inner| Ok(inner.load()?.settings.get(&key).cloned())).await
    }

    /// Sets (or, with `None`, clears) a non-secret setting kept with the accounts. Never put a
    /// secret here: settings are stored next to the account list, in plain JSON.
    pub async fn set_setting(&self, key: &str, value: Option<&str>) -> Result<(), CredentialsError> {
        check_setting_key(key)?;
        if let Some(value) = value {
            if value.len() > 1024 || value.chars().any(char::is_control) {
                return Err(CredentialsError::Invalid(format!(
                    "the value of setting `{key}` is too long or not one line"
                )));
            }
        }
        let (key, value) = (key.to_string(), value.map(str::to_string));
        self.blocking(move |inner| {
            let mut registry = inner.load()?;
            match value {
                Some(value) => registry.settings.insert(key, value),
                None => registry.settings.remove(&key),
            };
            inner.save(&registry)
        })
        .await
    }

    /// Checks that the keychain and the account registry can be read. `list` falls back to
    /// environment accounts when they can't, so the harness calls this to tell the user why the
    /// stored accounts are missing.
    pub async fn check_storage(&self) -> Result<(), CredentialsError> {
        self.blocking(|inner| inner.load().map(|_| ())).await
    }

    /// Runs `f` on the blocking pool with the registry lock held.
    async fn blocking<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Inner) -> Result<T, CredentialsError> + Send + 'static,
    ) -> Result<T, CredentialsError> {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || {
            let _guard = inner.lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            f(&inner)
        })
        .await
        .map_err(|_| CredentialsError::Storage("the keychain task stopped unexpectedly".into()))?
    }
}

impl Inner {
    fn env_secret(&self, account: &AccountRef) -> Option<&Secret> {
        self.env.iter().find(|(env, _)| env == account).map(|(_, secret)| secret)
    }

    fn is_env(&self, account: &AccountRef) -> bool {
        account.account == ENV_ACCOUNT && self.env_secret(account).is_some()
    }

    fn load(&self) -> Result<Registry, CredentialsError> {
        let Some(json) = self.store.get(REGISTRY).map_err(storage)? else { return Ok(Registry::default()) };
        let registry: Registry = serde_json::from_str(&json)
            .map_err(|_| CredentialsError::Storage(format!("the account registry ({REGISTRY}) is unreadable")))?;
        if registry.version > REGISTRY_VERSION {
            return Err(CredentialsError::Storage(format!(
                "the account registry was written by a newer Nixxa (format {})",
                registry.version
            )));
        }
        Ok(registry)
    }

    fn save(&self, registry: &Registry) -> Result<(), CredentialsError> {
        let registry = Registry {
            version: REGISTRY_VERSION,
            accounts: registry.accounts.clone(),
            settings: registry.settings.clone(),
        };
        let json = serde_json::to_string(&registry)
            .map_err(|_| CredentialsError::Storage("could not encode the account registry".into()))?;
        self.store.set(REGISTRY, &json).map_err(storage)
    }

    /// Stored accounts plus environment accounts; an environment account is its provider's
    /// default when no stored account is.
    fn all(&self) -> Result<Vec<AccountInfo>, CredentialsError> {
        let mut accounts = self.load()?.accounts;
        for (account, secret) in &self.env {
            let has_default = accounts.iter().any(|a| a.account.provider == account.provider && a.is_default);
            accounts.push(AccountInfo {
                account: account.clone(),
                method: AuthMethod::Env,
                is_default: !has_default,
                hint: Some(secret.hint()).filter(|hint| !hint.is_empty()),
            });
        }
        Ok(accounts)
    }

    /// Everything except environment accounts can only be read from the keychain if the
    /// registry is reachable; environment accounts work without it.
    fn all_or_env(&self) -> Result<Vec<AccountInfo>, CredentialsError> {
        match self.all() {
            Ok(accounts) => Ok(accounts),
            Err(err) if self.env.is_empty() => Err(err),
            Err(_) => Ok(self
                .env
                .iter()
                .map(|(account, secret)| AccountInfo {
                    account: account.clone(),
                    method: AuthMethod::Env,
                    is_default: true,
                    hint: Some(secret.hint()).filter(|hint| !hint.is_empty()),
                })
                .collect()),
        }
    }

    fn get(&self, account: &AccountRef) -> Result<Credential, CredentialsError> {
        if let Some(secret) = self.env_secret(account) {
            return Ok(Credential::ApiKey(secret.clone()));
        }
        let registry = self.load()?;
        let info = registry
            .accounts
            .iter()
            .find(|a| &a.account == account)
            .ok_or_else(|| CredentialsError::NotFound(account.clone()))?;
        if info.method == AuthMethod::Local {
            // A local server has no secret: its base URL is the registry hint.
            let base_url = info.hint.clone().ok_or_else(|| {
                CredentialsError::Storage(format!("the base URL of `{account}` is missing from the registry"))
            })?;
            return Ok(Credential::Local { base_url });
        }
        match self.store.get(&entry_name(account)).map_err(storage)? {
            Some(value) => Ok(Credential::ApiKey(Secret::new(value))),
            None => Err(CredentialsError::Storage(format!("the keychain entry for `{account}` is missing"))),
        }
    }

    fn add(&self, account: AccountRef, credential: Credential) -> Result<AccountInfo, CredentialsError> {
        if account.account == ENV_ACCOUNT {
            return Err(CredentialsError::Unsupported(format!(
                "`{account}` is reserved for the {} environment variable and is read-only",
                account.provider
            )));
        }
        check_names(&account)?;
        let secret = match &credential {
            Credential::ApiKey(secret) => {
                check_key(secret)?;
                Some(secret)
            }
            Credential::Local { base_url } => {
                check_base_url(base_url)?;
                None
            }
            other => {
                return Err(CredentialsError::Unsupported(format!(
                    "storing {:?} credentials (Stage 3 stores API keys and local servers only)",
                    other.method()
                )));
            }
        };

        let mut registry = self.load()?;
        let existing = registry.accounts.iter().position(|a| a.account == account);
        let first = !registry.accounts.iter().any(|a| a.account.provider == account.provider);
        let is_default = match existing {
            Some(n) => registry.accounts.remove(n).is_default,
            None => first,
        };
        let info = AccountInfo { account, method: credential.method(), is_default, hint: credential.display_hint() };
        // The secret first: a registry entry must never point at a missing secret.
        if let Some(secret) = secret {
            self.store.set(&entry_name(&info.account), secret.expose()).map_err(storage)?;
        }
        registry.accounts.push(info.clone());
        self.save(&registry)?;
        if secret.is_none() {
            // An API key replaced by a local server leaves no secret behind.
            self.store.delete(&entry_name(&info.account)).map_err(storage)?;
        }
        Ok(info)
    }

    fn remove(&self, account: &AccountRef) -> Result<(), CredentialsError> {
        if self.is_env(account) {
            return Err(CredentialsError::Unsupported(format!(
                "`{account}` comes from an environment variable and cannot be removed"
            )));
        }
        let mut registry = self.load()?;
        let n = registry
            .accounts
            .iter()
            .position(|a| &a.account == account)
            .ok_or_else(|| CredentialsError::NotFound(account.clone()))?;
        let removed = registry.accounts.remove(n);
        // Keep a default for the provider if any stored account is left.
        if removed.is_default {
            if let Some(next) = registry.accounts.iter_mut().find(|a| a.account.provider == account.provider) {
                next.is_default = true;
            }
        }
        // The registry first: an orphaned secret is harmless, a dangling registry entry is not.
        self.save(&registry)?;
        self.store.delete(&entry_name(account)).map_err(storage)
    }

    fn set_default(&self, account: &AccountRef) -> Result<(), CredentialsError> {
        let mut registry = self.load()?;
        let env = self.is_env(account);
        if !env && !registry.accounts.iter().any(|a| &a.account == account) {
            return Err(CredentialsError::NotFound(account.clone()));
        }
        // Choosing the environment account clears every stored default for the provider.
        for info in registry.accounts.iter_mut().filter(|a| a.account.provider == account.provider) {
            info.is_default = &info.account == account;
        }
        self.save(&registry)
    }
}

#[async_trait]
impl Credentials for KeychainCredentials {
    async fn list(&self, provider: Option<&ProviderId>) -> Result<Vec<AccountInfo>, CredentialsError> {
        let provider = provider.cloned();
        let accounts = self.blocking(Inner::all_or_env).await?;
        Ok(accounts.into_iter().filter(|a| provider.as_ref().is_none_or(|p| &a.account.provider == p)).collect())
    }

    async fn get(&self, account: &AccountRef) -> Result<Credential, CredentialsError> {
        let account = account.clone();
        self.blocking(move |inner| inner.get(&account)).await
    }

    async fn default_account(&self, provider: &ProviderId) -> Result<Option<AccountRef>, CredentialsError> {
        let accounts = self.list(Some(provider)).await?;
        Ok(accounts.into_iter().find(|a| a.is_default).map(|a| a.account))
    }

    async fn add(&self, account: AccountRef, credential: Credential) -> Result<AccountInfo, CredentialsError> {
        self.blocking(move |inner| inner.add(account, credential)).await
    }

    async fn remove(&self, account: &AccountRef) -> Result<(), CredentialsError> {
        let account = account.clone();
        self.blocking(move |inner| inner.remove(&account)).await
    }

    async fn set_default(&self, account: &AccountRef) -> Result<(), CredentialsError> {
        let account = account.clone();
        self.blocking(move |inner| inner.set_default(&account)).await
    }

    async fn validate(&self, account: &AccountRef) -> Result<(), CredentialsError> {
        match self.get(account).await? {
            Credential::ApiKey(secret) => check_key(&secret),
            Credential::Local { base_url } => check_base_url(&base_url),
            _ => Ok(()),
        }
    }
}

/// The keychain entry holding an account's secret.
fn entry_name(account: &AccountRef) -> String {
    format!("nixxa/{}/{}", account.provider, account.account)
}

/// Provider and label become part of a keychain entry name, so keep them plain. Lower-case
/// only: Windows Credential Manager matches names case-insensitively, so `anthropic:Work` and
/// `anthropic:work` would share one secret.
fn check_names(account: &AccountRef) -> Result<(), CredentialsError> {
    let ok = |s: &str| {
        !s.is_empty()
            && s.len() <= 64
            && !s.starts_with('_')
            && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.'))
    };
    if ok(account.provider.as_str()) && ok(&account.account) {
        Ok(())
    } else {
        Err(CredentialsError::Invalid(format!(
            "`{account}`: provider and label must be 1–64 lower-case letters, digits, `-`, `_` or `.`, \
             not starting with `_`"
        )))
    }
}

/// A local shape check on an API key. The message never contains the key.
fn check_key(secret: &Secret) -> Result<(), CredentialsError> {
    let key = secret.expose();
    if key.is_empty() {
        return Err(CredentialsError::Invalid("the API key is empty".into()));
    }
    if key.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(CredentialsError::Invalid("the API key contains spaces or control characters".into()));
    }
    Ok(())
}

/// Setting names are plain: letters, digits, `.`, `_`, `-`, `:`, up to 128 characters.
fn check_setting_key(key: &str) -> Result<(), CredentialsError> {
    let ok = !key.is_empty()
        && key.len() <= 128
        && key.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':'));
    if ok {
        Ok(())
    } else {
        Err(CredentialsError::Invalid(format!("`{}` is not a valid setting name", key.escape_debug())))
    }
}

/// A local server's base URL: `http://` or `https://`, no credentials in it, no spaces or
/// control characters. Not a secret, so it may appear in messages.
fn check_base_url(url: &str) -> Result<(), CredentialsError> {
    let invalid = |why: &str| CredentialsError::Invalid(format!("base URL `{}`: {why}", url.escape_debug()));
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .ok_or_else(|| invalid("must start with http:// or https://"))?;
    if url.len() > 2048 || url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(invalid("must be one line without spaces"));
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.is_empty() {
        return Err(invalid("has no host"));
    }
    if authority.contains('@') {
        return Err(invalid("must not contain a user name or password; store a key separately"));
    }
    Ok(())
}

fn storage(err: StoreError) -> CredentialsError {
    CredentialsError::Storage(err.0)
}

#[cfg(test)]
mod tests;
