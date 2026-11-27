//! Everything the harness can render.

use serde::{Deserialize, Serialize};

use crate::{
    AccountRef, ActionId, ActionRequest, ApprovalMode, BrainInfo, CheckpointId, JobId, Message, Observation,
    RedactionSource, SandboxCapabilities, SessionId, SourceInfo, TurnId, Usage, WorkspaceDiff,
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
    /// A checkpoint of the workspace was taken: before an action that can change files, or at
    /// the start of a turn. Restoring it undoes that action, or that turn, and what came after.
    Checkpoint {
        /// The turn.
        turn: TurnId,
        /// The checkpoint.
        checkpoint: CheckpointId,
        /// The action it comes before; `None` for the start of the turn.
        before: Option<ActionId>,
    },
    /// No checkpoint could be taken (the store's size cap, or a failure): the action or turn
    /// still runs, but can't be undone.
    NotUndoable {
        /// The turn.
        turn: TurnId,
        /// The action; `None` for the start of the turn.
        before: Option<ActionId>,
        /// Why.
        reason: String,
    },
    /// The workspace was put back as it was at a checkpoint (the user's undo).
    Restored {
        /// The checkpoint.
        checkpoint: CheckpointId,
        /// What that undid.
        undone: WorkspaceDiff,
    },
    /// A secret was found in something about to enter the agent's context, and replaced by a
    /// placeholder there and in the session's record (#3). Never carries the secret, nor
    /// anything that would rebuild it: the placeholder's tag is a keyed hash.
    SecretRedacted {
        /// The turn, if any.
        turn: Option<TurnId>,
        /// What kind of secret (`aws-access-key`, `anthropic-key`, `private-key`, …).
        kind: String,
        /// Where it came from.
        source: RedactionSource,
        /// What stands in for it: `‹secret:<kind>#<tag>›`, the same for the same value within a
        /// session.
        placeholder: String,
        /// The user chose to send this one, in their own message, as it is: the agent got the
        /// value; the session's record still holds the placeholder.
        sent: bool,
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
