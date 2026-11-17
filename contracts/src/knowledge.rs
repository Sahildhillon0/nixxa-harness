//! The [`Knowledge`] contract: external memory and context sources.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{ActionId, KnowledgeEntry, KnowledgeError, Observation, Primer, SourceId, WorkspaceInfo};

/// Whether a source can be used right now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", content = "reason", rename_all = "snake_case")]
pub enum SourceStatus {
    /// Found and working.
    Ready,
    /// Not installed / not found. Not an error.
    Missing,
    /// Found but failing.
    Error(String),
}

/// A knowledge source and its state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceInfo {
    /// Stable id, e.g. `obsidian`.
    pub id: SourceId,
    /// Display name, e.g. "Obsidian vault".
    pub name: String,
    /// Current state.
    pub status: SourceStatus,
}

/// A tool a knowledge source offers to the agent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    /// The owning source.
    pub source: SourceId,
    /// Tool name, unique within the source.
    pub name: String,
    /// What it does, for the model.
    pub description: String,
    /// JSON Schema of the arguments.
    pub input_schema: serde_json::Value,
    /// Whether calling it changes anything (used by the policy gate).
    pub mutating: bool,
}

/// Connects Nixxa to memory and context that live outside it.
///
/// Contract:
/// * [`Knowledge::tools`] lists tools of [`SourceStatus::Ready`] sources only;
/// * an unknown or non-ready source returns [`KnowledgeError::SourceUnavailable`];
/// * an unknown tool on a ready source returns [`KnowledgeError::UnknownTool`];
/// * every primer section names a known source;
/// * the returned observation's `action_id` equals the given `id`.
#[async_trait]
pub trait Knowledge: Send + Sync {
    /// All configured sources and their status.
    async fn sources(&self) -> Vec<SourceInfo>;

    /// Tools of ready sources.
    async fn tools(&self) -> Vec<ToolSpec>;

    /// Start-of-session context for a workspace (may be empty).
    async fn primer(&self, workspace: &WorkspaceInfo) -> Result<Primer, KnowledgeError>;

    /// Calls a source's tool.
    async fn call(
        &self,
        id: ActionId,
        source: &SourceId,
        tool: &str,
        args: serde_json::Value,
    ) -> Result<Observation, KnowledgeError>;

    /// Writes a structured entry to a source.
    async fn record(
        &self,
        id: ActionId,
        source: &SourceId,
        entry: KnowledgeEntry,
    ) -> Result<Observation, KnowledgeError>;
}
