//! Workspace checkpoints, for undo: what changed between two states of the workspace.

use serde::{Deserialize, Serialize};

/// How the workspace differs between two states, `before` and `after`, path by path.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceDiff {
    /// Every path that differs, sorted by path. A directory added or removed is listed along
    /// with everything in it.
    pub changes: Vec<FileChange>,
    /// What checkpoints leave out in this workspace (e.g. `target/`, `node_modules/`), so
    /// restoring a checkpoint leaves those as they are. Shown wherever an undo is offered.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub not_covered: Vec<String>,
}

impl WorkspaceDiff {
    /// Whether nothing differs.
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }
}

/// One path that differs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChange {
    /// Workspace-relative, `/`-separated.
    pub path: String,
    /// What was there before; `None` when it was created since.
    pub before: Option<EntryState>,
    /// What is there after; `None` when it was deleted since.
    pub after: Option<EntryState>,
    /// A unified diff of the contents, for text files of a reasonable size whose contents
    /// changed. Untrusted text, like any file content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
}

/// What a path is, as far as a checkpoint is concerned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EntryState {
    /// A regular file.
    File {
        /// Its size in bytes.
        size: u64,
        /// Its permission bits (`0o644`, `0o755`…); on Windows `0o444` when read-only, else
        /// `0o644`.
        mode: u32,
        /// The lowercase hex SHA-256 of its contents.
        hash: String,
    },
    /// A directory.
    Dir {
        /// Its permission bits.
        mode: u32,
    },
    /// A symbolic link, kept as a link: its target is never followed.
    Symlink {
        /// Where it points, as written in the link.
        target: String,
    },
}
