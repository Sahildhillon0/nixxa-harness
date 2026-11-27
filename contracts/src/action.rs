//! Actions the agent can propose. The controller routes on [`Action`].

use serde::{Deserialize, Serialize};

use crate::{ActionId, JobId, SourceId};

/// An action executed by the [`Sandbox`](crate::Sandbox).
///
/// Paths are workspace-relative and `/`-separated on every OS. The sandbox resolves them and
/// must refuse anything that escapes the workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SandboxAction {
    /// Run a shell command inside the sandbox.
    Exec {
        /// The command line, interpreted by the sandbox's shell.
        command: String,
        /// Working directory, workspace-relative. `None` = workspace root.
        cwd: Option<String>,
        /// Requested time limit. The sandbox may enforce a lower one.
        timeout_ms: Option<u64>,
        /// Start the command as a background job and return at once with its id (needs
        /// [`SandboxCapabilities::jobs`](crate::SandboxCapabilities::jobs)). Absent in records
        /// written before Stage 6, hence the default.
        #[serde(default)]
        background: bool,
    },
    /// Say whether a background job is still running, and how it ended.
    JobStatus {
        /// The job.
        job: JobId,
    },
    /// The last part of a background job's output.
    JobOutput {
        /// The job.
        job: JobId,
        /// How many bytes to show from the end of each of stdout and stderr. The sandbox may
        /// show fewer.
        tail_bytes: u64,
    },
    /// Stop a background job and everything it started.
    JobStop {
        /// The job.
        job: JobId,
    },
    /// Read a text file.
    ReadFile {
        /// Workspace-relative path.
        path: String,
    },
    /// Create or replace a text file.
    WriteFile {
        /// Workspace-relative path.
        path: String,
        /// The full new contents.
        content: String,
    },
    /// Change a text file by search/replace edits, against the state the agent last read
    /// ([`Outcome::File`](crate::Outcome::File)'s hash). `WriteFile` stays for new files.
    ///
    /// * the file must exist, be a regular file, and not be a symbolic link (never followed);
    /// * if its hash isn't `expected_hash`, nothing changes and the observation fails with the
    ///   file's current hash: a stale edit never clobbers;
    /// * the edits apply in order, each to the result of the ones before; each `search` must
    ///   match exactly once, and zero or several matches fail, saying which edit;
    /// * all the edits apply, or none do; the file is replaced at once, with its mode kept.
    ///
    /// On success the observation is [`Outcome::Patched`](crate::Outcome::Patched): a unified
    /// diff of what changed and the file's new hash.
    Patch {
        /// Workspace-relative path.
        path: String,
        /// The edits, in order.
        edits: Vec<Edit>,
        /// The file's hash as the agent last saw it: the lowercase hex SHA-256 of its raw bytes.
        expected_hash: String,
    },
    /// List a directory.
    ListDir {
        /// Workspace-relative path. `""` or `"."` = workspace root.
        path: String,
    },
}

/// One edit of a [`SandboxAction::Patch`]: `search` is replaced by `replace`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edit {
    /// Text that must occur exactly once in the file (after the edits before this one).
    pub search: String,
    /// What it becomes.
    pub replace: String,
}

/// The kind of a [`SandboxAction`], without its arguments.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxActionKind {
    /// [`SandboxAction::Exec`]
    Exec,
    /// [`SandboxAction::ReadFile`]
    ReadFile,
    /// [`SandboxAction::WriteFile`]
    WriteFile,
    /// [`SandboxAction::Patch`]
    Patch,
    /// [`SandboxAction::ListDir`]
    ListDir,
    /// [`SandboxAction::JobStatus`]
    JobStatus,
    /// [`SandboxAction::JobOutput`]
    JobOutput,
    /// [`SandboxAction::JobStop`]
    JobStop,
}

impl SandboxActionKind {
    /// Every kind, in a stable order.
    pub const ALL: [SandboxActionKind; 8] = [
        Self::Exec,
        Self::ReadFile,
        Self::WriteFile,
        Self::Patch,
        Self::ListDir,
        Self::JobStatus,
        Self::JobOutput,
        Self::JobStop,
    ];
}

impl SandboxAction {
    /// The kind of this action.
    pub fn kind(&self) -> SandboxActionKind {
        match self {
            Self::Exec { .. } => SandboxActionKind::Exec,
            Self::ReadFile { .. } => SandboxActionKind::ReadFile,
            Self::WriteFile { .. } => SandboxActionKind::WriteFile,
            Self::Patch { .. } => SandboxActionKind::Patch,
            Self::ListDir { .. } => SandboxActionKind::ListDir,
            Self::JobStatus { .. } => SandboxActionKind::JobStatus,
            Self::JobOutput { .. } => SandboxActionKind::JobOutput,
            Self::JobStop { .. } => SandboxActionKind::JobStop,
        }
    }

    /// Whether the action can change state. `Exec` counts as mutating.
    pub fn is_mutating(&self) -> bool {
        matches!(self, Self::Exec { .. } | Self::WriteFile { .. } | Self::Patch { .. })
    }

    /// Whether the action can change the workspace's files, so a checkpoint comes before it:
    /// `WriteFile`, `Patch` and every `Exec`, foreground or background.
    pub fn changes_files(&self) -> bool {
        matches!(self, Self::Exec { .. } | Self::WriteFile { .. } | Self::Patch { .. })
    }
}

/// A structured entry written to a knowledge source (session note, memory record).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnowledgeEntry {
    /// Short title.
    pub title: String,
    /// Markdown body.
    pub body: String,
    /// Free-form tags.
    pub tags: Vec<String>,
}

/// An action handled by [`Knowledge`](crate::Knowledge).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum KnowledgeAction {
    /// Call one of a source's tools (see [`ToolSpec`](crate::ToolSpec)). Whether it mutates is
    /// declared by the tool spec, which the controller consults for policy.
    Call {
        /// The source that owns the tool.
        source: SourceId,
        /// Tool name, as in its [`ToolSpec`](crate::ToolSpec).
        tool: String,
        /// Tool arguments, matching the tool's input schema.
        args: serde_json::Value,
    },
    /// Write a structured entry (e.g. session write-back). Always mutating.
    Record {
        /// Target source.
        source: SourceId,
        /// What to write.
        entry: KnowledgeEntry,
    },
}

/// Anything the agent can ask to do. The controller routes on the variant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "target", rename_all = "snake_case")]
pub enum Action {
    /// Routed to the [`Sandbox`](crate::Sandbox).
    Sandbox {
        /// The action.
        action: SandboxAction,
    },
    /// Routed to [`Knowledge`](crate::Knowledge).
    Knowledge {
        /// The action.
        action: KnowledgeAction,
    },
}

/// A proposed action with its identity and the agent's stated reason.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActionRequest {
    /// Correlates the request with its observation.
    pub id: ActionId,
    /// What to do.
    pub action: Action,
    /// Why the agent wants it (shown in approval prompts). Untrusted.
    pub reason: Option<String>,
}
