//! Everything the harness can render.

use serde::{Deserialize, Serialize};

use crate::{
    AccountRef, ActionId, ActionRequest, ApprovalMode, BrainInfo, JobId, Message, Observation, SandboxCapabilities,
    SessionId, SourceInfo, TurnId, Usage,
};

/// An event from the controller to the harness (via [`EventSink`](crate::EventSink)).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// A session began or was resumed.
    SessionStarted {
        /// The session.
        session: SessionId,
        /// Its account.
        account: AccountRef,
        /// The brain in use.
        brain: BrainInfo,
    },
    /// The sandbox's capabilities (sent at start and whenever they change).
    SandboxStatus {
        /// Capabilities.
        capabilities: SandboxCapabilities,
    },
    /// A knowledge source's status (sent at start and whenever it changes).
    SourceStatus {
        /// The source.
        source: SourceInfo,
    },
    /// The approval mode changed.
    ModeChanged {
        /// The new mode.
        mode: ApprovalMode,
    },
    /// The session's account changed.
    AccountChanged {
        /// The new account.
        account: AccountRef,
    },
    /// A turn began.
    TurnStarted {
        /// The turn.
        turn: TurnId,
    },
    /// Streamed text from the brain.
    TextDelta {
        /// The turn.
        turn: TurnId,
        /// The piece.
        text: String,
    },
    /// A complete message was recorded.
    Message {
        /// The turn.
        turn: TurnId,
        /// The message.
        message: Message,
    },
    /// The brain proposed an action.
    ActionProposed {
        /// The turn.
        turn: TurnId,
        /// The proposal.
        request: ActionRequest,
    },
    /// The policy gate wants the human to decide (the harness's [`ApprovalPort`](crate::ApprovalPort) is being asked).
    ApprovalNeeded {
        /// The turn.
        turn: TurnId,
        /// The proposal.
        request: ActionRequest,
        /// The policy's reason.
        reason: String,
    },
    /// An action started running.
    ActionStarted {
        /// The turn.
        turn: TurnId,
        /// The action.
        action_id: ActionId,
    },
    /// An action finished (including denials).
    ActionFinished {
        /// The turn.
        turn: TurnId,
        /// The result.
        observation: Observation,
    },
    /// Usage was incurred on an account.
    Usage {
        /// The account.
        account: AccountRef,
        /// The usage.
        usage: Usage,
    },
    /// Something went wrong. Never contains secrets.
    Error {
        /// The turn, if any.
        turn: Option<TurnId>,
        /// Human-readable message.
        message: String,
    },
    /// A background job the agent started has ended. Only the session's own jobs are reported.
    JobFinished {
        /// The job.
        job: JobId,
        /// The command it ran.
        command: String,
        /// The shell's exit code; `None` when it didn't exit by itself (stopped, timed out,
        /// killed or lost with its sandbox).
        exit_code: Option<i32>,
        /// How long it ran.
        duration_ms: u64,
        /// How it ended, in the sandbox's words ("job j1 exited with code 0 after 3.1 s").
        status: String,
    },
    /// A turn is starting without a user message: background jobs the agent was waiting on have
    /// ended. A [`Event::TurnStarted`] for the same turn follows.
    WakeUp {
        /// The turn about to start.
        turn: TurnId,
        /// The jobs whose ends woke the agent.
        jobs: Vec<JobId>,
    },
    /// A turn finished.
    TurnDone {
        /// The turn.
        turn: TurnId,
    },
    /// The session ended.
    SessionEnded {
        /// The session.
        session: SessionId,
    },
}
