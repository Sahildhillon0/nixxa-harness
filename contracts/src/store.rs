//! The [`Store`] contract: Nixxa's own sessions.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{
    AccountRef, ActionRequest, CheckpointId, Message, Observation, SessionId, StoreError, Timestamp, Verdict,
    WorkspaceInfo,
};

/// Input for [`Store::create_session`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewSession {
    /// The workspace.
    pub workspace: WorkspaceInfo,
    /// The account the session starts with.
    pub account: AccountRef,
    /// Start time.
    pub started_at: Timestamp,
}

/// Summary of a stored session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    /// Assigned by the store.
    pub id: SessionId,
    /// The workspace.
    pub workspace: WorkspaceInfo,
    /// The account the session started with.
    pub account: AccountRef,
    /// Start time.
    pub started_at: Timestamp,
    /// The text of the session's first [`Role::User`](crate::Role::User) text message, for
    /// session lists. `None` until one is appended; later messages never change it.
    pub first_message: Option<String>,
}

/// One action as it went through the loop.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActionRecord {
    /// What was proposed.
    pub request: ActionRequest,
    /// What the policy gate (or the human) decided.
    pub verdict: Verdict,
    /// The result, if it ran.
    pub observation: Option<Observation>,
    /// The checkpoint taken just before it ran, for an action that can change files: undoing
    /// it restores this one. Absent in records written before Stage 7.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<CheckpointId>,
    /// When the record was written.
    pub at: Timestamp,
}

/// A full stored session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionRecord {
    /// Summary.
    pub info: SessionInfo,
    /// Messages in append order.
    pub messages: Vec<Message>,
    /// Actions in append order.
    pub actions: Vec<ActionRecord>,
}

/// A search result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchHit {
    /// The session containing the match.
    pub session: SessionId,
    /// A short excerpt around the match.
    pub snippet: String,
}

/// Persists Nixxa's own sessions.
///
/// Contract:
/// * appends are returned by [`Store::load_session`] in append order, exactly as given (every
///   [`Content`](crate::Content) variant round-trips, so a `Summary` stays a `Summary`);
/// * operations on an unknown session return [`StoreError::NotFound`];
/// * [`Store::list_sessions`] is newest first (by `started_at`);
/// * [`Store::search`] matches the text of [`Content::Text`](crate::Content::Text) messages
///   case-insensitively; snippets are short and drawn from the match.
#[async_trait]
pub trait Store: Send + Sync {
    /// Creates a session and assigns its id.
    async fn create_session(&self, new: NewSession) -> Result<SessionInfo, StoreError>;

    /// Loads a full session.
    async fn load_session(&self, id: &SessionId) -> Result<SessionRecord, StoreError>;

    /// Appends a message.
    async fn append_message(&self, id: &SessionId, message: Message) -> Result<(), StoreError>;

    /// Appends an action record.
    async fn append_action(&self, id: &SessionId, record: ActionRecord) -> Result<(), StoreError>;

    /// Lists sessions, newest first.
    async fn list_sessions(&self) -> Result<Vec<SessionInfo>, StoreError>;

    /// Full-text search over message text.
    async fn search(&self, query: &str) -> Result<Vec<SearchHit>, StoreError>;
}
