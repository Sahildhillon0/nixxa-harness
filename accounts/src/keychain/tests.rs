//! Tests for [`KeychainCredentials`]. All of them use the in-memory store; the only test that
//! touches the real OS keychain is `#[ignore]`d and run by hand (AGENT.md rule 10).

use std::sync::Arc;

use nixxa_contracts::conformance::credentials::run;
use nixxa_contracts::{AccountRef, AuthMethod, Credential, Credentials, CredentialsError, ProviderId, Secret};

use super::store::MemoryStore;
use super::{ENV_ACCOUNT, EnvKey, KeychainConfig, KeychainCredentials, REGISTRY};

const KEY: &str = "sk-ant-test-key-very-secret-7f3a";
const ENV_KEY: &str = "sk-ant-env-key-also-secret-9b2c";

fn anthropic(label: &str) -> AccountRef {
    AccountRef::new("anthropic", label)
}

fn api_key(key: &str) -> Credential {
    Credential::ApiKey(Secret::new(key))
}

fn env_config() -> KeychainConfig {
    KeychainConfig { env_keys: vec![EnvKey { provider: ProviderId::new("anthropic"), secret: Secret::new(ENV_KEY) }] }
}

fn over(store: &Arc<MemoryStore>, config: KeychainConfig) -> KeychainCredentials {
    KeychainCredentials::with_store(store.clone(), config)
}

#[tokio::test]
async fn conforms() {
    run(|| KeychainCredentials::in_memory(KeychainConfig::default())).await;
}

#[tokio::test]
async fn conforms_alongside_an_env_account() {
    run(|| KeychainCredentials::in_memory(env_config())).await;
}

#[tokio::test]
async fn accounts_survive_a_restart() {
    let store = Arc::new(MemoryStore::default());
    over(&store, KeychainConfig::default()).add(anthropic("default"), api_key(KEY)).await.unwrap();

    let relaunched = over(&store, KeychainConfig::default());
    assert_eq!(relaunched.get(&anthropic("default")).await.unwrap(), api_key(KEY));
    let provider = ProviderId::new("anthropic");
    assert_eq!(relaunched.default_account(&provider).await.unwrap(), Some(anthropic("default")));
}

#[tokio::test]
async fn secrets_and_registry_use_the_documented_entry_names() {
    let store = Arc::new(MemoryStore::default());
    let credentials = over(&store, KeychainConfig::default());
    credentials.add(anthropic("default"), api_key(KEY)).await.unwrap();

    assert_eq!(store.names(), vec!["nixxa/_registry", "nixxa/anthropic/default"]);
    assert_eq!(store.raw("nixxa/anthropic/default").as_deref(), Some(KEY));
    let registry = store.raw(REGISTRY).unwrap();
    assert!(!registry.contains("very-secret"), "the registry must hold no secret: {registry}");
    assert!(registry.contains("\"hint\":\"7f3a\""), "{registry}");
}

#[tokio::test]
async fn the_env_account_is_listed_read_only_and_never_stored() {
    let store = Arc::new(MemoryStore::default());
    let credentials = over(&store, env_config());
    let env = anthropic(ENV_ACCOUNT);

    let listed = credentials.list(None).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].account, env);
    assert_eq!(listed[0].method, AuthMethod::Env);
    assert!(listed[0].is_default);
    assert_eq!(listed[0].hint.as_deref(), Some("9b2c"));
    assert_eq!(credentials.get(&env).await.unwrap(), api_key(ENV_KEY));
    credentials.validate(&env).await.unwrap();

    assert!(matches!(credentials.remove(&env).await, Err(CredentialsError::Unsupported(_))));
    assert!(matches!(credentials.add(env.clone(), api_key(KEY)).await, Err(CredentialsError::Unsupported(_))));
    assert_eq!(credentials.get(&env).await.unwrap(), api_key(ENV_KEY), "the env key is unchanged");
    assert!(store.names().is_empty(), "nothing was written to the keychain: {:?}", store.names());
}

#[tokio::test]
async fn a_stored_default_wins_over_env_until_env_is_chosen() {
    let credentials = KeychainCredentials::in_memory(env_config());
    let provider = ProviderId::new("anthropic");
    credentials.add(anthropic("work"), api_key(KEY)).await.unwrap();
    assert_eq!(credentials.default_account(&provider).await.unwrap(), Some(anthropic("work")));

    credentials.set_default(&anthropic(ENV_ACCOUNT)).await.unwrap();
    assert_eq!(credentials.default_account(&provider).await.unwrap(), Some(anthropic(ENV_ACCOUNT)));
    let defaults = credentials.list(Some(&provider)).await.unwrap().into_iter().filter(|a| a.is_default).count();
    assert_eq!(defaults, 1);
}

#[tokio::test]
async fn removing_the_default_promotes_another_stored_account() {
    let credentials = KeychainCredentials::in_memory(KeychainConfig::default());
    credentials.add(anthropic("a"), api_key(KEY)).await.unwrap();
    credentials.add(anthropic("b"), api_key(KEY)).await.unwrap();
    credentials.remove(&anthropic("a")).await.unwrap();
    let provider = ProviderId::new("anthropic");
    assert_eq!(credentials.default_account(&provider).await.unwrap(), Some(anthropic("b")));
}

#[tokio::test]
async fn an_unavailable_keychain_is_a_clear_storage_error() {
    let store = Arc::new(MemoryStore::default());
    store.break_with("the OS keychain is not available (no Secret Service)");
    let credentials = over(&store, KeychainConfig::default());

    let Err(CredentialsError::Storage(message)) = credentials.list(None).await else { panic!("expected Storage") };
    assert!(message.contains("not available"), "{message}");
    assert!(matches!(credentials.add(anthropic("default"), api_key(KEY)).await, Err(CredentialsError::Storage(_))));
    assert!(matches!(credentials.get(&anthropic("default")).await, Err(CredentialsError::Storage(_))));
}

#[tokio::test]
async fn env_accounts_still_work_when_the_keychain_is_unavailable() {
    let store = Arc::new(MemoryStore::default());
    store.break_with("the OS keychain is not available");
    let credentials = over(&store, env_config());

    let provider = ProviderId::new("anthropic");
    assert_eq!(credentials.default_account(&provider).await.unwrap(), Some(anthropic(ENV_ACCOUNT)));
    assert_eq!(credentials.get(&anthropic(ENV_ACCOUNT)).await.unwrap(), api_key(ENV_KEY));
}

#[tokio::test]
async fn check_storage_reports_what_list_hides_behind_the_env_account() {
    let store = Arc::new(MemoryStore::default());
    let credentials = over(&store, env_config());
    credentials.check_storage().await.expect("a working store");

    store.break_with("the OS keychain is not available");
    assert_eq!(credentials.list(None).await.unwrap().len(), 1, "list quietly shows only the env account");
    let Err(CredentialsError::Storage(message)) = credentials.check_storage().await else { panic!("expected Storage") };
    assert!(message.contains("not available"), "{message}");
}

#[tokio::test]
async fn an_unreadable_registry_is_reported_and_never_overwritten() {
    let store = Arc::new(MemoryStore::default());
    store_raw(&store, REGISTRY, "{ not json");
    let credentials = over(&store, KeychainConfig::default());

    let Err(CredentialsError::Storage(message)) = credentials.list(None).await else { panic!("expected Storage") };
    assert!(message.contains("unreadable"), "{message}");
    assert!(credentials.add(anthropic("default"), api_key(KEY)).await.is_err());
    assert_eq!(store.raw(REGISTRY).as_deref(), Some("{ not json"));
}

#[tokio::test]
async fn a_registry_from_a_newer_version_is_refused() {
    let store = Arc::new(MemoryStore::default());
    store_raw(&store, REGISTRY, r#"{"version":99,"accounts":[]}"#);
    let credentials = over(&store, KeychainConfig::default());
    assert!(matches!(credentials.list(None).await, Err(CredentialsError::Storage(_))));
}

#[tokio::test]
async fn malformed_keys_are_rejected_without_echoing_them() {
    let credentials = KeychainCredentials::in_memory(KeychainConfig::default());
    for bad in ["", "sk-ant has-a-space", "sk-ant-\x1b[2J", "sk-ant\n"] {
        let err = credentials.add(anthropic("default"), api_key(bad)).await.unwrap_err();
        assert!(matches!(err, CredentialsError::Invalid(_)), "{bad:?} → {err:?}");
        if !bad.is_empty() {
            assert!(!err.to_string().contains(bad), "the error echoes the key: {err}");
        }
    }
    assert!(credentials.list(None).await.unwrap().is_empty());
}

#[tokio::test]
async fn names_that_would_break_the_entry_layout_are_rejected() {
    let credentials = KeychainCredentials::in_memory(KeychainConfig::default());
    for account in [anthropic("a/b"), anthropic(""), AccountRef::new("_registry", "x"), anthropic("_hidden")] {
        let err = credentials.add(account.clone(), api_key(KEY)).await.unwrap_err();
        assert!(matches!(err, CredentialsError::Invalid(_)), "{account} → {err:?}");
    }
}

/// Regression (PR #5 review, S1): Windows Credential Manager matches entry names
/// case-insensitively, so names that differ only in case would share one secret.
#[tokio::test]
async fn names_that_differ_only_in_case_cannot_coexist() {
    let credentials = KeychainCredentials::in_memory(KeychainConfig::default());
    credentials.add(anthropic("work"), api_key(KEY)).await.unwrap();
    for account in [anthropic("Work"), anthropic("WORK"), AccountRef::new("Anthropic", "work")] {
        let err = credentials.add(account.clone(), api_key(ENV_KEY)).await.unwrap_err();
        let CredentialsError::Invalid(message) = &err else { panic!("{account} → {err:?}") };
        assert!(message.contains("lower-case"), "{message}");
    }
    assert_eq!(credentials.get(&anthropic("work")).await.unwrap(), api_key(KEY), "the original key is intact");
    assert_eq!(credentials.list(None).await.unwrap().len(), 1);
}

#[tokio::test]
async fn only_api_keys_and_local_servers_are_stored_in_stage_3() {
    let credentials = KeychainCredentials::in_memory(KeychainConfig::default());
    let profile = Credential::CloudProfile { profile: "default".into() };
    let err = credentials.add(AccountRef::new("bedrock", "work"), profile).await.unwrap_err();
    assert!(matches!(err, CredentialsError::Unsupported(_)), "{err:?}");
}

#[tokio::test]
async fn a_local_server_is_stored_without_a_secret_and_survives_a_restart() {
    let store = Arc::new(MemoryStore::default());
    let local = AccountRef::new("local", "default");
    let credential = Credential::Local { base_url: "http://localhost:1234/v1".into() };
    let info = over(&store, KeychainConfig::default()).add(local.clone(), credential.clone()).await.unwrap();
    assert_eq!(info.method, AuthMethod::Local);
    assert_eq!(info.hint.as_deref(), Some("http://localhost:1234/v1"));
    assert!(info.is_default);
    assert_eq!(store.names(), vec!["nixxa/_registry"], "no secret entry for a local server");

    let relaunched = over(&store, KeychainConfig::default());
    assert_eq!(relaunched.get(&local).await.unwrap(), credential);
    relaunched.validate(&local).await.unwrap();
    relaunched.remove(&local).await.unwrap();
    assert!(relaunched.list(None).await.unwrap().is_empty());
}

#[tokio::test]
async fn replacing_a_key_with_a_local_server_deletes_the_key() {
    let store = Arc::new(MemoryStore::default());
    let credentials = over(&store, KeychainConfig::default());
    let account = AccountRef::new("local", "default");
    credentials.add(account.clone(), api_key(KEY)).await.unwrap();
    credentials.add(account.clone(), Credential::Local { base_url: "http://127.0.0.1:11434/v1".into() }).await.unwrap();
    assert_eq!(store.names(), vec!["nixxa/_registry"], "the old key is gone");
}

#[tokio::test]
async fn bad_base_urls_are_refused() {
    let credentials = KeychainCredentials::in_memory(KeychainConfig::default());
    for url in [
        "localhost:1234",
        "ftp://host/v1",
        "http://",
        "http://user:pass@host/v1",
        "http://host/ v1",
        "http://host/\x1b[2J",
    ] {
        let err = credentials
            .add(AccountRef::new("local", "default"), Credential::Local { base_url: url.into() })
            .await
            .unwrap_err();
        assert!(matches!(err, CredentialsError::Invalid(_)), "{url:?} → {err:?}");
        assert!(!err.to_string().contains('\x1b'), "escaped in the message: {err}");
    }
}

#[tokio::test]
async fn no_secret_reaches_debug_output_or_error_text() {
    let store = Arc::new(MemoryStore::default());
    let credentials = over(&store, env_config());
    credentials.add(anthropic("default"), api_key(KEY)).await.unwrap();

    let mut shown = vec![
        format!("{credentials:?}"),
        format!("{:?}", env_config()),
        format!("{:?}", credentials.list(None).await.unwrap()),
        format!("{:?}", credentials.get(&anthropic("default")).await.unwrap()),
        format!("{:?}", credentials.get(&anthropic(ENV_ACCOUNT)).await.unwrap()),
    ];
    // Every error path, in both Debug and Display.
    let mut errors = vec![
        credentials.add(anthropic(ENV_ACCOUNT), api_key(KEY)).await.unwrap_err(),
        credentials.remove(&anthropic(ENV_ACCOUNT)).await.unwrap_err(),
        credentials.add(anthropic("x"), api_key(&format!("{KEY} {ENV_KEY}"))).await.unwrap_err(),
        credentials.get(&anthropic("ghost")).await.unwrap_err(),
    ];
    store.break_with("the OS keychain failed");
    errors.push(credentials.get(&anthropic("default")).await.unwrap_err());
    for err in errors {
        shown.push(format!("{err:?}"));
        shown.push(err.to_string());
    }
    for text in shown {
        for secret in [KEY, ENV_KEY, "very-secret", "also-secret"] {
            assert!(!text.contains(secret), "{secret:?} leaked into {text:?}");
        }
    }
}

fn store_raw(store: &MemoryStore, name: &str, value: &str) {
    use super::store::SecretStore;
    store.set(name, value).unwrap();
}

/// Round-trips one throwaway entry through the REAL OS keychain backend. Run by hand only:
/// `cargo test -p nixxa-accounts -- --ignored real_keychain`.
/// It never touches the account registry or a real account, and deletes its entry.
#[test]
#[ignore = "touches the real OS keychain"]
fn real_keychain_round_trip() {
    use super::store::{OsKeychain, SecretStore};

    let name = format!("nixxa/nixxa-manual-test/{}", std::process::id());
    OsKeychain.set(&name, KEY).expect("write to the OS keychain");
    let read = OsKeychain.get(&name);
    OsKeychain.delete(&name).expect("clean up");
    assert_eq!(read.expect("read back").as_deref(), Some(KEY));
    assert_eq!(OsKeychain.get(&name).expect("read after delete"), None);
}
