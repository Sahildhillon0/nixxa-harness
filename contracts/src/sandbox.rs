//! The [`Sandbox`] contract: executes actions in isolation.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{ActionId, Observation, SandboxAction, SandboxActionKind, SandboxError};

/// What a sandbox backend can do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxCapabilities {
    /// Backend name, e.g. `bubblewrap`, `fake`.
    pub backend: String,
    /// Whether execution is actually isolated from the host. The TUI shows this prominently.
    pub isolated: bool,
    /// Whether network access is available inside.
    pub network: bool,
    /// Supported action kinds.
    pub actions: Vec<SandboxActionKind>,
}

/// Executes [`SandboxAction`]s in isolation and returns [`Observation`]s.
///
/// Contract:
/// * an unsupported action kind returns [`SandboxError::Unsupported`];
/// * action-level failures (non-zero exit, missing file, path outside the workspace) are
///   [`Observation`]s, not errors;
/// * a path that escapes the workspace never succeeds;
/// * the returned observation's `action_id` equals the given `id`;
/// * an isolated backend that supports `Exec` really runs the command;
/// * [`reset`](Sandbox::reset) keeps the workspace;
/// * dropping an in-flight `execute` future cancels the action, and the next call still works;
/// * the sandbox enforces its own limits regardless of what the controller allowed.
#[async_trait]
pub trait Sandbox: Send + Sync {
    /// What this backend supports.
    fn capabilities(&self) -> SandboxCapabilities;

    /// Executes one action.
    async fn execute(&self, id: ActionId, action: SandboxAction) -> Result<Observation, SandboxError>;

    /// Checks the backend is usable.
    async fn health(&self) -> Result<(), SandboxError>;

    /// Kills anything running and returns to a clean state (the workspace is kept).
    async fn reset(&self) -> Result<(), SandboxError>;
}
