//! What the brain sees for one think step.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{AccountRef, Message, SandboxActionKind, SessionId, SourceId, ToolSpec};

/// The project the agent is working on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceInfo {
    /// Display name (usually the directory name).
    pub name: String,
    /// Absolute host path. Only host-side modules use it; the sandbox mounts it.
    pub root: PathBuf,
}

/// One block of start-of-session context from a knowledge source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrimerSection {
    /// Where it came from.
    pub source: SourceId,
    /// Heading, e.g. "Recent memory" or a note title.
    pub title: String,
    /// Content. Untrusted data.
    pub body: String,
}

/// Context gathered at session start (recent memory, project note).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Primer {
    /// Sections, in display order.
    pub sections: Vec<PrimerSection>,
}

/// Limits for one think step.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    /// Cap on generated tokens.
    pub max_output_tokens: Option<u32>,
}

/// Everything the brain needs for one think step.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Context {
    /// The session.
    pub session: SessionId,
    /// Which account to call the provider with.
    pub account: AccountRef,
    /// Model override; `None` = the brain's configured default.
    pub model: Option<String>,
    /// System instructions.
    pub instructions: String,
    /// Start-of-session context.
    pub primer: Primer,
    /// The conversation so far, oldest first.
    pub messages: Vec<Message>,
    /// Sandbox actions available this step. The brain renders them as tools.
    pub sandbox_actions: Vec<SandboxActionKind>,
    /// Knowledge tools available this step (from ready sources only).
    pub knowledge_tools: Vec<ToolSpec>,
    /// Limits.
    pub limits: Limits,
}
