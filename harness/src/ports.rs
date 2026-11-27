//! The ports the harness implements for the controller: an event sink and an approval port.
//! Both forward into the UI loop over one unbounded channel, so neither ever blocks the
//! controller.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use async_trait::async_trait;
use nixxa_contracts::{ActionId, ActionRequest, ApprovalPort, Event, EventSink, Verdict};
use tokio::sync::{mpsc, oneshot};

/// What the UI loop receives from the controller side.
pub(crate) enum UiEvent {
    /// An event to render.
    Controller(Event),
    /// The controller is waiting for the human to decide on an action.
    Approval(PendingApproval),
    /// The setup screen's account was checked and saved, or why not.
    AccountSaved(Result<nixxa_contracts::AccountInfo, String>),
    /// The local server's models, or why they couldn't be read.
    ModelsListed(Result<Vec<nixxa_brain::ModelInfo>, String>),
    /// The sign-in listener is up: the page to open (or why it couldn't start).
    SignInStarted(Result<String, String>),
    /// The browser came back with a key (or why not). The key goes no further than the UI loop,
    /// which holds it until a model is chosen.
    SignedIn(Result<nixxa_contracts::Secret, String>),
    /// Auto mode approved this action without asking (D-052).
    AutoApproved(ActionId),
    /// The stored sessions for the session list (newest first), or why not.
    SessionsListed(Result<Vec<nixxa_contracts::SessionInfo>, String>),
    /// Transcript matches for the session list's query, or why not.
    SessionsFound { query: String, result: Result<Vec<nixxa_contracts::SearchHit>, String> },
    /// The sandbox now applies this network policy, or why it couldn't.
    NetworkSet(Result<nixxa_contracts::NetworkPolicy, String>),
    /// Something happened in the sandbox (a network request; later, jobs ending).
    Sandbox(nixxa_contracts::SandboxEvent),
    /// What changed since a checkpoint, for the undo view, or why it couldn't be told.
    UndoDiff { checkpoint: nixxa_contracts::CheckpointId, result: Result<nixxa_contracts::WorkspaceDiff, String> },
}

/// Which session the UI shows. Each controller's sink is tied to the generation it was made in;
/// starting another session moves on, so events a replaced session sends while it shuts down
/// (its interrupt notice, its last `TurnDone`) never land in the new one's conversation.
#[derive(Debug, Clone, Default)]
pub(crate) struct Generation(Arc<AtomicU64>);

impl Generation {
    fn current(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }

    /// Stops showing events from every sink made so far.
    pub(crate) fn advance(&self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// What a controller needs to reach the UI.
#[derive(Debug, Clone)]
pub(crate) struct UiLink {
    pub(crate) tx: mpsc::UnboundedSender<UiEvent>,
    pub(crate) auto: AutoApprove,
    pub(crate) generation: Generation,
}

impl UiLink {
    pub(crate) fn new(tx: mpsc::UnboundedSender<UiEvent>, auto: AutoApprove) -> Self {
        Self { tx, auto, generation: Generation::default() }
    }
}

/// Whether actions are approved without asking (Auto mode, D-052). Shared by the TUI, which
/// switches it, and [`TuiApprovals`], which reads it. Off at every launch.
#[derive(Debug, Clone, Default)]
pub(crate) struct AutoApprove(Arc<AtomicBool>);

impl AutoApprove {
    pub(crate) fn set(&self, on: bool) {
        self.0.store(on, Ordering::SeqCst);
    }

    pub(crate) fn is_on(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// An approval prompt waiting for the human.
pub(crate) struct PendingApproval {
    pub(crate) request: ActionRequest,
    pub(crate) reason: String,
    reply: oneshot::Sender<Verdict>,
}

impl PendingApproval {
    /// Answers the prompt. A controller that stopped waiting (interrupt) is not an error.
    pub(crate) fn answer(self, verdict: Verdict) {
        let _ = self.reply.send(verdict);
    }

    /// A prompt for tests, with the receiving end of its answer.
    #[cfg(test)]
    pub(crate) fn for_test(request: ActionRequest, reason: &str) -> (Self, oneshot::Receiver<Verdict>) {
        let (reply, answer) = oneshot::channel();
        (Self { request, reason: reason.to_string(), reply }, answer)
    }

    /// Whether the controller stopped waiting for this answer.
    pub(crate) fn is_abandoned(&self) -> bool {
        self.reply.is_closed()
    }
}

/// Forwards controller events to the UI loop while its session is the one shown.
pub(crate) struct ChannelSink {
    tx: mpsc::UnboundedSender<UiEvent>,
    generation: Generation,
    mine: u64,
}

impl ChannelSink {
    pub(crate) fn new(tx: mpsc::UnboundedSender<UiEvent>, generation: Generation) -> Self {
        let mine = generation.current();
        Self { tx, generation, mine }
    }
}

impl EventSink for ChannelSink {
    fn emit(&self, event: Event) {
        if self.generation.current() != self.mine {
            // A replaced session winding down: its events belong to a conversation no longer shown.
            return;
        }
        // The UI has gone away (quitting): there is nobody left to show it to.
        let _ = self.tx.send(UiEvent::Controller(event));
    }
}

/// Asks the human through the TUI's approval prompt, or, in Auto mode, approves straight away
/// and tells the UI so the action is shown as auto-approved.
pub(crate) struct TuiApprovals {
    tx: mpsc::UnboundedSender<UiEvent>,
    auto: AutoApprove,
}

impl TuiApprovals {
    pub(crate) fn new(tx: mpsc::UnboundedSender<UiEvent>, auto: AutoApprove) -> Self {
        Self { tx, auto }
    }
}

#[async_trait]
impl ApprovalPort for TuiApprovals {
    async fn ask(&self, request: &ActionRequest, reason: &str) -> Verdict {
        if self.auto.is_on() {
            let _ = self.tx.send(UiEvent::AutoApproved(request.id.clone()));
            crate::logging::approval(&request.id, &Verdict::Allow);
            return Verdict::Allow;
        }
        let (reply, answer) = oneshot::channel();
        let prompt = PendingApproval { request: request.clone(), reason: reason.to_string(), reply };
        if self.tx.send(UiEvent::Approval(prompt)).is_err() {
            return Verdict::Deny("the approval prompt is closed".into());
        }
        let verdict = answer.await.unwrap_or_else(|_| Verdict::Deny("the approval prompt is closed".into()));
        crate::logging::approval(&request.id, &verdict);
        verdict
    }
}

#[cfg(test)]
mod tests {
    use nixxa_contracts::{Action, ActionId, SandboxAction};

    use super::*;

    fn request() -> ActionRequest {
        ActionRequest {
            id: ActionId::new("a"),
            action: Action::Sandbox { action: SandboxAction::ListDir { path: ".".into() } },
            reason: None,
        }
    }

    #[tokio::test]
    async fn approvals_round_trip_through_the_ui_channel() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let approvals = TuiApprovals::new(tx, AutoApprove::default());
        let ui = tokio::spawn(async move {
            let Some(UiEvent::Approval(prompt)) = rx.recv().await else { panic!("expected a prompt") };
            assert_eq!(prompt.reason, "why");
            prompt.answer(Verdict::Allow);
        });
        assert_eq!(approvals.ask(&request(), "why").await, Verdict::Allow);
        ui.await.unwrap();
    }

    #[tokio::test]
    async fn a_closed_ui_denies() {
        let (tx, rx) = mpsc::unbounded_channel();
        drop(rx);
        let verdict = TuiApprovals::new(tx, AutoApprove::default()).ask(&request(), "why").await;
        assert_eq!(verdict, Verdict::Deny("the approval prompt is closed".into()));
    }

    #[tokio::test]
    async fn auto_mode_approves_without_a_prompt_and_says_so() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let auto = AutoApprove::default();
        let approvals = TuiApprovals::new(tx, auto.clone());
        auto.set(true);
        assert_eq!(approvals.ask(&request(), "why").await, Verdict::Allow);
        match rx.try_recv() {
            Ok(UiEvent::AutoApproved(id)) => assert_eq!(id, ActionId::new("a")),
            _ => panic!("expected the action to be reported as auto-approved, not a prompt"),
        }
        auto.set(false);
        let asking = tokio::spawn(async move { approvals.ask(&request(), "why").await });
        let Some(UiEvent::Approval(prompt)) = rx.recv().await else { panic!("Ask mode prompts again") };
        prompt.answer(Verdict::Allow);
        asking.await.unwrap();
    }

    #[tokio::test]
    async fn a_prompt_knows_when_the_controller_stopped_waiting() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let approvals = TuiApprovals::new(tx, AutoApprove::default());
        let asking = tokio::spawn(async move { approvals.ask(&request(), "why").await });
        let Some(UiEvent::Approval(prompt)) = rx.recv().await else { panic!("expected a prompt") };
        assert!(!prompt.is_abandoned());
        asking.abort();
        let _ = asking.await;
        assert!(prompt.is_abandoned());
    }

    #[test]
    fn a_replaced_sessions_events_are_dropped() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let generation = Generation::default();
        let old = ChannelSink::new(tx.clone(), generation.clone());
        old.emit(Event::Error { turn: None, message: "shown".into() });
        generation.advance();
        let new = ChannelSink::new(tx, generation);
        old.emit(Event::Error { turn: None, message: "dropped".into() });
        new.emit(Event::Error { turn: None, message: "shown too".into() });
        let mut seen = Vec::new();
        while let Ok(UiEvent::Controller(Event::Error { message, .. })) = rx.try_recv() {
            seen.push(message);
        }
        assert_eq!(seen, vec!["shown", "shown too"]);
    }
}
