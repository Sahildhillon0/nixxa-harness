//! Fakes for the accounts contract.

use std::sync::Mutex;

use async_trait::async_trait;

use nixxa_contracts::{AccountInfo, AccountRef, Credential, Credentials, CredentialsError, ProviderId};

/// An in-memory [`Credentials`] fake.
#[derive(Debug, Default)]
pub struct MemoryCredentials {
    accounts: Mutex<Vec<(AccountInfo, Credential)>>,
}

#[async_trait]
impl Credentials for MemoryCredentials {
    async fn list(&self, provider: Option<&ProviderId>) -> Result<Vec<AccountInfo>, CredentialsError> {
        let all = self.accounts.lock().unwrap();
        Ok(all
            .iter()
            .map(|(info, _)| info.clone())
            .filter(|info| provider.is_none_or(|p| &info.account.provider == p))
            .collect())
    }

    async fn get(&self, account: &AccountRef) -> Result<Credential, CredentialsError> {
        let all = self.accounts.lock().unwrap();
        all.iter()
            .find(|(info, _)| info.account == *account)
            .map(|(_, credential)| credential.clone())
            .ok_or_else(|| CredentialsError::NotFound(account.clone()))
    }

    async fn default_account(&self, provider: &ProviderId) -> Result<Option<AccountRef>, CredentialsError> {
        let all = self.accounts.lock().unwrap();
        Ok(all
            .iter()
            .find(|(info, _)| info.account.provider == *provider && info.is_default)
            .map(|(info, _)| info.account.clone()))
    }

    async fn add(&self, account: AccountRef, credential: Credential) -> Result<AccountInfo, CredentialsError> {
        let mut all = self.accounts.lock().unwrap();
        let first = !all.iter().any(|(info, _)| info.account.provider == account.provider);
        let is_default = match all.iter().position(|(info, _)| info.account == account) {
            Some(n) => all.remove(n).0.is_default,
            None => first,
        };
        let info = AccountInfo { account, method: credential.method(), is_default, hint: credential.display_hint() };
        all.push((info.clone(), credential));
        Ok(info)
    }

    async fn remove(&self, account: &AccountRef) -> Result<(), CredentialsError> {
        let mut all = self.accounts.lock().unwrap();
        let found = all
            .iter()
            .position(|(info, _)| info.account == *account)
            .ok_or_else(|| CredentialsError::NotFound(account.clone()))?;
        all.remove(found);
        Ok(())
    }

    async fn set_default(&self, account: &AccountRef) -> Result<(), CredentialsError> {
        let mut all = self.accounts.lock().unwrap();
        if !all.iter().any(|(info, _)| info.account == *account) {
            return Err(CredentialsError::NotFound(account.clone()));
        }
        for (info, _) in all.iter_mut().filter(|(info, _)| info.account.provider == account.provider) {
            info.is_default = &info.account == account;
        }
        Ok(())
    }

    async fn validate(&self, account: &AccountRef) -> Result<(), CredentialsError> {
        self.get(account).await.map(|_| ())
    }
}

#[cfg(all(test, feature = "conformance"))]
mod tests {
    use nixxa_contracts::conformance::credentials::run;

    use super::MemoryCredentials;

    #[tokio::test]
    async fn memory_credentials_conforms() {
        run(MemoryCredentials::default).await;
    }
}
