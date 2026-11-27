//! One error enum per contract trait. Implementations map their internal errors into these;
//! internal error types never cross a module boundary.

use crate::{AccountRef, CheckpointId, SessionId, SourceId};

/// Errors from [`Brain`](crate::Brain).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BrainError {
    /// The provider is rate limiting.
    #[error("rate limited{}", .retry_after_ms.map(|ms| format!(", retry after {ms} ms")).unwrap_or_default())]
    RateLimited {
        /// Suggested wait.
        retry_after_ms: Option<u64>,
    },
    /// Missing, invalid or expired credentials.
    #[error("authentication failed: {0}")]
    Auth(String),
    /// The context does not fit the model's window even after trimming.
    #[error("context too large for the model")]
    ContextTooLarge,
    /// The provider returned an error.
    #[error("provider error: {0}")]
    Provider(String),
    /// Network trouble reaching the provider.
    #[error("network error: {0}")]
    Network(String),
    /// Anything else.
    #[error("{0}")]
    Other(String),
}

/// Errors from [`Sandbox`](crate::Sandbox). Action-level failures are observations, not errors.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SandboxError {
    /// This backend does not support the action kind.
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// The backend is not available (not installed, cannot start).
    #[error("sandbox unavailable: {0}")]
    Unavailable(String),
    /// The action hit the sandbox's hard time limit.
    #[error("timed out")]
    Timeout,
    /// The action hit another hard limit (memory, output, processes, the checkpoint store's
    /// size cap…).
    #[error("limit exceeded: {0}")]
    LimitExceeded(String),
    /// No such checkpoint: never taken by this sandbox, or its store is gone.
    #[error("no such checkpoint: {0}")]
    NoCheckpoint(CheckpointId),
    /// Anything else.
    #[error("{0}")]
    Other(String),
}

/// Errors from [`Store`](crate::Store).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StoreError {
    /// No such session.
    #[error("session not found: {0}")]
    NotFound(SessionId),
    /// The storage backend failed.
    #[error("storage error: {0}")]
    Backend(String),
}

/// Errors from [`Knowledge`](crate::Knowledge).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KnowledgeError {
    /// The source is unknown or not ready.
    #[error("knowledge source unavailable: {0}")]
    SourceUnavailable(SourceId),
    /// The source has no such tool.
    #[error("unknown tool `{tool}` on source {source_id}")]
    UnknownTool {
        /// The source.
        source_id: SourceId,
        /// The tool name.
        tool: String,
    },
    /// The source does not accept this operation (e.g. a read-only source asked to record).
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// Anything else.
    #[error("{0}")]
    Other(String),
}

/// Errors from [`Credentials`](crate::Credentials). Never contain secret material.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CredentialsError {
    /// No such account.
    #[error("account not found: {0}")]
    NotFound(AccountRef),
    /// The credential has expired and could not be refreshed.
    #[error("credential expired: {0}")]
    Expired(AccountRef),
    /// The credential is malformed or rejected.
    #[error("invalid credential: {0}")]
    Invalid(String),
    /// The operation is not supported (e.g. sign-in for this provider).
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// The secret store (keychain, encrypted file) failed.
    #[error("credential storage error: {0}")]
    Storage(String),
}
