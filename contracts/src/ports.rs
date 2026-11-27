//! Ports the harness implements for the controller.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{ActionRequest, Event, Verdict};

/// Whether the approval mode may answer for the human.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Asking {
    /// As the mode says: in Auto the action is approved without a prompt.
    ByMode,
    /// The human is asked even in Auto (reading a file that usually holds secrets).
    Always,
}

/// Asks the human about an action.
///
/// Contract: returns [`Verdict::Allow`] or [`Verdict::Deny`]. The controller treats any
/// [`Verdict::Ask`] as a denial. With [`Asking::Always`] a mode that approves by itself (Auto)
/// must still ask.
#[async_trait]
pub trait ApprovalPort: Send + Sync {
    /// Shows the request and the policy's reason, and waits for a decision.
    async fn ask(&self, request: &ActionRequest, reason: &str, asking: Asking) -> Verdict;
}

/// Receives events from the controller. Must not block; buffer or drop instead.
pub trait EventSink: Send + Sync {
    /// Delivers one event.
    fn emit(&self, event: Event);
}
