//! The ports the harness implements for the controller: an event sink and an approval port.
//! Both forward into the UI loop over one unbounded channel, so neither ever blocks the
//! controller.

use async_trait::async_trait;
use nixxa_contracts::{ActionRequest, ApprovalPort, Event, EventSink, Verdict};
use tokio::sync::{mpsc, oneshot};

/// What the UI loop receives from the controller side.
pub(crate) enum UiEvent {
    /// An event to render.
    Controller(Event),
    /// The controller is waiting for the human to decide on an action.
    Approval(PendingApproval),
    /// The setup screen's key was checked and saved, or why not.
    KeySaved(Result<nixxa_contracts::AccountInfo, String>),
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

/// Forwards controller events to the UI loop.
pub(crate) struct ChannelSink {
    tx: mpsc::UnboundedSender<UiEvent>,
}

impl ChannelSink {
    pub(crate) fn new(tx: mpsc::UnboundedSender<UiEvent>) -> Self {
        Self { tx }
    }
}

impl EventSink for ChannelSink {
    fn emit(&self, event: Event) {
        // The UI has gone away (quitting): there is nobody left to show it to.
        let _ = self.tx.send(UiEvent::Controller(event));
    }
}

/// Asks the human through the TUI's approval prompt.
pub(crate) struct TuiApprovals {
    tx: mpsc::UnboundedSender<UiEvent>,
}

impl TuiApprovals {
    pub(crate) fn new(tx: mpsc::UnboundedSender<UiEvent>) -> Self {
        Self { tx }
    }
}

#[async_trait]
impl ApprovalPort for TuiApprovals {
    async fn ask(&self, request: &ActionRequest, reason: &str) -> Verdict {
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
        let approvals = TuiApprovals::new(tx);
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
        let verdict = TuiApprovals::new(tx).ask(&request(), "why").await;
        assert_eq!(verdict, Verdict::Deny("the approval prompt is closed".into()));
    }

    #[tokio::test]
    async fn a_prompt_knows_when_the_controller_stopped_waiting() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let approvals = TuiApprovals::new(tx);
        let asking = tokio::spawn(async move { approvals.ask(&request(), "why").await });
        let Some(UiEvent::Approval(prompt)) = rx.recv().await else { panic!("expected a prompt") };
        assert!(!prompt.is_abandoned());
        asking.abort();
        let _ = asking.await;
        assert!(prompt.is_abandoned());
    }
}
