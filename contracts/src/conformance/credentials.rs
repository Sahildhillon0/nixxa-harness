//! Conformance suite for [`Credentials`].

use crate::{AccountRef, AuthMethod, Credential, Credentials, CredentialsError, ProviderId, Secret};

const PROVIDER: &str = "conformance-test";

fn acct(name: &str) -> AccountRef {
    AccountRef::new(PROVIDER, name)
}

fn key(s: &str) -> Credential {
    Credential::ApiKey(Secret::new(s))
}

/// Runs every scenario, each on a fresh store from `fresh`. Uses the provider id
/// `conformance-test`, so it is safe against a store that also holds real accounts.
pub async fn run<C: Credentials, F: Fn() -> C>(fresh: F) {
    add_get_list(&fresh()).await;
    defaults(&fresh()).await;
    replace_and_remove(&fresh()).await;
    unknown_is_not_found(&fresh()).await;
}

async fn add_get_list(c: &dyn Credentials) {
    let info = c.add(acct("a"), key("sk-conformance-aaaa-1111")).await.expect("add");
    assert_eq!(info.account, acct("a"));
    assert_eq!(info.method, AuthMethod::ApiKey);
    assert_eq!(info.hint.as_deref(), Some("1111"), "hint must be the last four characters");
    assert!(info.is_default, "first account for a provider becomes default");

    let got = c.get(&acct("a")).await.expect("get");
    assert_eq!(got, key("sk-conformance-aaaa-1111"));
    assert!(!format!("{got:?}").contains("conformance-aaaa"), "Debug must not reveal the secret");

    let listed = c.list(Some(&ProviderId::new(PROVIDER))).await.expect("list");
    assert!(listed.iter().any(|i| i.account == acct("a")));
    assert!(listed.iter().all(|i| i.account.provider.as_str() == PROVIDER), "provider filter must apply");
    for i in &listed {
        assert!(!format!("{i:?}").contains("conformance-aaaa"), "AccountInfo must not carry secrets");
    }
    c.validate(&acct("a")).await.expect("validate");
}

async fn defaults(c: &dyn Credentials) {
    c.add(acct("a"), key("sk-conformance-aaaa-1111")).await.unwrap();
    c.add(acct("b"), key("sk-conformance-bbbb-2222")).await.unwrap();
    let provider = ProviderId::new(PROVIDER);
    assert_eq!(c.default_account(&provider).await.unwrap(), Some(acct("a")));
    c.set_default(&acct("b")).await.expect("set_default");
    assert_eq!(c.default_account(&provider).await.unwrap(), Some(acct("b")));
    let defaults: Vec<_> = c.list(Some(&provider)).await.unwrap().into_iter().filter(|i| i.is_default).collect();
    assert_eq!(defaults.len(), 1, "exactly one default per provider");
}

async fn replace_and_remove(c: &dyn Credentials) {
    c.add(acct("a"), key("sk-conformance-old-0000")).await.unwrap();
    c.add(acct("a"), key("sk-conformance-new-9999")).await.unwrap();
    assert_eq!(c.get(&acct("a")).await.unwrap(), key("sk-conformance-new-9999"), "add must replace (rotation)");
    let count = c.list(Some(&ProviderId::new(PROVIDER))).await.unwrap().len();
    assert_eq!(count, 1, "replacing must not duplicate the account");
    c.remove(&acct("a")).await.expect("remove");
    assert!(matches!(c.get(&acct("a")).await, Err(CredentialsError::NotFound(_))));
    assert!(c.list(Some(&ProviderId::new(PROVIDER))).await.unwrap().is_empty());
}

async fn unknown_is_not_found(c: &dyn Credentials) {
    let ghost = acct("ghost");
    assert!(matches!(c.get(&ghost).await, Err(CredentialsError::NotFound(_))));
    assert!(matches!(c.remove(&ghost).await, Err(CredentialsError::NotFound(_))));
    assert!(matches!(c.set_default(&ghost).await, Err(CredentialsError::NotFound(_))));
}
