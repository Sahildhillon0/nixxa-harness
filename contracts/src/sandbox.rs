//! The [`Sandbox`] contract: executes actions in isolation.

use async_trait::async_trait;
use futures_core::stream::BoxStream;
use serde::{Deserialize, Serialize};

use crate::{ActionId, JobId, Observation, SandboxAction, SandboxActionKind, SandboxError, Timestamp};

/// What a sandbox backend can do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxCapabilities {
    /// Backend name, e.g. `bubblewrap`, `fake`.
    pub backend: String,
    /// Whether execution is actually isolated from the host. The TUI shows this prominently.
    pub isolated: bool,
    /// Whether network access is available inside.
    pub network: bool,
    /// Whether background jobs are supported: `Exec { background: true }`, the `Job*` actions
    /// and [`SandboxEvent::JobFinished`].
    #[serde(default)]
    pub jobs: bool,
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
///
/// Background jobs, when [`SandboxCapabilities::jobs`] is set:
/// * `Exec { background: true }` returns [`Outcome::Started`](crate::Outcome::Started) at once,
///   with a job id no other job of this sandbox has had;
/// * every started job ends with exactly one [`SandboxEvent::JobFinished`] for its id, carrying
///   the shell's exit code when it exited by itself;
/// * the `Job*` actions on an id the sandbox never started don't succeed;
/// * [`reset`](Sandbox::reset) stops every job the agent started.
///
/// Without jobs, `Exec { background: true }` returns [`SandboxError::Unsupported`], the `Job*`
/// kinds aren't listed, and [`subscribe`](Sandbox::subscribe) returns an empty stream.
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

    /// The sandbox's events from now on. Dropping the stream unsubscribes. The default, for
    /// backends without jobs, is an empty stream.
    fn subscribe(&self) -> BoxStream<'static, SandboxEvent> {
        Box::pin(Empty)
    }
}

/// Something that happened in a sandbox outside any one `execute` call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum SandboxEvent {
    /// A background job ended: it exited, hit its time limit, or was stopped.
    JobFinished {
        /// The job.
        job: JobId,
        /// The shell's exit code; `None` when it was killed (time limit, stop, reset, signal).
        exit_code: Option<i32>,
        /// How long it ran.
        duration_ms: u64,
        /// Whether output was dropped (only the last part of each stream is kept).
        truncated: bool,
    },
    /// A command made a network request through the sandbox's proxy.
    NetRequest(NetRequest),
}

/// One network request, as the sandbox's proxy saw it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetRequest {
    /// When it was made.
    pub time: Timestamp,
    /// The job that made it, when the sandbox can tell.
    pub job: Option<JobId>,
    /// The host asked for.
    pub host: String,
    /// The port asked for.
    pub port: u16,
    /// The HTTP method (plain HTTP only; HTTPS is a tunnel the proxy can't see into).
    pub method: Option<String>,
    /// The full URL (plain HTTP only).
    pub url: Option<String>,
    /// Bytes sent by the command.
    pub bytes_up: u64,
    /// Bytes received by the command.
    pub bytes_down: u64,
    /// How long the connection lasted.
    pub duration_ms: u64,
    /// Whether it was let through.
    pub result: NetResult,
}

/// Whether the proxy let a request through.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum NetResult {
    /// Let through.
    Allowed,
    /// Refused.
    Blocked {
        /// Why, in words the agent and the user can act on.
        reason: String,
    },
}

/// The stream of a backend without events.
struct Empty;

impl futures_core::Stream for Empty {
    type Item = SandboxEvent;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        std::task::Poll::Ready(None)
    }
}
