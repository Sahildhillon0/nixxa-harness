//! Ports the harness implements for the controller.

use async_trait::async_trait;

use crate::{ActionRequest, Event, Verdict};

/// Asks the human about an action.
///
/// Contract: returns [`Verdict::Allow`] or [`Verdict::Deny`]. The controller treats any
/// [`Verdict::Ask`] as a denial.
#[async_trait]
pub trait ApprovalPort: Send + Sync {
    /// Shows the request and the policy's reason, and waits for a decision.
    async fn ask(&self, request: &ActionRequest, reason: &str) -> Verdict;
}

/// Receives events from the controller. Must not block; buffer or drop instead.
pub trait EventSink: Send + Sync {
    /// Delivers one event.
    fn emit(&self, event: Event);
}
