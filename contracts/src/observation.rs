//! Results of actions.

use serde::{Deserialize, Serialize};

use crate::ActionId;
use crate::JobId;

/// One entry of a directory listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirEntry {
    /// File or directory name (no path).
    pub name: String,
    /// Whether it is a directory.
    pub is_dir: bool,
    /// Size in bytes, for files.
    pub size: Option<u64>,
}

/// What happened when an action ran. All content is untrusted data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Outcome {
    /// A command ran to completion (any exit code).
    Exec {
        /// Exit code; `None` if killed by a signal or limit.
        exit_code: Option<i32>,
        /// Captured stdout.
        stdout: String,
        /// Captured stderr.
        stderr: String,
    },
    /// A background job was started and is now running.
    Started {
        /// The job's id.
        job: JobId,
    },
    /// Text content: a file read, or a knowledge tool result.
    Text {
        /// The content.
        content: String,
    },
    /// A directory listing.
    Listing {
        /// The entries.
        entries: Vec<DirEntry>,
    },
    /// A file was written.
    Written {
        /// Bytes written.
        bytes: u64,
    },
    /// A knowledge entry was recorded.
    Recorded {
        /// Where it landed (e.g. a vault path), if meaningful.
        location: Option<String>,
    },
    /// The policy gate or the human refused the action.
    Denied {
        /// Why.
        reason: String,
    },
    /// The action ran but failed (missing file, path outside the workspace, bad arguments…).
    Failed {
        /// What went wrong, in words the agent can act on.
        message: String,
    },
}

/// The result of one action.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Observation {
    /// The [`ActionRequest`](crate::ActionRequest) this answers.
    pub action_id: ActionId,
    /// What happened.
    pub outcome: Outcome,
    /// Wall-clock duration.
    pub duration_ms: u64,
    /// Whether output was cut to fit limits.
    pub truncated: bool,
}

impl Observation {
    /// An observation with no timing or truncation.
    pub fn new(action_id: ActionId, outcome: Outcome) -> Self {
        Self { action_id, outcome, duration_ms: 0, truncated: false }
    }

    /// A [`Outcome::Denied`] observation.
    pub fn denied(action_id: ActionId, reason: impl Into<String>) -> Self {
        Self::new(action_id, Outcome::Denied { reason: reason.into() })
    }

    /// A [`Outcome::Failed`] observation.
    pub fn failed(action_id: ActionId, message: impl Into<String>) -> Self {
        Self::new(action_id, Outcome::Failed { message: message.into() })
    }
}
