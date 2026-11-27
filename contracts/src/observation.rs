//! Results of actions.

use serde::{Deserialize, Serialize};

use crate::{ActionId, JobId, NetRequest};

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
    /// Text content: a knowledge tool result, a job's status or output.
    Text {
        /// The content.
        content: String,
    },
    /// A file read: its contents and its hash, which a [`Patch`](crate::SandboxAction::Patch)
    /// of it must send back.
    File {
        /// The contents.
        content: String,
        /// The lowercase hex SHA-256 of the file's raw bytes.
        hash: String,
    },
    /// A patch was applied.
    Patched {
        /// What changed, as a unified diff.
        diff: String,
        /// The file's new hash, for the next patch.
        hash: String,
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
    /// The network requests the action made, allowed or blocked, in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub network: Vec<NetRequest>,
    /// What Nixxa adds about the result for the agent (e.g. that a file it wrote now holds a
    /// secret's placeholder, not the secret).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

impl Observation {
    /// An observation with no timing or truncation.
    pub fn new(action_id: ActionId, outcome: Outcome) -> Self {
        Self { action_id, outcome, duration_ms: 0, truncated: false, network: Vec::new(), warnings: Vec::new() }
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
