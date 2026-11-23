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
    /// List a directory.
    ListDir {
        /// Workspace-relative path. `""` or `"."` = workspace root.
        path: String,
    },
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
    pub const ALL: [SandboxActionKind; 7] =
        [Self::Exec, Self::ReadFile, Self::WriteFile, Self::ListDir, Self::JobStatus, Self::JobOutput, Self::JobStop];
}

impl SandboxAction {
    /// The kind of this action.
    pub fn kind(&self) -> SandboxActionKind {
        match self {
            Self::Exec { .. } => SandboxActionKind::Exec,
            Self::ReadFile { .. } => SandboxActionKind::ReadFile,
            Self::WriteFile { .. } => SandboxActionKind::WriteFile,
            Self::ListDir { .. } => SandboxActionKind::ListDir,
            Self::JobStatus { .. } => SandboxActionKind::JobStatus,
            Self::JobOutput { .. } => SandboxActionKind::JobOutput,
            Self::JobStop { .. } => SandboxActionKind::JobStop,
        }
    }

    /// Whether the action can change state. `Exec` counts as mutating.
    pub fn is_mutating(&self) -> bool {
        matches!(self, Self::Exec { .. } | Self::WriteFile { .. })
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
