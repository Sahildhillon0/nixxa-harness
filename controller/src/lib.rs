//! `nixxa-controller` — runs the agent loop: asks the brain, gates every action through policy,
//! routes it to the sandbox or knowledge, records to the db and emits events.
//!
//! Stage 2: the loop from ARCHITECTURE.md §4 with routing; the policy asks the human before
//! every action. Budgets beyond the per-turn step cap, retries and write-back come later.
//!
//! Stage 6 (D-068): the loop also hears the sandbox. The end of a background job the agent
//! started is noted for the agent once, at its next think step; if the agent is idle and was
//! waiting on the job, the controller starts a turn for it (a wake-up), a few at most in a row.
//!
//! Stage 7 (D-072): before every action that can change files (a patch, a write, a command) and
//! at the start of every turn, the controller asks the sandbox for a checkpoint, and records it
//! with the action. [`Command::Undo`] puts the workspace back as it was at one of them, between
//! turns only, and tells the agent which files went back.

#![warn(missing_docs)]

mod jobs;
mod network;
mod policy;

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::StreamExt;
use tokio::sync::{mpsc, watch};

use nixxa_contracts::{
    AccountRef, Action, ActionId, ActionRecord, ActionRequest, ApprovalPort, Brain, BrainEvent, CheckpointId, Content,
    Context, Event, EventSink, Intent, JobId, Knowledge, KnowledgeAction, Limits, Message, NewSession, Observation,
    Outcome, Primer, Role, Sandbox, SandboxAction, SandboxError, SandboxEvent, SessionId, SessionInfo, Store,
    StoreError, Timestamp, TurnId, Verdict, WorkspaceDiff, WorkspaceInfo,
};

/// The default hard cap on brain steps per turn.
pub const DEFAULT_MAX_STEPS: u32 = 8;
/// The default cap on turns the controller starts in a row without a user message.
pub const DEFAULT_MAX_WAKES: u32 = 3;
/// How long the controller waits, once a job it should wake the agent for has ended, for others
/// ending close behind it: they all go into one turn.
pub const DEFAULT_WAKE_DELAY: Duration = Duration::from_secs(1);

/// What opens a turn the controller starts itself (D-068). The job notes follow it.
const WAKE: &str = "This turn started without a message from the user: background jobs you started have ended \
     (below). The user may be away. Carry on with what you were doing, or say what you found.";

/// What the agent is told when a turn is interrupted.
const INTERRUPTED: &str = "interrupted by the user";
/// What an action that never ran is told when the turn stopped before it.
const NOT_RUN: &str = "not run: the turn stopped before this action";
/// What an action left without a result in a stored session (Nixxa quit or crashed while it
/// was waiting for approval or running) is told on resume (D-057).
const QUIT_MID_ACTION: &str =
    "not finished: Nixxa quit before this action finished, so it may not have run, or may have run only in part";
/// What the agent (and the user) are told when a stored session is resumed (D-057). The place
/// the agent works isn't stored: it is chosen again at every launch.
const RESUMED: &str = "This session was resumed. The workspace may have changed since it was recorded (files \
     edited, another place to work in, or a fresh copy), so check it again before relying on what the history says \
     about it.";

/// A command the harness sends to the controller's loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// The human submitted a message. Runs a turn now, or after the in-flight turn ends.
    UserMessage(String),
    /// Cancel the in-flight turn (brain stream, approval prompt, action). No-op when idle.
    Interrupt,
    /// Cancel the in-flight turn and end the session.
    Shutdown,
    /// Record a notice from Nixxa in the session, as a system message the agent sees (e.g. the
    /// user changed the network policy). Kept until the in-flight turn ends, so it never lands
    /// between an action and its result.
    Note(String),
    /// The user's undo: put the workspace back as it was at checkpoint `to`. Runs between turns
    /// only (kept until the in-flight turn ends). `seen` is the newest checkpoint the user had
    /// heard of when they chose: if one was taken since, the agent changed things the user
    /// hasn't looked at, so nothing is restored and an error says so. The agent's running
    /// background jobs are stopped first, a checkpoint is taken (so the undo can be undone), and
    /// the agent is told which files went back.
    Undo {
        /// The checkpoint to go back to.
        to: CheckpointId,
        /// The newest checkpoint the user had seen.
        seen: Option<CheckpointId>,
    },
}

/// Tunables for the agent loop.
#[derive(Debug, Clone)]
pub struct ControllerConfig {
    /// The project being worked on.
    pub workspace: WorkspaceInfo,
    /// The account the session runs with.
    pub account: AccountRef,
    /// The system instructions.
    pub instructions: String,
    /// Hard cap on brain steps per turn.
    pub max_steps: u32,
    /// Source of timestamps for messages and records.
    pub clock: fn() -> Timestamp,
    /// The most turns the controller starts in a row, for ended jobs, without a user message.
    pub max_wakes: u32,
    /// How long it waits for other jobs ending close behind one before it starts that turn.
    pub wake_delay: Duration,
}

impl ControllerConfig {
    /// A config with the default step cap and the system clock.
    pub fn new(workspace: WorkspaceInfo, account: AccountRef, instructions: impl Into<String>) -> Self {
        Self {
            workspace,
            account,
            instructions: instructions.into(),
            max_steps: DEFAULT_MAX_STEPS,
            clock: system_clock,
            max_wakes: DEFAULT_MAX_WAKES,
            wake_delay: DEFAULT_WAKE_DELAY,
        }
    }
}

/// Wall-clock time in milliseconds since the Unix epoch.
fn system_clock() -> Timestamp {
    let millis = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or_default();
    Timestamp(u64::try_from(millis).unwrap_or(u64::MAX))
}

/// Errors the controller can run into while starting a session.
#[derive(Debug, thiserror::Error)]
pub enum ControllerError {
    /// The store refused.
    #[error("store error: {0}")]
    Store(#[from] StoreError),
}

/// The live state of the started session.
struct Session {
    id: SessionId,
    primer: Primer,
    /// The conversation the brain sees, across turns.
    conversation: Vec<Message>,
}

/// What opens a turn.
enum Opening {
    /// The user's message.
    User(String),
    /// Jobs the agent was waiting on ended; these woke it.
    Wake(Vec<JobId>),
}

/// What the idle loop hands back.
enum Next {
    Command(Command),
    Wake,
}

/// Whether the loop keeps going after an intent or a step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flow {
    Continue,
    Finish,
    /// The turn was interrupted at a cancellation point.
    Cancelled,
}

/// Awaits `future` unless the turn is cancelled first (`None`).
///
/// Only the brain stream, the approval prompt and the action itself are wrapped in this; store
/// writes and conversation updates always run to completion, so a cancelled turn never leaves
/// the history or the db half-written.
async fn cancellable<F: Future>(cancel: &watch::Receiver<bool>, future: F) -> Option<F::Output> {
    let mut cancel = cancel.clone();
    let cancelled = async move {
        if cancel.wait_for(|cancelled| *cancelled).await.is_err() {
            // No sender: nothing can cancel this turn.
            std::future::pending::<()>().await;
        }
    };
    tokio::select! {
        biased;
        () = cancelled => None,
        output = future => Some(output),
    }
}

/// Runs the agent loop: policy gate, routing, recording, events.
pub struct Controller {
    brain: Arc<dyn Brain>,
    sandbox: Arc<dyn Sandbox>,
    store: Arc<dyn Store>,
    knowledge: Arc<dyn Knowledge>,
    approvals: Arc<dyn ApprovalPort>,
    events: Arc<dyn EventSink>,
    config: ControllerConfig,
    session: Option<Session>,
    next_turn: u64,
    /// Set to `true` by [`Controller::run`] to cancel the in-flight turn.
    cancel: watch::Receiver<bool>,
    /// The agent's background jobs (D-068).
    jobs: jobs::Shared,
    /// Notices for the agent at its next think step (an action that can't be undone).
    notes: Vec<String>,
    /// The newest checkpoint taken (D-072).
    newest: Option<CheckpointId>,
}

impl Controller {
    /// Wires the controller to the modules it orchestrates.
    pub fn new(
        brain: Arc<dyn Brain>,
        sandbox: Arc<dyn Sandbox>,
        store: Arc<dyn Store>,
        knowledge: Arc<dyn Knowledge>,
        approvals: Arc<dyn ApprovalPort>,
        events: Arc<dyn EventSink>,
        config: ControllerConfig,
    ) -> Self {
        Self {
            brain,
            sandbox,
            store,
            knowledge,
            approvals,
            events,
            config,
            session: None,
            next_turn: 0,
            cancel: watch::channel(false).1,
            jobs: jobs::Shared::default(),
            notes: Vec::new(),
            newest: None,
        }
    }

    /// Creates a session, gathers the primer and emits the session start events.
    ///
    /// A failing primer is reported as an [`Event::Error`] and the session starts unprimed.
    pub async fn start_session(&mut self) -> Result<SessionInfo, ControllerError> {
        let info = self
            .store
            .create_session(NewSession {
                workspace: self.config.workspace.clone(),
                account: self.config.account.clone(),
                started_at: self.now(),
            })
            .await?;
        let primer = self.begin(&info.id).await;
        self.session = Some(Session { id: info.id.clone(), primer, conversation: Vec::new() });
        Ok(info)
    }

    /// Continues a stored session: new messages are appended to it (D-057).
    ///
    /// * The UI is told the history again: [`Event::Message`] for every shown message,
    ///   [`Event::ActionProposed`] and [`Event::ActionFinished`] for every past action, with the
    ///   turn ids the session had. No turn starts.
    /// * The brain gets the stored conversation back, minus `Done` summaries (D-029).
    /// * A proposal stored without a result (Nixxa quit mid-approval or mid-action) is closed
    ///   with [`QUIT_MID_ACTION`] first, because providers reject a tool call with no result.
    /// * A system message ([`RESUMED`]) tells the agent the workspace may have changed and, when
    ///   the session was recorded with another account, that it continues with this one.
    pub async fn resume_session(&mut self, id: &SessionId) -> Result<SessionInfo, ControllerError> {
        let record = self.store.load_session(id).await?;
        let primer = self.begin(&record.info.id).await;
        let mut turns = 0u64;
        let mut turn = TurnId::new("turn-0");
        let mut unanswered: Vec<ActionRequest> = Vec::new();
        let mut conversation = Vec::with_capacity(record.messages.len());
        // Background jobs it started, with their commands, and the job notes it was given, in order.
        let mut history: Vec<jobs::Seen> = Vec::new();
        let mut commands: Vec<(ActionId, String)> = Vec::new();
        for message in record.messages {
            match &message.content {
                Content::Action { request } => {
                    if let Action::Sandbox { action: SandboxAction::Exec { command, background: true, .. } } =
                        &request.action
                    {
                        commands.push((request.id.clone(), command.clone()));
                    }
                }
                Content::Observation { observation } => {
                    if let Outcome::Started { job } = &observation.outcome {
                        let command = commands.iter().find(|(id, _)| *id == observation.action_id);
                        let command = command.map(|(_, c)| c.clone()).unwrap_or_default();
                        history.push(jobs::Seen::Started(job.clone(), command));
                    }
                }
                Content::Text { text } if message.role == Role::System && text.contains(jobs::NOTE) => {
                    history.push(jobs::Seen::Note(text.clone()));
                }
                _ => {}
            }
            match &message.content {
                // Stored for the record, never shown or replayed (D-029).
                Content::Summary { .. } => continue,
                Content::Text { .. } => {
                    if message.role == Role::User {
                        turn = TurnId::new(format!("turn-{turns}"));
                        turns += 1;
                    }
                    self.emit(Event::Message { turn: turn.clone(), message: message.clone() });
                }
                Content::Action { request } => {
                    unanswered.push(request.clone());
                    self.emit(Event::ActionProposed { turn: turn.clone(), request: request.clone() });
                }
                Content::Observation { observation } => {
                    unanswered.retain(|request| request.id != observation.action_id);
                    self.emit(Event::ActionFinished { turn: turn.clone(), observation: observation.clone() });
                }
                Content::Reasoning { .. } => {}
            }
            conversation.push(message);
        }
        self.next_turn = turns;
        self.session = Some(Session { id: record.info.id.clone(), primer, conversation });
        for request in unanswered {
            let observation = Observation::failed(request.id.clone(), QUIT_MID_ACTION);
            self.finish_action(&turn, request, Verdict::Deny(QUIT_MID_ACTION.into()), observation, None).await;
        }
        let mut notice = RESUMED.to_string();
        if record.info.account != self.config.account {
            notice.push_str(&format!(
                " It was recorded with the account {}; it continues with {}.",
                record.info.account, self.config.account
            ));
        }
        if let Some(lost) = jobs::lost_on_resume(&history) {
            notice.push('\n');
            notice.push_str(&lost);
        }
        let message = Message::text(Role::System, notice, self.now());
        self.record_message(&turn, message, true).await;
        Ok(record.info)
    }

    /// Emits the session start events and gathers the primer. A failing primer is reported as
    /// an [`Event::Error`] and the session starts unprimed.
    async fn begin(&mut self, session: &SessionId) -> Primer {
        self.emit(Event::SessionStarted {
            session: session.clone(),
            account: self.config.account.clone(),
            brain: self.brain.describe(),
        });
        self.emit(Event::SandboxStatus { capabilities: self.sandbox.capabilities() });
        for source in self.knowledge.sources().await {
            self.emit(Event::SourceStatus { source });
        }
        match self.knowledge.primer(&self.config.workspace).await {
            Ok(primer) => primer,
            Err(err) => {
                self.emit(Event::Error { turn: None, message: format!("could not load the session primer: {err}") });
                Primer::default()
            }
        }
    }

    /// The command loop. Returns when the session ends ([`Command::Shutdown`] or the sender is
    /// dropped).
    ///
    /// Turns run one at a time. While a turn is in flight the loop keeps listening: an interrupt
    /// or shutdown signals the turn to cancel at its next cancellation point (brain stream,
    /// approval prompt or action), and messages that arrive meanwhile are queued. It also hears
    /// the sandbox throughout: the end of a job the agent started is noted for the agent, and
    /// while idle may start a turn (D-068).
    pub async fn run(&mut self, mut commands: mpsc::Receiver<Command>) {
        let (cancel_tx, cancel_rx) = watch::channel(false);
        self.cancel = cancel_rx;
        let mut sandbox = Some(self.sandbox.subscribe());
        let mut queued: VecDeque<Command> = VecDeque::new();
        let mut shutting_down = false;
        // Turns started for ended jobs since the user last wrote.
        let mut wakes = 0u32;
        while !shutting_down {
            let next = match queued.pop_front() {
                Some(command) => Next::Command(command),
                None => self.idle(&mut commands, &mut sandbox, wakes).await,
            };
            let opening = match next {
                Next::Command(Command::UserMessage(text)) => {
                    jobs::lock(&self.jobs).user_spoke();
                    wakes = 0;
                    Opening::User(text)
                }
                Next::Command(Command::Note(text)) => {
                    self.note(text).await;
                    continue;
                }
                Next::Command(Command::Undo { to, seen }) => {
                    self.undo(to, seen).await;
                    continue;
                }
                Next::Command(Command::Interrupt) => continue,
                Next::Command(Command::Shutdown) => break,
                Next::Wake => {
                    wakes += 1;
                    Opening::Wake(jobs::lock(&self.jobs).waking())
                }
            };
            if self.session.is_none() {
                self.emit(Event::Error { turn: None, message: "no session: start one before sending messages".into() });
                continue;
            }
            let turn = TurnId::new(format!("turn-{}", self.next_turn));
            self.next_turn += 1;
            cancel_tx.send_replace(false);
            let (watch, sink) = (self.jobs.clone(), self.events.clone());
            {
                let turn_future = self.run_turn(opening, &turn);
                tokio::pin!(turn_future);
                let mut listening = true;
                loop {
                    tokio::select! {
                        () = &mut turn_future => break,
                        command = commands.recv(), if listening => match command {
                            Some(command @ (Command::UserMessage(_) | Command::Note(_) | Command::Undo { .. })) => {
                                queued.push_back(command)
                            }
                            Some(Command::Interrupt) => {
                                cancel_tx.send_replace(true);
                            }
                            Some(Command::Shutdown) | None => {
                                cancel_tx.send_replace(true);
                                shutting_down = true;
                                listening = false;
                            }
                        },
                        event = next_event(&mut sandbox) => observe(&watch, &*sink, event),
                    }
                }
            }
            self.emit(Event::TurnDone { turn });
        }
        if let Some(session) = self.session.take() {
            self.emit(Event::SessionEnded { session: session.id });
        }
    }

    /// Waits, between turns, for the next command or for a turn to start for ended jobs. Ends
    /// that won't wake the agent (the user spoke since, the agent stopped the job, or the cap of
    /// turns in a row is reached) are recorded at once, between turns.
    async fn idle(
        &mut self,
        commands: &mut mpsc::Receiver<Command>,
        sandbox: &mut Option<futures::stream::BoxStream<'static, SandboxEvent>>,
        wakes: u32,
    ) -> Next {
        let mut wake_at: Option<tokio::time::Instant> = None;
        loop {
            let due = jobs::lock(&self.jobs).wake_due();
            if due && wakes < self.config.max_wakes {
                // Wait a moment for others ending close behind: one turn for all of them.
                wake_at.get_or_insert_with(|| tokio::time::Instant::now() + self.config.wake_delay);
            } else {
                let capped = due.then_some(self.config.max_wakes);
                self.give_ended(None, capped).await;
            }
            let timer = async {
                match wake_at {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                command = commands.recv() => return Next::Command(command.unwrap_or(Command::Shutdown)),
                event = next_event(sandbox) => observe(&self.jobs, &*self.events, event),
                () = timer => return Next::Wake,
            }
        }
    }

    /// Gives the agent the job ends it hasn't been told about, as one system message: within a
    /// turn at a step, or between turns (`turn` is `None`). `capped` says no turn was started
    /// for them because this many already were in a row.
    async fn give_ended(&mut self, turn: Option<&TurnId>, capped: Option<u32>) {
        let ended = jobs::lock(&self.jobs).take_ended();
        if ended.is_empty() {
            return;
        }
        let mut text = ended.iter().map(|ended| ended.note.as_str()).collect::<Vec<_>>().join("\n");
        if let Some(cap) = capped {
            text.push_str(&format!(
                "\nNo turn was started for this: {cap} already were in a row without a message from the user."
            ));
        }
        let turn = turn.cloned().unwrap_or_else(|| TurnId::new(format!("note-{}", self.next_turn)));
        let message = Message::text(Role::System, text, self.now());
        self.record_message(&turn, message, true).await;
    }

    /// Takes a checkpoint before `before` (an action that can change files) or, without one, at
    /// the start of a turn, when the sandbox keeps checkpoints (D-072). When none can be taken
    /// (the store's cap, or a failure) the action still runs: the user is told it can't be
    /// undone, and the agent at its next step.
    async fn checkpoint(&mut self, turn: &TurnId, before: Option<&ActionRequest>) -> Option<CheckpointId> {
        if !self.sandbox.capabilities().checkpoints {
            return None;
        }
        let action = before.map(|request| request.id.clone());
        match self.sandbox.checkpoint().await {
            Ok(checkpoint) => {
                self.newest = Some(checkpoint.clone());
                self.emit(Event::Checkpoint { turn: turn.clone(), checkpoint: checkpoint.clone(), before: action });
                Some(checkpoint)
            }
            Err(err) => {
                let reason = match err {
                    SandboxError::LimitExceeded(why) => why,
                    other => other.to_string(),
                };
                let what = before.map_or_else(|| "this turn".to_string(), |request| format!("action `{}`", request.id));
                self.notes.push(format!(
                    "Nixxa couldn't take a checkpoint before {what} ({reason}), so the user can't undo it."
                ));
                self.emit(Event::NotUndoable { turn: turn.clone(), before: action, reason });
                None
            }
        }
    }

    /// The user's undo (D-072): between turns, back to `to`.
    async fn undo(&mut self, to: CheckpointId, seen: Option<CheckpointId>) {
        if self.session.is_none() {
            return;
        }
        if self.newest != seen {
            self.emit(Event::Error {
                turn: None,
                message:
                    "nothing was undone: the workspace has changed since the undo was shown. Open it again to see \
                          what it would do now"
                        .into(),
            });
            return;
        }
        // The user leads now: nothing ended so far wakes the agent.
        jobs::lock(&self.jobs).user_spoke();
        // Running jobs would go on changing files: they are stopped first.
        let running = jobs::lock(&self.jobs).running();
        let mut stopped = Vec::new();
        for (job, command) in running {
            jobs::lock(&self.jobs).stopping(&job);
            let id = ActionId::new(format!("undo-stop-{job}"));
            match self.sandbox.execute(id, SandboxAction::JobStop { job: job.clone() }).await {
                Ok(_) => stopped.push(format!("{job} (`{command}`)")),
                Err(err) => self.emit(Event::Error { turn: None, message: format!("could not stop job {job}: {err}") }),
            }
        }
        let turn = TurnId::new(format!("undo-{}", self.next_turn));
        // So the undo can itself be undone.
        self.checkpoint(&turn, None).await;
        let text = match self.sandbox.restore(&to).await {
            Ok(undone) => {
                let text = undo_note(&undone, &stopped);
                self.emit(Event::Restored { checkpoint: to, undone });
                text
            }
            Err(err) => {
                self.emit(Event::Error { turn: None, message: format!("the undo failed: {err}") });
                format!(
                    "The user tried to undo changes in the workspace, but it failed ({err}). Files may be partly \
                     restored: read them again before relying on what you read before."
                )
            }
        };
        self.note(text).await;
    }

    /// Gives the agent the notices it hasn't been told about, at a step.
    async fn give_notes(&mut self, turn: &TurnId) {
        if self.notes.is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.notes).join("\n");
        let message = Message::text(Role::System, text, self.now());
        self.record_message(turn, message, true).await;
    }

    /// Records a notice between turns. Without a session there is nothing to record it in.
    async fn note(&mut self, text: String) {
        if self.session.is_none() {
            return;
        }
        let turn = TurnId::new(format!("note-{}", self.next_turn));
        let message = Message::text(Role::System, text, self.now());
        self.record_message(&turn, message, true).await;
    }

    /// Runs one turn until the brain is done, something fails, the step budget runs out or the
    /// turn is cancelled. The caller emits [`Event::TurnDone`]. A wake-up turn opens with a
    /// system message saying so (recorded, so a resumed session reads naturally); the job notes
    /// follow at its first step.
    async fn run_turn(&mut self, opening: Opening, turn: &TurnId) {
        let message = match opening {
            Opening::User(text) => {
                self.emit(Event::TurnStarted { turn: turn.clone() });
                Message::text(Role::User, text, self.now())
            }
            Opening::Wake(jobs) => {
                self.emit(Event::WakeUp { turn: turn.clone(), jobs });
                self.emit(Event::TurnStarted { turn: turn.clone() });
                Message::text(Role::System, WAKE, self.now())
            }
        };
        self.record_message(turn, message, true).await;
        // "Undo this turn" goes back to here.
        let cancel = self.cancel.clone();
        if cancellable(&cancel, self.checkpoint(turn, None)).await.is_none() {
            let message = Message::text(Role::System, INTERRUPTED, self.now());
            self.record_message(turn, message, true).await;
            return;
        }
        for _ in 0..self.config.max_steps {
            match self.step(turn).await {
                Flow::Continue => {}
                Flow::Finish => return,
                Flow::Cancelled => {
                    let message = Message::text(Role::System, INTERRUPTED, self.now());
                    self.record_message(turn, message, true).await;
                    return;
                }
            }
        }
        let notice = format!("stopped: the step budget of {} for this turn ran out", self.config.max_steps);
        let message = Message::text(Role::System, notice, self.now());
        self.record_message(turn, message, true).await;
    }

    /// One think step: build the context, stream the brain, then run its actions.
    ///
    /// The brain's output for the step (reasoning, text, every proposed action) is recorded in
    /// stream order first, and only then are the actions gated and run, so their observations
    /// follow together. That keeps the model's message whole in the history, which providers
    /// that sign their reasoning require (D-028).
    async fn step(&mut self, turn: &TurnId) -> Flow {
        // Between steps, never between an action and its result: notices, and jobs that ended
        // meanwhile.
        self.give_notes(turn).await;
        self.give_ended(Some(turn), None).await;
        let context = self.build_context().await;
        let mut stream = match cancellable(&self.cancel, self.brain.think(context)).await {
            None => return Flow::Cancelled,
            Some(Ok(stream)) => stream,
            Some(Err(err)) => {
                self.emit(Event::Error { turn: Some(turn.clone()), message: err.to_string() });
                return Flow::Finish;
            }
        };
        let mut pending: VecDeque<ActionRequest> = VecDeque::new();
        let mut done = false;
        let mut saw_intent = false;
        loop {
            let Some(next) = cancellable(&self.cancel, stream.next()).await else {
                self.close_unrun(turn, pending, INTERRUPTED).await;
                return Flow::Cancelled;
            };
            let Some(item) = next else { break };
            match item {
                // Usage is always reported, even after the turn has finished, so it is never lost.
                Ok(BrainEvent::Usage { usage }) => {
                    self.emit(Event::Usage { account: self.config.account.clone(), usage });
                }
                Ok(_) if done => {}
                Ok(BrainEvent::TextDelta { text }) => self.emit(Event::TextDelta { turn: turn.clone(), text }),
                Ok(BrainEvent::Reasoning { provider, data }) => {
                    // Kept for the brain, in stream order; never shown.
                    let content = Content::Reasoning { provider, data };
                    let message = Message { role: Role::Agent, content, at: self.now() };
                    self.record_message(turn, message, false).await;
                }
                Ok(BrainEvent::Intent { intent }) => {
                    saw_intent = true;
                    match intent {
                        Intent::Say { text } => {
                            let message = Message::text(Role::Agent, text, self.now());
                            self.record_message(turn, message, true).await;
                        }
                        Intent::Act { request } => {
                            self.propose(turn, &request).await;
                            pending.push_back(request);
                        }
                        Intent::Done { summary } => {
                            // Stored, marked as a summary, and kept out of what the brain sees: it
                            // isn't part of the model's own message (D-029).
                            let content = Content::Summary { text: summary };
                            let message = Message { role: Role::Agent, content, at: self.now() };
                            self.store_message(turn, &message).await;
                            done = true;
                        }
                    }
                }
                Err(err) => {
                    self.emit(Event::Error { turn: Some(turn.clone()), message: err.to_string() });
                    self.close_unrun(turn, pending, NOT_RUN).await;
                    return Flow::Finish;
                }
            }
        }
        if !saw_intent {
            self.emit(Event::Error {
                turn: Some(turn.clone()),
                message: "the brain ended its step without an intent".into(),
            });
            return Flow::Finish;
        }
        while let Some(request) = pending.pop_front() {
            match self.act(turn, request).await {
                Flow::Continue => {}
                Flow::Finish => {
                    self.close_unrun(turn, pending, NOT_RUN).await;
                    return Flow::Finish;
                }
                Flow::Cancelled => {
                    self.close_unrun(turn, pending, INTERRUPTED).await;
                    return Flow::Cancelled;
                }
            }
        }
        if done { Flow::Finish } else { Flow::Continue }
    }

    /// Records a proposed action in the conversation and announces it.
    async fn propose(&mut self, turn: &TurnId, request: &ActionRequest) {
        let proposal =
            Message { role: Role::Agent, content: Content::Action { request: request.clone() }, at: self.now() };
        self.record_message(turn, proposal, false).await;
        self.emit(Event::ActionProposed { turn: turn.clone(), request: request.clone() });
    }

    /// Closes proposals that will never run with a denial, so every proposal has a result.
    async fn close_unrun(&mut self, turn: &TurnId, unrun: VecDeque<ActionRequest>, reason: &str) {
        for request in unrun {
            let observation = Observation::denied(request.id.clone(), reason);
            self.finish_action(turn, request, Verdict::Deny(reason.into()), observation, None).await;
        }
    }

    /// Gates, routes and records one already-proposed action. An infrastructure failure ends the
    /// turn; denials and action-level failures go back to the brain as observations. An
    /// interrupt while asking or running still closes the action with an observation, so the
    /// history never holds a proposal without its result.
    async fn act(&mut self, turn: &TurnId, request: ActionRequest) -> Flow {
        let verdict = match policy::gate(&request) {
            Verdict::Ask(reason) => {
                self.emit(Event::ApprovalNeeded {
                    turn: turn.clone(),
                    request: request.clone(),
                    reason: reason.clone(),
                });
                match cancellable(&self.cancel, self.approvals.ask(&request, &reason)).await {
                    Some(Verdict::Allow) => Verdict::Allow,
                    // The port must answer Allow or Deny; anything else counts as a denial.
                    Some(Verdict::Deny(reason) | Verdict::Ask(reason)) => Verdict::Deny(reason),
                    None => {
                        let observation = Observation::denied(request.id.clone(), INTERRUPTED);
                        self.finish_action(turn, request, Verdict::Deny(INTERRUPTED.into()), observation, None).await;
                        return Flow::Cancelled;
                    }
                }
            }
            verdict => verdict,
        };

        // A checkpoint before anything that can change files, once it may run (D-072).
        let mut checkpoint = None;
        if verdict == Verdict::Allow {
            if let Action::Sandbox { action } = &request.action {
                if action.changes_files() {
                    let cancel = self.cancel.clone();
                    match cancellable(&cancel, self.checkpoint(turn, Some(&request))).await {
                        Some(taken) => checkpoint = taken,
                        None => {
                            let observation = Observation::denied(request.id.clone(), INTERRUPTED);
                            let verdict = Verdict::Deny(INTERRUPTED.into());
                            self.finish_action(turn, request, verdict, observation, None).await;
                            return Flow::Cancelled;
                        }
                    }
                }
            }
        }

        let (observation, flow) = match &verdict {
            Verdict::Allow => {
                self.emit(Event::ActionStarted { turn: turn.clone(), action_id: request.id.clone() });
                if let Action::Sandbox { action: SandboxAction::JobStop { job } } = &request.action {
                    jobs::lock(&self.jobs).stopping(job);
                }
                match cancellable(&self.cancel, self.route(&request)).await {
                    None => (Observation::failed(request.id.clone(), INTERRUPTED), Flow::Cancelled),
                    Some(Ok(observation)) => {
                        self.watch_started(&request, &observation);
                        (observation, Flow::Continue)
                    }
                    Some(Err(message)) => {
                        self.emit(Event::Error { turn: Some(turn.clone()), message: message.clone() });
                        (Observation::failed(request.id.clone(), message), Flow::Finish)
                    }
                }
            }
            Verdict::Deny(reason) | Verdict::Ask(reason) => {
                (Observation::denied(request.id.clone(), reason.clone()), Flow::Continue)
            }
        };
        self.finish_action(turn, request, verdict, observation, checkpoint).await;
        flow
    }

    /// Watches a background job the agent just started (D-068); if it has already ended, its
    /// end is taken now.
    fn watch_started(&self, request: &ActionRequest, observation: &Observation) {
        if let (
            Action::Sandbox { action: SandboxAction::Exec { command, background: true, .. } },
            Outcome::Started { job },
        ) = (&request.action, &observation.outcome)
        {
            let early = jobs::lock(&self.jobs).started(job.clone(), command.clone());
            if let Some(event) = early {
                self.emit(event);
            }
        }
    }

    /// Sends an action to its executor. `Err` is an infrastructure failure, already in words.
    async fn route(&self, request: &ActionRequest) -> Result<Observation, String> {
        let id = request.id.clone();
        match &request.action {
            Action::Sandbox { action } => self.sandbox.execute(id, action.clone()).await.map_err(|e| e.to_string()),
            Action::Knowledge { action: KnowledgeAction::Call { source, tool, args } } => {
                self.knowledge.call(id, source, tool, args.clone()).await.map_err(|e| e.to_string())
            }
            Action::Knowledge { action: KnowledgeAction::Record { source, entry } } => {
                self.knowledge.record(id, source, entry.clone()).await.map_err(|e| e.to_string())
            }
        }
    }

    /// Records the action and its observation, and feeds the observation back to the brain.
    async fn finish_action(
        &mut self,
        turn: &TurnId,
        request: ActionRequest,
        verdict: Verdict,
        observation: Observation,
        checkpoint: Option<CheckpointId>,
    ) {
        self.emit(Event::ActionFinished { turn: turn.clone(), observation: observation.clone() });
        let record =
            ActionRecord { request, verdict, observation: Some(observation.clone()), checkpoint, at: self.now() };
        let message =
            Message { role: Role::Observation, content: Content::Observation { observation }, at: self.now() };
        self.record_message(turn, message, false).await;
        if let Some(id) = self.session_id() {
            if let Err(err) = self.store.append_action(&id, record).await {
                let message = format!("could not save the action: {err}");
                self.emit(Event::Error { turn: Some(turn.clone()), message });
            }
        }
    }

    /// Saves a message to the store only: the brain never sees it.
    async fn store_message(&mut self, turn: &TurnId, message: &Message) {
        let Some(id) = self.session_id() else { return };
        if let Err(err) = self.store.append_message(&id, message.clone()).await {
            self.emit(Event::Error { turn: Some(turn.clone()), message: format!("could not save the message: {err}") });
        }
    }

    /// Adds a message to the conversation and the store; optionally announces it.
    async fn record_message(&mut self, turn: &TurnId, message: Message, announce: bool) {
        let Some(session) = self.session.as_mut() else { return };
        session.conversation.push(message.clone());
        let id = session.id.clone();
        if let Err(err) = self.store.append_message(&id, message.clone()).await {
            self.emit(Event::Error { turn: Some(turn.clone()), message: format!("could not save the message: {err}") });
        }
        if announce {
            self.emit(Event::Message { turn: turn.clone(), message });
        }
    }

    /// Everything the brain needs for one think step.
    async fn build_context(&self) -> Context {
        let knowledge_tools = self.knowledge.tools().await;
        let (session, primer, messages) = match &self.session {
            Some(s) => (s.id.clone(), s.primer.clone(), s.conversation.clone()),
            None => (SessionId::new(""), Primer::default(), Vec::new()),
        };
        Context {
            session,
            account: self.config.account.clone(),
            model: None,
            instructions: format!(
                "{}\n\n{}",
                self.config.instructions,
                network::instructions(&self.sandbox.capabilities().network)
            ),
            primer,
            messages,
            sandbox_actions: self.sandbox.capabilities().actions,
            knowledge_tools,
            limits: Limits::default(),
        }
    }

    fn session_id(&self) -> Option<SessionId> {
        self.session.as_ref().map(|s| s.id.clone())
    }

    fn now(&self) -> Timestamp {
        (self.config.clock)()
    }

    fn emit(&self, event: Event) {
        self.events.emit(event);
    }
}

/// The next sandbox event, or never once the stream has ended.
async fn next_event(sandbox: &mut Option<futures::stream::BoxStream<'static, SandboxEvent>>) -> Option<SandboxEvent> {
    match sandbox.as_mut() {
        Some(stream) => match stream.next().await {
            Some(event) => Some(event),
            None => {
                *sandbox = None;
                None
            }
        },
        None => std::future::pending().await,
    }
}

/// Notes the end of a job the agent started, and tells the UI.
fn observe(watch: &jobs::Shared, sink: &dyn EventSink, event: Option<SandboxEvent>) {
    let Some(event) = event else { return };
    if let Some(event) = jobs::lock(watch).observe(event) {
        sink.emit(event);
    }
}

/// The paths an undo note lists; the rest are counted.
const NOTE_PATHS: usize = 40;

/// What the agent is told after the user's undo: which paths went back, and that what it read
/// before may be stale.
fn undo_note(undone: &WorkspaceDiff, stopped: &[String]) -> String {
    let mut text = String::from("The user undid changes: the workspace is back as it was at an earlier checkpoint.");
    if undone.changes.is_empty() {
        text.push_str(" Nothing had changed since, so no file changed.");
    } else {
        text.push_str(" These paths went back:");
        for change in undone.changes.iter().take(NOTE_PATHS) {
            let how = match (&change.before, &change.after) {
                (None, _) => "removed: it was created after the checkpoint",
                (_, None) => "restored: it had been deleted",
                _ => "changed back",
            };
            text.push_str(&format!("\n- {} ({how})", change.path));
        }
        if undone.changes.len() > NOTE_PATHS {
            text.push_str(&format!("\n- … and {} more", undone.changes.len() - NOTE_PATHS));
        }
        text.push_str(
            "\nWhat you read before may be stale: read files again before you edit them (an edit with an old hash fails).",
        );
    }
    if !stopped.is_empty() {
        text.push_str(&format!("\nYour background jobs were stopped first: {}.", stopped.join(", ")));
    }
    if !undone.not_covered.is_empty() {
        text.push_str(&format!(
            "\nCheckpoints leave these out, so they were left as they are: {}.",
            undone.not_covered.join(", ")
        ));
    }
    text
}
