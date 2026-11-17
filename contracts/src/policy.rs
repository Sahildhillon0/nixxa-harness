//! Policy vocabulary shared by the controller and the harness.

use serde::{Deserialize, Serialize};

/// The policy gate's decision on an action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verdict", content = "reason", rename_all = "snake_case")]
pub enum Verdict {
    /// Run it.
    Allow,
    /// Ask the human first.
    Ask(String),
    /// Refuse it.
    Deny(String),
}

/// How much the agent may do without asking. See DESIGN.md § Approval modes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalMode {
    /// Talk and read only.
    Locked,
    /// Reads allowed; writes and exec ask.
    #[default]
    Ask,
    /// Everything within budget allowed, still logged.
    Auto,
}
