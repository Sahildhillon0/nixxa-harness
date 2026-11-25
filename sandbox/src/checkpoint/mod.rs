//! Workspace checkpoints (D-070, D-071): a small content-addressed store of our own, on the
//! host, outside the workspace and never mounted into the sandbox.
//!
//! * **Blobs** (file contents) are files named by their SHA-256 under `objects/`, written once:
//!   a file that didn't change costs nothing in the next checkpoint.
//! * **Trees** (what each directory holds: files with their hash and mode, directories, links
//!   with their target, never followed) are kept in memory. A checkpoint is the root tree;
//!   directories that didn't change share their tree with the checkpoint before.
//! * **Unchanged files are not read again:** a file whose device, inode, size, mtime and ctime
//!   match the last snapshot keeps its hash. The ctime is there because a command can set the
//!   mtime back (`touch -d`) but not the ctime. A file changed within the last two seconds
//!   isn't trusted that way, since a change in the same clock tick wouldn't show.
//! * **Left out** (not covered): directories with a fixed set of names that hold rebuilt
//!   things (`target`, `node_modules`, …; [`EXCLUDED`]) at any depth, and special files
//!   (FIFOs, sockets, devices), which are never opened. Nothing in the workspace decides what is
//!   left out: `.gitignore` is not read, so the agent can't use it to hide a file from a
//!   checkpoint. Restoring leaves what isn't covered as it is.
//! * **The cap:** the blobs of one store may not pass `cap` bytes. A checkpoint that would is
//!   refused ([`SandboxError::LimitExceeded`]), and nothing of it is kept.
//!
//! The workspace is the agent's, so every entry is treated as hostile: see `unix.rs` for how
//! nothing is followed or opened that shouldn't be. Restoring removes what shouldn't be there
//! (a link stands where a directory was: the link goes, and the directory is made again) and
//! writes files as new files renamed into place.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nixxa_contracts::{CheckpointId, EntryState, FileChange, SandboxError, WorkspaceDiff};

#[cfg(unix)]
mod unix;
#[cfg(unix)]
use unix as sys;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as sys;

use sys::Dir;

/// Directory names whose contents checkpoints leave out, wherever they are: build output and
/// installed dependencies, which are rebuilt rather than restored. Fixed here; nothing in the
/// workspace changes it.
pub const EXCLUDED: [&str; 14] = [
    "target",
    "node_modules",
    ".venv",
    "venv",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".tox",
    ".gradle",
    ".next",
    ".nuxt",
    ".turbo",
    ".parcel-cache",
];

/// The default size cap of one store: 2 GiB of file contents.
pub const DEFAULT_CAP: u64 = 2 * 1024 * 1024 * 1024;
/// File contents kept in memory for the diff against "now", per file.
const PEEK_CAP: u64 = 256 * 1024;
/// The text diffs of one [`WorkspaceDiff`], in all; past it, changes are listed without one.
const DIFFS_CAP: usize = 1024 * 1024;
/// A file changed this recently is hashed again next time, whatever its metadata says.
const RACY: Duration = Duration::from_secs(2);
/// How deep a snapshot goes. A workspace nested deeper can't be checkpointed (it says so), and
/// is not looked into when showing how things are now. It bounds the snapshot's recursion (and
/// the restore's and the diff's, which follow the snapshots) and the descriptors it holds.
pub(crate) const MAX_DEPTH: usize = 256;
/// How deep removing a tree goes before it moves what is below up beside it, for a later
/// round: removing a tree of any depth holds this many descriptors at most, and recursion
/// this deep at most.
const REMOVE_DEPTH: usize = 64;
/// A file in the store, rewritten at every checkpoint: a store whose `alive` is old belongs to
/// a Nixxa that ended without removing it.
pub const ALIVE: &str = "alive";

/// Where a sandbox keeps its checkpoints, and how much they may hold. The harness chooses it.
#[derive(Debug, Clone)]
pub struct CheckpointConfig {
    /// The store's directory. It must not exist yet; it is created (0700) and removed when the
    /// sandbox is dropped. Its name prefixes every checkpoint id, so ids from another store
    /// are never taken for this one's.
    pub dir: PathBuf,
    /// The most bytes of file contents the store may hold.
    pub cap: u64,
}

// ---- What a snapshot records ----

/// What a directory entry is, from its metadata (links not followed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    File,
    Dir,
    Symlink,
    /// A FIFO, socket or device: never opened, not covered.
    Other,
}

/// What says a file is unchanged without reading it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Key {
    pub(crate) dev: u64,
    pub(crate) ino: u64,
    pub(crate) size: u64,
    pub(crate) mtime: i128,
    pub(crate) ctime: i128,
}

/// An entry's metadata.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Meta {
    pub(crate) kind: Kind,
    pub(crate) mode: u32,
    pub(crate) size: u64,
    pub(crate) key: Key,
}

type Hash = [u8; 32];
type Tree = BTreeMap<OsString, Node>;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Node {
    File {
        hash: Hash,
        size: u64,
        mode: u32,
    },
    Dir {
        mode: u32,
        tree: Arc<Tree>,
    },
    Symlink {
        target: Vec<u8>,
    },
    /// A directory whose contents are left out ([`EXCLUDED`]).
    Skipped {
        mode: u32,
    },
    /// A file or directory that wasn't read while looking at how things are now: its owner
    /// took away the rights, a file bigger than the store's cap, or a directory past
    /// [`MAX_DEPTH`]. Never in a checkpoint: a checkpoint fails instead.
    Unreadable {
        kind: Kind,
        mode: u32,
        size: u64,
    },
}

impl Node {
    fn kind(&self) -> Kind {
        match self {
            Self::File { .. } => Kind::File,
            Self::Dir { .. } | Self::Skipped { .. } => Kind::Dir,
            Self::Symlink { .. } => Kind::Symlink,
            Self::Unreadable { kind, .. } => *kind,
        }
    }

    fn state(&self) -> EntryState {
        match self {
            Self::File { hash, size, mode } => EntryState::File { size: *size, mode: *mode, hash: hex(hash) },
            Self::Dir { mode, .. } | Self::Skipped { mode } => EntryState::Dir { mode: *mode },
            Self::Symlink { target } => EntryState::Symlink { target: String::from_utf8_lossy(target).into_owned() },
            Self::Unreadable { kind: Kind::Dir, mode, .. } => EntryState::Dir { mode: *mode },
            Self::Unreadable { mode, size, .. } => {
                EntryState::File { size: *size, mode: *mode, hash: "(unreadable)".into() }
            }
        }
    }
}

/// One checkpoint: the root's mode and what it held.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Snapshot {
    mode: u32,
    tree: Arc<Tree>,
    /// What was left out, for the user.
    not_covered: Vec<String>,
}

fn hex(hash: &Hash) -> String {
    hash.iter().map(|b| format!("{b:02x}")).collect()
}

fn excluded(name: &OsStr) -> bool {
    name.to_str().is_some_and(|name| EXCLUDED.contains(&name))
}

/// A workspace-relative path for people: `/`-separated, lossy where a name isn't UTF-8.
fn shown(rel: &[u8]) -> String {
    String::from_utf8_lossy(rel).into_owned()
}

fn child(rel: &[u8], name: &OsStr) -> Vec<u8> {
    let mut out = rel.to_vec();
    if !out.is_empty() {
        out.push(b'/');
    }
    out.extend(sys::name_bytes(name));
    out
}

/// Why a checkpoint or restore stopped.
#[derive(Debug)]
enum Failure {
    /// The store's cap.
    Cap(String),
    /// Anything else, in words.
    Io(String),
}

impl From<Failure> for SandboxError {
    fn from(failure: Failure) -> Self {
        match failure {
            Failure::Cap(why) => SandboxError::LimitExceeded(why),
            Failure::Io(why) => SandboxError::Other(why),
        }
    }
}

fn io_at(what: &str, rel: &[u8], err: &io::Error) -> Failure {
    let path = if rel.is_empty() { "the workspace".to_string() } else { format!("`{}`", shown(rel)) };
    Failure::Io(format!("{what} {path}: {err}"))
}

// ---- The store ----

struct Store {
    workspace: PathBuf,
    dir: PathBuf,
    /// Prefixes every checkpoint id.
    tag: String,
    cap: u64,
    used: u64,
    blobs: HashSet<Hash>,
    taken: Vec<Snapshot>,
    /// What the last snapshot learnt: path → (metadata, hash).
    known: HashMap<Vec<u8>, (Key, Hash)>,
    temp: u64,
}

/// What a snapshot does with the contents it reads.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Keep {
    /// Stores them as blobs: a checkpoint.
    Store,
    /// Only hashes them, keeping small ones in memory for a diff: how things are now.
    Peek,
}

/// One snapshot in progress.
struct Walk<'s> {
    store: &'s mut Store,
    keep: Keep,
    /// Blobs this snapshot added, removed again if it fails.
    added: Vec<(Hash, u64)>,
    peeked: HashMap<Hash, Vec<u8>>,
    known: HashMap<Vec<u8>, (Key, Hash)>,
    not_covered: BTreeSet<String>,
    started: SystemTime,
    /// The store's own directory, should it be inside the workspace: never snapshotted.
    store_id: Option<(u64, u64)>,
}

impl Store {
    fn blob_path(&self, hash: &Hash) -> PathBuf {
        let hex = hex(hash);
        self.dir.join("objects").join(&hex[..2]).join(&hex[2..])
    }

    fn id(&self, n: usize) -> CheckpointId {
        CheckpointId::new(format!("{}.{n}", self.tag))
    }

    fn find(&self, id: &CheckpointId) -> Result<&Snapshot, SandboxError> {
        id.as_str()
            .strip_prefix(&self.tag)
            .and_then(|rest| rest.strip_prefix('.'))
            .and_then(|n| n.parse::<usize>().ok())
            .and_then(|n| self.taken.get(n))
            .ok_or_else(|| SandboxError::NoCheckpoint(id.clone()))
    }

    /// Reads the workspace as it is now.
    fn snapshot(&mut self, keep: Keep) -> Result<(Snapshot, HashMap<Hash, Vec<u8>>), Failure> {
        let root = Dir::open_root(&self.workspace).map_err(|err| io_at("can't open", b"", &err))?;
        let mode = root.meta().map_err(|err| io_at("can't read", b"", &err))?.mode;
        let store_id = store_identity(&self.dir);
        let previous = self.taken.last().map(|s| s.tree.clone());
        let mut walk = Walk {
            store: self,
            keep,
            added: Vec::new(),
            peeked: HashMap::new(),
            known: HashMap::new(),
            not_covered: BTreeSet::new(),
            started: SystemTime::now(),
            store_id,
        };
        match walk.tree(&root, &[], previous.as_ref(), 0) {
            Ok(tree) => {
                let Walk { known, not_covered, peeked, .. } = walk;
                self.known = known;
                Ok((Snapshot { mode, tree, not_covered: not_covered.into_iter().collect() }, peeked))
            }
            Err(failure) => {
                for (hash, size) in std::mem::take(&mut walk.added) {
                    let _ = std::fs::remove_file(walk.store.blob_path(&hash));
                    walk.store.blobs.remove(&hash);
                    walk.store.used -= size;
                }
                Err(failure)
            }
        }
    }

    fn checkpoint(&mut self) -> Result<CheckpointId, Failure> {
        let (snapshot, _) = self.snapshot(Keep::Store)?;
        if let Some(last) = self.taken.last() {
            if *last == snapshot {
                return Ok(self.id(self.taken.len() - 1));
            }
        }
        self.taken.push(snapshot);
        Ok(self.id(self.taken.len() - 1))
    }

    /// Says the store is in use (see [`ALIVE`]).
    fn alive(&self) {
        let _ = std::fs::write(self.dir.join(ALIVE), std::process::id().to_string());
    }

    fn blob(&self, hash: &Hash) -> Option<Vec<u8>> {
        std::fs::read(self.blob_path(hash)).ok()
    }

    /// What changed from `before` to `after`; `peeked` holds contents of `after` not stored.
    fn diff(&self, before: &Snapshot, after: &Snapshot, peeked: &HashMap<Hash, Vec<u8>>) -> WorkspaceDiff {
        let mut changes = Vec::new();
        let mut budget = DIFFS_CAP;
        let content = |hash: &Hash, size: u64| -> Option<Vec<u8>> {
            if size > PEEK_CAP {
                return None;
            }
            peeked.get(hash).cloned().or_else(|| self.blob(hash))
        };
        compare(&before.tree, &after.tree, &[], &mut |path, b, a| {
            let mut diff = None;
            if let (Some(Node::File { hash: h0, size: s0, .. }), Some(Node::File { hash: h1, size: s1, .. })) = (b, a) {
                if h0 != h1 && budget > 0 {
                    if let (Some(old), Some(new)) = (content(h0, *s0), content(h1, *s1)) {
                        if !old.contains(&0) && !new.contains(&0) {
                            let text = crate::patch::unified(&shown(path), &old, &new);
                            budget = budget.saturating_sub(text.len());
                            diff = Some(text);
                        }
                    }
                }
            }
            changes.push(FileChange { path: shown(path), before: b.map(Node::state), after: a.map(Node::state), diff });
        });
        changes.sort_by(|a, b| a.path.cmp(&b.path));
        let mut not_covered: Vec<String> = before.not_covered.iter().chain(&after.not_covered).cloned().collect();
        not_covered.sort();
        not_covered.dedup();
        WorkspaceDiff { changes, not_covered }
    }

    fn restore(&mut self, id: &CheckpointId) -> Result<WorkspaceDiff, SandboxError> {
        let target = self.find(id)?.clone();
        // Every blob must still be there before anything is touched.
        let mut missing = false;
        each_file(&target.tree, &mut |hash| missing |= !self.blob_path(hash).is_file());
        if missing {
            return Err(SandboxError::NoCheckpoint(id.clone()));
        }
        let (now, peeked) = self.snapshot(Keep::Peek)?;
        let undone = self.diff(&target, &now, &peeked);
        let root = Dir::open_root(&self.workspace).map_err(|err| io_at("can't open", b"", &err))?;
        // Owner rights while working; the recorded mode at the end.
        root.set_mode(target.mode | 0o700).map_err(|err| io_at("can't change", b"", &err))?;
        let result = self.restore_dir(&root, &target.tree, Some(&now.tree), &[]);
        root.set_mode(target.mode).map_err(|err| io_at("can't change", b"", &err))?;
        result.map_err(|failure| match failure {
            Failure::Io(why) => {
                SandboxError::Other(format!("the restore stopped partway, so the workspace is partly restored: {why}"))
            }
            cap => cap.into(),
        })?;
        Ok(undone)
    }

    fn restore_dir(&mut self, dir: &Dir, target: &Tree, now: Option<&Tree>, rel: &[u8]) -> Result<(), Failure> {
        // What shouldn't be here goes: not in the checkpoint, or not the same kind of thing.
        let entries = dir.list().map_err(|err| io_at("can't list", rel, &err))?;
        for (name, meta) in entries {
            let keep = match target.get(&name) {
                Some(node) => node.kind() == meta.kind,
                // Not covered, so left as it is.
                None => meta.kind == Kind::Dir && excluded(&name),
            };
            if !keep {
                remove_tree(dir, &name, meta.kind, &child(rel, &name))?;
            }
        }
        for (name, node) in target {
            let path = child(rel, name);
            let was = now.and_then(|now| now.get(name));
            match node {
                Node::Dir { mode, tree } => {
                    let sub = open_or_make(dir, name, &path)?;
                    sub.set_mode(0o700).map_err(|err| io_at("can't change", &path, &err))?;
                    let was = match was {
                        Some(Node::Dir { tree, .. }) => Some(tree.as_ref()),
                        _ => None,
                    };
                    self.restore_dir(&sub, tree, was, &path)?;
                    sub.set_mode(*mode).map_err(|err| io_at("can't change", &path, &err))?;
                }
                Node::Unreadable { .. } => {}
                Node::Skipped { mode } => {
                    if !matches!(was, Some(Node::Skipped { .. })) {
                        if let Ok(sub) = open_or_make(dir, name, &path) {
                            let _ = sub.set_mode(*mode);
                        }
                    }
                }
                Node::File { hash, mode, .. } => {
                    if was != Some(node) {
                        self.write_file(dir, name, hash, *mode, &path)?;
                    }
                }
                Node::Symlink { target } => {
                    if was != Some(node) {
                        let (temp, ()) = self
                            .fresh(|temp| dir.symlink(target, temp))
                            .map_err(|err| io_at("can't make the link", &path, &err))?;
                        if let Err(err) = dir.rename(&temp, name) {
                            let _ = dir.remove(&temp, false);
                            return Err(io_at("can't put back", &path, &err));
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Writes a stored blob as a new file beside `name`, then renames it over `name`.
    fn write_file(&mut self, dir: &Dir, name: &OsStr, hash: &Hash, mode: u32, path: &[u8]) -> Result<(), Failure> {
        let mut source = std::fs::File::open(self.blob_path(hash))
            .map_err(|err| io_at("the store lost the contents of", path, &err))?;
        let (temp, mut file) = self.fresh(|temp| dir.create(temp)).map_err(|err| io_at("can't write", path, &err))?;
        let written = io::copy(&mut source, &mut file)
            .and_then(|_| file.flush())
            .and_then(|()| sys::set_file_mode(&file, mode))
            .and_then(|()| {
                drop(file);
                dir.rename(&temp, name)
            });
        if let Err(err) = written {
            let _ = dir.remove(&temp, false);
            return Err(io_at("can't put back", path, &err));
        }
        Ok(())
    }

    /// Makes something under a new temporary name beside its target. A name that is taken
    /// (the agent may have put a file there) is skipped for the next.
    fn fresh<T>(&mut self, mut make: impl FnMut(&OsStr) -> io::Result<T>) -> io::Result<(OsString, T)> {
        let mut tries = 0;
        loop {
            self.temp += 1;
            let name = OsString::from(format!(".nixxa-restore-{}-{}", std::process::id(), self.temp));
            match make(&name) {
                Ok(made) => return Ok((name, made)),
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists && tries < 16 => tries += 1,
                Err(err) => return Err(err),
            }
        }
    }
}

/// The store directory's device and inode, so a workspace that contains it skips it.
fn store_identity(dir: &Path) -> Option<(u64, u64)> {
    let meta = Dir::open_root(dir).ok()?.meta().ok()?;
    (meta.key.ino != 0).then_some((meta.key.dev, meta.key.ino))
}

fn open_or_make(dir: &Dir, name: &OsStr, path: &[u8]) -> Result<Dir, Failure> {
    match dir.open_dir(name) {
        Ok(sub) => Ok(sub),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            dir.make_dir(name).map_err(|err| io_at("can't make", path, &err))?;
            dir.open_dir(name).map_err(|err| io_at("can't open", path, &err))
        }
        Err(err) if err.kind() == io::ErrorKind::PermissionDenied => {
            dir.unlock(name).map_err(|err| io_at("can't open", path, &err))?;
            dir.open_dir(name).map_err(|err| io_at("can't open", path, &err))
        }
        // A link (or anything else) where a directory should be, put there since the entries
        // were listed: stop rather than follow it.
        Err(err) => Err(io_at("can't open", path, &err)),
    }
}

/// Removes an entry, and everything in it for a directory, without following anything, at any
/// depth: a directory [`REMOVE_DEPTH`] levels down is moved up beside the tree (under a new
/// name in `dir`) and removed in a later round, so neither the recursion nor the descriptors
/// held grow with the tree's depth, and every directory is still opened about once.
fn remove_tree(dir: &Dir, name: &OsStr, kind: Kind, path: &[u8]) -> Result<(), Failure> {
    if kind != Kind::Dir {
        return match dir.remove(name, false) {
            Err(err) if err.kind() != io::ErrorKind::NotFound => Err(io_at("can't remove", path, &err)),
            _ => Ok(()),
        };
    }
    let mut rounds = vec![name.to_os_string()];
    let mut moved = 0u64;
    while let Some(next) = rounds.pop() {
        remove_dir(dir, dir, &next, path, 1, &mut rounds, &mut moved)?;
    }
    Ok(())
}

/// Removes the directory `name` in `parent` with everything in it; see [`remove_tree`].
fn remove_dir(
    top: &Dir,
    parent: &Dir,
    name: &OsStr,
    path: &[u8],
    depth: usize,
    rounds: &mut Vec<OsString>,
    moved: &mut u64,
) -> Result<(), Failure> {
    let sub = match parent.open_dir(name) {
        Ok(sub) => sub,
        Err(err) if err.kind() == io::ErrorKind::PermissionDenied => {
            parent.unlock(name).map_err(|err| io_at("can't remove", path, &err))?;
            parent.open_dir(name).map_err(|err| io_at("can't remove", path, &err))?
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(io_at("can't remove", path, &err)),
    };
    sub.set_mode(0o700).map_err(|err| io_at("can't remove", path, &err))?;
    for (inner, meta) in sub.list().map_err(|err| io_at("can't list", path, &err))? {
        if meta.kind != Kind::Dir {
            match sub.remove(&inner, false) {
                Err(err) if err.kind() != io::ErrorKind::NotFound => return Err(io_at("can't remove", path, &err)),
                _ => continue,
            }
        }
        if depth < REMOVE_DEPTH {
            remove_dir(top, &sub, &inner, path, depth + 1, rounds, moved)?;
            continue;
        }
        // Too deep to hold on to: up beside the tree, for the next round.
        let name = loop {
            *moved += 1;
            let candidate = OsString::from(format!(".nixxa-remove-{}-{moved}", std::process::id()));
            if !top.has(&candidate).map_err(|err| io_at("can't remove", path, &err))? {
                break candidate;
            }
        };
        sub.rename_into(&inner, top, &name).map_err(|err| io_at("can't remove", path, &err))?;
        rounds.push(name);
    }
    match parent.remove(name, true) {
        Err(err) if err.kind() != io::ErrorKind::NotFound => Err(io_at("can't remove", path, &err)),
        _ => Ok(()),
    }
}

/// Calls `found` with every file's hash in `tree`.
fn each_file(tree: &Tree, found: &mut impl FnMut(&Hash)) {
    for node in tree.values() {
        match node {
            Node::File { hash, .. } => found(hash),
            Node::Dir { tree, .. } => each_file(tree, found),
            _ => {}
        }
    }
}

/// Calls `change` for every path that differs between `before` and `after`, with what was
/// there on each side; a directory added or removed comes with everything in it.
fn compare(before: &Tree, after: &Tree, rel: &[u8], change: &mut impl FnMut(&[u8], Option<&Node>, Option<&Node>)) {
    let names: BTreeSet<&OsString> = before.keys().chain(after.keys()).collect();
    for name in names {
        let (b, a) = (before.get(name), after.get(name));
        if b == a {
            continue;
        }
        let path = child(rel, name);
        match (b, a) {
            (Some(Node::Dir { mode: m0, tree: t0 }), Some(Node::Dir { mode: m1, tree: t1 })) => {
                if m0 != m1 {
                    change(&path, b, a);
                }
                compare(t0, t1, &path, change);
            }
            _ => {
                change(&path, b, a);
                let empty = Tree::new();
                match (b, a) {
                    (Some(Node::Dir { tree, .. }), _) => compare(tree, &empty, &path, change),
                    (_, Some(Node::Dir { tree, .. })) => compare(&empty, tree, &path, change),
                    _ => {}
                }
            }
        }
    }
}

impl Walk<'_> {
    fn tree(
        &mut self,
        dir: &Dir,
        rel: &[u8],
        previous: Option<&Arc<Tree>>,
        depth: usize,
    ) -> Result<Arc<Tree>, Failure> {
        let mut tree = Tree::new();
        let entries = dir.list().map_err(|err| io_at("can't list", rel, &err))?;
        for (name, meta) in entries {
            let path = child(rel, &name);
            let node = match meta.kind {
                Kind::Dir if excluded(&name) => {
                    self.not_covered.insert(format!("{}/", shown(&path)));
                    Node::Skipped { mode: meta.mode }
                }
                Kind::Dir => {
                    if self.store_id == Some((meta.key.dev, meta.key.ino)) {
                        continue;
                    }
                    if depth + 1 >= MAX_DEPTH {
                        if self.keep == Keep::Peek {
                            tree.insert(name, Node::Unreadable { kind: Kind::Dir, mode: meta.mode, size: 0 });
                            continue;
                        }
                        return Err(Failure::Io(format!(
                            "the workspace is nested more than {MAX_DEPTH} directories deep at `{}`, so no checkpoint \
                             can be taken until that tree is gone",
                            shown(&path)
                        )));
                    }
                    let sub = match dir.open_dir(&name) {
                        Ok(sub) => sub,
                        // Gone or replaced since it was listed.
                        Err(err) if vanished(&err) => continue,
                        Err(err) if self.unreadable(&err) => {
                            tree.insert(name, Node::Unreadable { kind: Kind::Dir, mode: meta.mode, size: 0 });
                            continue;
                        }
                        Err(err) => return Err(io_at("can't open", &path, &err)),
                    };
                    let before = previous.and_then(|p| match p.get(&name) {
                        Some(Node::Dir { tree, .. }) => Some(tree.clone()),
                        _ => None,
                    });
                    Node::Dir { mode: meta.mode, tree: self.tree(&sub, &path, before.as_ref(), depth + 1)? }
                }
                Kind::File => match self.file(dir, &name, meta, &path)? {
                    Some(node) => node,
                    None => continue,
                },
                Kind::Symlink => match dir.read_link(&name) {
                    Ok(target) => Node::Symlink { target },
                    Err(err) if vanished(&err) => continue,
                    Err(err) => return Err(io_at("can't read the link", &path, &err)),
                },
                Kind::Other => {
                    self.not_covered.insert(format!("{} (a special file)", shown(&path)));
                    continue;
                }
            };
            tree.insert(name, node);
        }
        // Share an unchanged directory with the checkpoint before.
        match previous {
            Some(previous) if **previous == tree => Ok(previous.clone()),
            _ => Ok(Arc::new(tree)),
        }
    }

    /// A file's node: from what the last snapshot learnt when its metadata is the same, else
    /// read (and stored, for a checkpoint). `None` when it vanished.
    fn file(&mut self, dir: &Dir, name: &OsStr, meta: Meta, path: &[u8]) -> Result<Option<Node>, Failure> {
        if let Some((key, hash)) = self.store.known.get(path).copied() {
            let stored = self.keep == Keep::Peek || self.store.blobs.contains(&hash);
            if key == meta.key && stored && !racy(&meta.key, self.started) {
                self.known.insert(path.to_vec(), (key, hash));
                return Ok(Some(Node::File { hash, size: meta.size, mode: meta.mode }));
            }
        }
        // The cap is checked before anything is read: a sparse file costs a command nothing,
        // and its every byte would otherwise be copied before being refused.
        let keep = self.keep;
        let too_big = |size: u64, store: &Store| match keep {
            Keep::Store => store.used.saturating_add(size) > store.cap,
            Keep::Peek => size > store.cap,
        };
        if too_big(meta.size, self.store) {
            return match self.keep {
                Keep::Store => Err(self.full(path)),
                Keep::Peek => Ok(Some(Node::Unreadable { kind: Kind::File, mode: meta.mode, size: meta.size })),
            };
        }
        let (mut file, opened) = match dir.open_file(name) {
            Ok(open) => open,
            Err(err) if vanished(&err) => return Ok(None),
            Err(err) if self.unreadable(&err) => {
                return Ok(Some(Node::Unreadable { kind: Kind::File, mode: meta.mode, size: meta.size }));
            }
            Err(err) => return Err(io_at("can't read", path, &err)),
        };
        let mut hasher = <sha2::Sha256 as sha2::Digest>::new();
        let mut temp = match self.keep {
            Keep::Store => Some(self.temp_blob().map_err(|err| io_at("the store can't take", path, &err))?),
            Keep::Peek => None,
        };
        let mut kept = Vec::new();
        let mut size = 0u64;
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = file.read(&mut buf).map_err(|err| io_at("can't read", path, &err))?;
            if n == 0 {
                break;
            }
            size += n as u64;
            // It may grow while it is read.
            if too_big(size, self.store) {
                if let Some((temp_path, out)) = temp.take() {
                    drop(out);
                    let _ = std::fs::remove_file(temp_path);
                }
                return match self.keep {
                    Keep::Store => Err(self.full(path)),
                    Keep::Peek => Ok(Some(Node::Unreadable { kind: Kind::File, mode: opened.mode, size })),
                };
            }
            sha2::Digest::update(&mut hasher, &buf[..n]);
            if let Some((_, out)) = temp.as_mut() {
                out.write_all(&buf[..n]).map_err(|err| io_at("the store can't take", path, &err))?;
            }
            if self.keep == Keep::Peek && size <= PEEK_CAP {
                kept.extend_from_slice(&buf[..n]);
            }
        }
        let hash: Hash = sha2::Digest::finalize(hasher).into();
        if let Some((temp_path, out)) = temp {
            drop(out);
            self.keep_blob(&temp_path, hash, size, path)?;
        }
        if self.keep == Keep::Peek && size <= PEEK_CAP {
            self.peeked.insert(hash, kept);
        }
        if !racy(&opened.key, self.started) {
            self.known.insert(path.to_vec(), (opened.key, hash));
        }
        Ok(Some(Node::File { hash, size, mode: opened.mode }))
    }

    fn full(&self, path: &[u8]) -> Failure {
        Failure::Cap(format!(
            "the checkpoint store for this session is full ({} MiB): `{}` and what came before it don't fit",
            self.store.cap / (1024 * 1024),
            shown(path)
        ))
    }

    /// Whether `err` means an entry can't be read, and this snapshot only looks (a checkpoint
    /// must read everything, or fail).
    fn unreadable(&self, err: &io::Error) -> bool {
        self.keep == Keep::Peek && err.kind() == io::ErrorKind::PermissionDenied
    }

    fn temp_blob(&mut self) -> io::Result<(PathBuf, std::fs::File)> {
        self.store.temp += 1;
        let path = self.store.dir.join("tmp").join(self.store.temp.to_string());
        let file = std::fs::OpenOptions::new().write(true).create_new(true).open(&path)?;
        Ok((path, file))
    }

    /// Moves a read file's contents into the store, unless they are there already.
    fn keep_blob(&mut self, temp: &Path, hash: Hash, size: u64, path: &[u8]) -> Result<(), Failure> {
        if self.store.blobs.contains(&hash) {
            let _ = std::fs::remove_file(temp);
            return Ok(());
        }
        if self.store.used + size > self.store.cap {
            let _ = std::fs::remove_file(temp);
            return Err(self.full(path));
        }
        let blob = self.store.blob_path(&hash);
        let stored = blob.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| std::fs::rename(temp, &blob));
        if let Err(err) = stored {
            let _ = std::fs::remove_file(temp);
            return Err(io_at("the store can't take", path, &err));
        }
        self.store.blobs.insert(hash);
        self.store.used += size;
        self.added.push((hash, size));
        Ok(())
    }
}

/// An entry that disappeared or changed kind between being listed and being opened.
fn vanished(err: &io::Error) -> bool {
    #[cfg(unix)]
    {
        use rustix::io::Errno;
        let raw = err.raw_os_error();
        if raw == Some(Errno::LOOP.raw_os_error()) || raw == Some(Errno::NOTDIR.raw_os_error()) {
            return true;
        }
    }
    matches!(err.kind(), io::ErrorKind::NotFound | io::ErrorKind::InvalidInput)
}

/// Whether a file changed so recently that equal metadata doesn't prove equal contents.
fn racy(key: &Key, now: SystemTime) -> bool {
    let now = now.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos() as i128);
    let window = RACY.as_nanos() as i128;
    now - key.mtime < window || now - key.ctime < window
}

// ---- The backends' handle ----

/// The checkpoints of one sandbox's workspace. Work runs on a blocking thread, one operation
/// at a time. Dropping it removes the store.
#[derive(Debug)]
pub(crate) struct Checkpoints {
    store: Arc<Mutex<Option<Store>>>,
    dir: PathBuf,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store").field("dir", &self.dir).field("taken", &self.taken.len()).finish_non_exhaustive()
    }
}

impl Drop for Checkpoints {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Checkpoints {
    /// A new, empty store for `workspace` (a canonical host directory).
    pub(crate) fn new(workspace: &Path, config: &CheckpointConfig) -> Result<Self, SandboxError> {
        let unavailable =
            |err: io::Error| SandboxError::Unavailable(format!("the checkpoint store {}: {err}", config.dir.display()));
        if let Some(parent) = config.dir.parent() {
            std::fs::create_dir_all(parent).map_err(unavailable)?;
        }
        sys::make_private_dir(&config.dir).map_err(unavailable)?;
        std::fs::create_dir(config.dir.join("objects")).map_err(unavailable)?;
        std::fs::create_dir(config.dir.join("tmp")).map_err(unavailable)?;
        let tag = config.dir.file_name().map_or_else(|| "cp".into(), |name| name.to_string_lossy().into_owned());
        let store = Store {
            workspace: workspace.to_path_buf(),
            dir: config.dir.clone(),
            tag,
            cap: config.cap,
            used: 0,
            blobs: HashSet::new(),
            taken: Vec::new(),
            known: HashMap::new(),
            temp: 0,
        };
        store.alive();
        Ok(Self { store: Arc::new(Mutex::new(Some(store))), dir: config.dir.clone() })
    }

    async fn run<T: Send + 'static>(
        &self,
        work: impl FnOnce(&mut Store) -> Result<T, SandboxError> + Send + 'static,
    ) -> Result<T, SandboxError> {
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || {
            let mut guard = store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            match guard.as_mut() {
                Some(store) => work(store),
                None => Err(SandboxError::Unavailable("the checkpoint store is closed".into())),
            }
        })
        .await
        .map_err(|err| SandboxError::Other(format!("the checkpoint store failed: {err}")))?
    }

    pub(crate) async fn checkpoint(&self) -> Result<CheckpointId, SandboxError> {
        self.run(|store| {
            store.alive();
            store.checkpoint().map_err(SandboxError::from)
        })
        .await
    }

    pub(crate) async fn restore(&self, id: &CheckpointId) -> Result<WorkspaceDiff, SandboxError> {
        let id = id.clone();
        self.run(move |store| store.restore(&id)).await
    }

    pub(crate) async fn diff(
        &self,
        from: &CheckpointId,
        to: Option<&CheckpointId>,
    ) -> Result<WorkspaceDiff, SandboxError> {
        let (from, to) = (from.clone(), to.cloned());
        self.run(move |store| {
            let before = store.find(&from)?.clone();
            match to {
                Some(to) => {
                    let after = store.find(&to)?.clone();
                    Ok(store.diff(&before, &after, &HashMap::new()))
                }
                None => {
                    let (now, peeked) = store.snapshot(Keep::Peek)?;
                    Ok(store.diff(&before, &now, &peeked))
                }
            }
        })
        .await
    }
}

#[cfg(test)]
mod tests;
