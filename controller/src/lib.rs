//! `nixxa-controller` — runs the agent loop: asks the brain, gates every action through policy,
//! routes it to the sandbox or knowledge, records to the db and emits events.
//!
//! Stage 2: the loop from ARCHITECTURE.md §4 with routing; the policy asks the human before
//! every action. Budgets beyond the per-turn step cap, retries and write-back come later.

#![warn(missing_docs)]

mod policy;

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use futures::StreamExt;
use tokio::sync::{mpsc, watch};

use nixxa_contracts::{
    AccountRef, Action, ActionRecord, ActionRequest, ApprovalPort, Brain, BrainEvent, Content, Context, Event,
    EventSink, Intent, Knowledge, KnowledgeAction, Limits, Message, NewSession, Observation, Primer, Role, Sandbox,
    SessionId, SessionInfo, Store, StoreError, Timestamp, TurnId, Verdict, WorkspaceInfo,
};

/// The default hard cap on brain steps per turn.
pub const DEFAULT_MAX_STEPS: u32 = 8;

/// What the agent is told when a turn is interrupted.
const INTERRUPTED: &str = "interrupted by the user";
/// What an action that never ran is told when the turn stopped before it.
const NOT_RUN: &str = "not run: the turn stopped before this action";

/// A command the harness sends to the controller's loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// The human submitted a message. Runs a turn now, or after the in-flight turn ends.
    UserMessage(String),
    /// Cancel the in-flight turn (brain stream, approval prompt, action). No-op when idle.
    Interrupt,
    /// Cancel the in-flight turn and end the session.
    Shutdown,
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
        self.emit(Event::SessionStarted {
            session: info.id.clone(),
            account: self.config.account.clone(),
            brain: self.brain.describe(),
        });
        self.emit(Event::SandboxStatus { capabilities: self.sandbox.capabilities() });
        for source in self.knowledge.sources().await {
            self.emit(Event::SourceStatus { source });
        }
        let primer = match self.knowledge.primer(&self.config.workspace).await {
            Ok(primer) => primer,
            Err(err) => {
                self.emit(Event::Error { turn: None, message: format!("could not load the session primer: {err}") });
                Primer::default()
            }
        };
        self.session = Some(Session { id: info.id.clone(), primer, conversation: Vec::new() });
        Ok(info)
    }

    /// The command loop. Returns when the session ends ([`Command::Shutdown`] or the sender is
    /// dropped).
    ///
    /// Turns run one at a time. While a turn is in flight the loop keeps listening: an interrupt
    /// or shutdown signals the turn to cancel at its next cancellation point (brain stream,
    /// approval prompt or action), and messages that arrive meanwhile are queued.
    pub async fn run(&mut self, mut commands: mpsc::Receiver<Command>) {
        let (cancel_tx, cancel_rx) = watch::channel(false);
        self.cancel = cancel_rx;
        let mut queued: VecDeque<String> = VecDeque::new();
        let mut shutting_down = false;
        while !shutting_down {
            let command = match queued.pop_front() {
                Some(text) => Command::UserMessage(text),
                None => commands.recv().await.unwrap_or(Command::Shutdown),
            };
            let text = match command {
                Command::UserMessage(text) => text,
                Command::Interrupt => continue,
                Command::Shutdown => break,
            };
            if self.session.is_none() {
                self.emit(Event::Error { turn: None, message: "no session: start one before sending messages".into() });
                continue;
            }
            let turn = TurnId::new(format!("turn-{}", self.next_turn));
            self.next_turn += 1;
            cancel_tx.send_replace(false);
            {
                let turn_future = self.run_turn(text, &turn);
                tokio::pin!(turn_future);
                let mut listening = true;
                loop {
                    tokio::select! {
                        () = &mut turn_future => break,
                        command = commands.recv(), if listening => match command {
                            Some(Command::UserMessage(text)) => queued.push_back(text),
                            Some(Command::Interrupt) => {
                                cancel_tx.send_replace(true);
                            }
                            Some(Command::Shutdown) | None => {
                                cancel_tx.send_replace(true);
                                shutting_down = true;
                                listening = false;
                            }
                        },
                    }
                }
            }
            self.emit(Event::TurnDone { turn });
        }
        if let Some(session) = self.session.take() {
            self.emit(Event::SessionEnded { session: session.id });
        }
    }

    /// Runs one user turn until the brain is done, something fails, the step budget runs out
    /// or the turn is cancelled. The caller emits [`Event::TurnDone`].
    async fn run_turn(&mut self, text: String, turn: &TurnId) {
        self.emit(Event::TurnStarted { turn: turn.clone() });
        let message = Message::text(Role::User, text, self.now());
        self.record_message(turn, message, true).await;
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
                            // Stored, but kept out of what the brain sees: it isn't part of the
                            // model's own message (D-029).
                            let message = Message::text(Role::Agent, summary, self.now());
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
            self.finish_action(turn, request, Verdict::Deny(reason.into()), observation).await;
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
                        self.finish_action(turn, request, Verdict::Deny(INTERRUPTED.into()), observation).await;
                        return Flow::Cancelled;
                    }
                }
            }
            verdict => verdict,
        };

        let (observation, flow) = match &verdict {
            Verdict::Allow => {
                self.emit(Event::ActionStarted { turn: turn.clone(), action_id: request.id.clone() });
                match cancellable(&self.cancel, self.route(&request)).await {
                    None => (Observation::failed(request.id.clone(), INTERRUPTED), Flow::Cancelled),
                    Some(Ok(observation)) => (observation, Flow::Continue),
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
        self.finish_action(turn, request, verdict, observation).await;
        flow
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
    ) {
        self.emit(Event::ActionFinished { turn: turn.clone(), observation: observation.clone() });
        let record = ActionRecord { request, verdict, observation: Some(observation.clone()), at: self.now() };
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
            instructions: self.config.instructions.clone(),
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
