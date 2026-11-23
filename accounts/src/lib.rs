//! `nixxa-accounts` — holds every account and API key, in the OS keychain.
//!
//! Stage 3: [`KeychainCredentials`] stores API-key accounts in the OS keychain and exposes
//! environment-variable keys as read-only accounts. Multi-account switching, sign-in methods
//! and the encrypted-file fallback arrive in Stage 13.

mod keychain;
pub mod memory;
pub mod methods;

pub use keychain::{ENV_ACCOUNT, EnvKey, KeychainConfig, KeychainCredentials};
