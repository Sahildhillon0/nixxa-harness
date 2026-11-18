//! Fakes for the db contract.

use std::sync::Mutex;

use async_trait::async_trait;

use nixxa_contracts::{Message, NewSession, SearchHit, SessionInfo, SessionRecord, Store, StoreError};

/// An in-memory [`Store`] fake.
#[derive(Debug, Default)]
pub struct MemoryStore {
    sessions: Mutex<Vec<SessionRecord>>,
}

#[async_trait]
impl Store for MemoryStore {
    async fn create_session(&self, new: NewSession) -> Result<SessionInfo, StoreError> {
        let mut all = self.sessions.lock().unwrap();
        let info = SessionInfo {
            id: nixxa_contracts::SessionId::new(format!("s{}", all.len() + 1)),
            workspace: new.workspace,
            account: new.account,
            started_at: new.started_at,
        };
        all.push(SessionRecord { info: info.clone(), messages: vec![], actions: vec![] });
        Ok(info)
    }

    async fn load_session(&self, id: &nixxa_contracts::SessionId) -> Result<SessionRecord, StoreError> {
        let all = self.sessions.lock().unwrap();
        all.iter().find(|s| &s.info.id == id).cloned().ok_or_else(|| StoreError::NotFound(id.clone()))
    }

    async fn append_message(&self, id: &nixxa_contracts::SessionId, message: Message) -> Result<(), StoreError> {
        let mut all = self.sessions.lock().unwrap();
        let session = all.iter_mut().find(|s| &s.info.id == id).ok_or_else(|| StoreError::NotFound(id.clone()))?;
        session.messages.push(message);
        Ok(())
    }

    async fn append_action(
        &self,
        id: &nixxa_contracts::SessionId,
        record: nixxa_contracts::ActionRecord,
    ) -> Result<(), StoreError> {
        let mut all = self.sessions.lock().unwrap();
        let session = all.iter_mut().find(|s| &s.info.id == id).ok_or_else(|| StoreError::NotFound(id.clone()))?;
        session.actions.push(record);
        Ok(())
    }

    async fn list_sessions(&self) -> Result<Vec<SessionInfo>, StoreError> {
        let mut list: Vec<_> = self.sessions.lock().unwrap().iter().map(|s| s.info.clone()).collect();
        list.sort_by_key(|s| std::cmp::Reverse(s.started_at));
        Ok(list)
    }

    async fn search(&self, query: &str) -> Result<Vec<SearchHit>, StoreError> {
        let q = query.to_lowercase();
        let mut hits = vec![];
        for session in self.sessions.lock().unwrap().iter() {
            for message in &session.messages {
                if let nixxa_contracts::Content::Text { text } = &message.content {
                    if text.to_lowercase().contains(&q) {
                        hits.push(SearchHit { session: session.info.id.clone(), snippet: text.clone() });
                    }
                }
            }
        }
        Ok(hits)
    }
}

#[cfg(test)]
mod tests {
    use nixxa_contracts::conformance::store::run;

    use super::MemoryStore;

    #[tokio::test]
    async fn memory_store_conforms() {
        run(MemoryStore::default).await;
    }
}
