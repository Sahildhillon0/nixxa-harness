//! The seam between [`KeychainCredentials`](super::KeychainCredentials) and where secrets
//! actually live: the OS keychain in production, memory in tests (tests never touch the real
//! keychain — AGENT.md rule 10).

use std::collections::HashMap;
use std::sync::Mutex;

/// The keychain service every Nixxa entry is filed under is its full name, e.g.
/// `nixxa/anthropic/default`; this is the constant user field.
const USER: &str = "nixxa";

/// Why a store operation failed. Never carries a secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StoreError(pub(crate) String);

/// A flat map of entry names to secret strings.
pub(crate) trait SecretStore: Send + Sync {
    /// The value of an entry, or `None` if there is no such entry.
    fn get(&self, name: &str) -> Result<Option<String>, StoreError>;
    /// Creates or replaces an entry.
    fn set(&self, name: &str, value: &str) -> Result<(), StoreError>;
    /// Deletes an entry; deleting a missing entry is not an error.
    fn delete(&self, name: &str) -> Result<(), StoreError>;
}

/// The OS keychain via the `keyring` crate: Secret Service on Linux, Keychain on macOS,
/// Credential Manager on Windows.
pub(crate) struct OsKeychain;

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
impl OsKeychain {
    fn entry(name: &str) -> Result<keyring::Entry, StoreError> {
        keyring::Entry::new(name, USER).map_err(describe)
    }
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
impl SecretStore for OsKeychain {
    fn get(&self, name: &str) -> Result<Option<String>, StoreError> {
        match Self::entry(name)?.get_password() {
            Ok(value) => Ok(Some(value)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(err) => Err(describe(err)),
        }
    }

    fn set(&self, name: &str, value: &str) -> Result<(), StoreError> {
        Self::entry(name)?.set_password(value).map_err(describe)
    }

    fn delete(&self, name: &str) -> Result<(), StoreError> {
        match Self::entry(name)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(err) => Err(describe(err)),
        }
    }
}

/// Words for a keychain error. Built by hand, because some variants carry the raw bytes of an
/// entry (which could be a secret).
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
fn describe(err: keyring::Error) -> StoreError {
    let message = match err {
        keyring::Error::NoStorageAccess(cause) => format!("the OS keychain is not available ({cause})"),
        keyring::Error::PlatformFailure(cause) => format!("the OS keychain failed ({cause})"),
        keyring::Error::NoEntry => "no such keychain entry".to_string(),
        keyring::Error::BadEncoding(_) => "a keychain entry is not valid UTF-8".to_string(),
        keyring::Error::TooLong(attribute, limit) => format!("keychain {attribute} is longer than {limit}"),
        keyring::Error::Invalid(attribute, reason) => format!("keychain {attribute} is invalid: {reason}"),
        keyring::Error::Ambiguous(_) => "several keychain entries match".to_string(),
        _ => "the OS keychain failed".to_string(),
    };
    StoreError(message)
}

/// No native keychain backend is built for this OS: every operation fails clearly instead of
/// silently keeping secrets in memory.
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
impl SecretStore for OsKeychain {
    fn get(&self, _name: &str) -> Result<Option<String>, StoreError> {
        Err(unsupported())
    }

    fn set(&self, _name: &str, _value: &str) -> Result<(), StoreError> {
        Err(unsupported())
    }

    fn delete(&self, _name: &str) -> Result<(), StoreError> {
        Err(unsupported())
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn unsupported() -> StoreError {
    StoreError("there is no OS keychain backend for this platform".into())
}

/// An in-memory store: for tests and offline demos. Nothing outlives the process.
#[derive(Default)]
pub(crate) struct MemoryStore {
    entries: Mutex<HashMap<String, String>>,
    /// When set, every operation fails with this message (simulates a locked or missing
    /// keychain).
    broken: Mutex<Option<String>>,
}

impl MemoryStore {
    #[cfg(test)]
    pub(crate) fn break_with(&self, message: &str) {
        *self.broken.lock().unwrap() = Some(message.to_string());
    }

    #[cfg(test)]
    pub(crate) fn names(&self) -> Vec<String> {
        let mut names: Vec<_> = self.entries.lock().unwrap().keys().cloned().collect();
        names.sort();
        names
    }

    #[cfg(test)]
    pub(crate) fn raw(&self, name: &str) -> Option<String> {
        self.entries.lock().unwrap().get(name).cloned()
    }

    fn check(&self) -> Result<(), StoreError> {
        match self.broken.lock().unwrap().as_ref() {
            Some(message) => Err(StoreError(message.clone())),
            None => Ok(()),
        }
    }
}

impl SecretStore for MemoryStore {
    fn get(&self, name: &str) -> Result<Option<String>, StoreError> {
        self.check()?;
        Ok(self.entries.lock().unwrap().get(name).cloned())
    }

    fn set(&self, name: &str, value: &str) -> Result<(), StoreError> {
        self.check()?;
        self.entries.lock().unwrap().insert(name.to_string(), value.to_string());
        Ok(())
    }

    fn delete(&self, name: &str) -> Result<(), StoreError> {
        self.check()?;
        self.entries.lock().unwrap().remove(name);
        Ok(())
    }
}
