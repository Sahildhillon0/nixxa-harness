//! A backend that works directly in a directory on the host, with **no isolation**.
//!
//! The user picks it at start-up ("work in this directory", D-039): reads, writes and listings
//! touch the real files under the root, and commands run in a real shell with the root as their
//! working directory. Paths still can't leave the root (`..`, absolute paths and symlinks that
//! point outside are refused), but a command can do anything the user can, so the controller's
//! approval prompt is the only guard. [`SandboxCapabilities::isolated`] is `false`, and the TUI
//! says so.
//!
//! Background jobs are real host processes with the same limits as in the sandbox (how many run
//! at once, a time limit, the last part of their output). On Unix each job is its own process
//! group, and ending it kills the group; a process that leaves the group (`setsid`) escapes,
//! which is one more way this mode isn't isolated. On Windows only the shell itself is killed.
//!
//! Patches replace the file at once (a new file beside it, renamed over it, with its mode kept),
//! and never through a symbolic link. Checkpoints (undo) live in a store on the host (see
//! `checkpoint/`), outside the directory.
//!
//! The user's terminal (`ctrl+t`, Unix only) is the user's own shell in the root, on a PTY; see
//! `host_terminal.rs`. On Windows there is none yet: it needs ConPTY (Stage 16), and
//! [`Sandbox::open_terminal`] says so.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::stream::BoxStream;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};
use tokio::sync::broadcast;

use nixxa_contracts::{
    ActionId, CheckpointId, DirEntry, Edit, JobId, NetworkAccess, Observation, OpenTerminal, Outcome, Sandbox,
    SandboxAction, SandboxActionKind, SandboxCapabilities, SandboxError, SandboxEvent, TerminalSize, WorkspaceDiff,
};

use crate::JobLimits;
use crate::checkpoint::{CheckpointConfig, Checkpoints};
use crate::job::{End, Handles, Job, Table, Tail, no_such_job};

/// The time limit when the action asks for none.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);
/// The most time any one command gets, whatever it asks for.
const MAX_TIMEOUT: Duration = Duration::from_secs(600);
/// How much of each of stdout and stderr is kept.
const OUTPUT_CAP: usize = 64 * 1024;
/// The largest file `ReadFile` returns.
const READ_CAP: u64 = 1024 * 1024;
/// How long a job's finished shell's pipes may stay open before its leftovers are killed.
const LINGER: Duration = Duration::from_millis(250);
/// How long a job's output readers get once it has been killed.
const DRAIN: Duration = Duration::from_secs(2);

/// Settings for [`HostSandbox`]. The harness fills them in; this module reads no config.
#[derive(Debug, Clone)]
pub struct HostConfig {
    /// The directory to work in. Every path is relative to it.
    pub root: PathBuf,
    /// Environment variables commands and the terminal must not see (secrets the harness
    /// itself reads).
    pub hidden_env: Vec<String>,
    /// Limits on background jobs.
    pub jobs: JobLimits,
    /// The user's shell for the terminal (the harness passes `$SHELL`); `/bin/sh` without one.
    pub shell: Option<PathBuf>,
    /// Where checkpoints (undo) are kept; `None` for none.
    pub checkpoints: Option<CheckpointConfig>,
}

/// Works on the real files under one host directory. Not isolated.
#[derive(Debug)]
pub struct HostSandbox {
    /// Canonical, so containment checks compare like with like.
    root: PathBuf,
    hidden_env: Vec<String>,
    job_limits: JobLimits,
    jobs: Mutex<Table>,
    /// The shell pid of each running job (its process group on Unix), so dropping the sandbox
    /// can kill them at once, even when the runtime is shutting down and no task runs again.
    running: Groups,
    events: broadcast::Sender<SandboxEvent>,
    /// The shell the terminal runs.
    #[cfg_attr(not(unix), allow(dead_code))]
    shell: Option<PathBuf>,
    /// The open terminal, if any.
    #[cfg(unix)]
    terminal: crate::host_terminal::Slot,
    checkpoints: Option<Checkpoints>,
    /// Numbers temporary files.
    temp: std::sync::atomic::AtomicU64,
}

/// Shell pids of the running jobs, by job.
type Groups = Arc<Mutex<HashMap<JobId, u32>>>;

impl Drop for HostSandbox {
    /// The session is over: its jobs end with it. The groups are killed here, synchronously:
    /// when the app quits, the runtime drops the jobs' tasks without running them again, and
    /// `kill_on_drop` alone would kill only each shell, not what it started.
    fn drop(&mut self) {
        if let Ok(jobs) = self.jobs.lock() {
            jobs.stop_all();
        }
        let pids: Vec<u32> = self.running.lock().map(|running| running.values().copied().collect()).unwrap_or_default();
        pids.into_iter().for_each(kill_group);
        #[cfg(unix)]
        crate::host_terminal::end_now(&self.terminal);
    }
}

impl HostSandbox {
    /// A backend rooted at `config.root`, which must be an existing directory.
    pub fn new(config: HostConfig) -> Result<Self, SandboxError> {
        let root = config
            .root
            .canonicalize()
            .map_err(|err| SandboxError::Unavailable(format!("{}: {err}", config.root.display())))?;
        if !root.is_dir() {
            return Err(SandboxError::Unavailable(format!("{} is not a directory", root.display())));
        }
        let jobs = Table::new(config.jobs.max_running as usize, config.jobs.output_cap as usize, "");
        let checkpoints = config.checkpoints.as_ref().map(|c| Checkpoints::new(&root, c)).transpose()?;
        Ok(Self {
            root,
            hidden_env: config.hidden_env,
            job_limits: config.jobs,
            jobs: Mutex::new(jobs),
            running: Groups::default(),
            events: broadcast::channel(crate::events::CAPACITY).0,
            shell: config.shell,
            #[cfg(unix)]
            terminal: crate::host_terminal::Slot::default(),
            checkpoints,
            temp: std::sync::atomic::AtomicU64::new(0),
        })
    }

    /// The host path for a workspace-relative, `/`-separated path, if it stays inside the root.
    fn resolve(&self, path: &str) -> Option<PathBuf> {
        if path.starts_with('/') || path.starts_with('\\') {
            return None;
        }
        let mut resolved = self.root.clone();
        for part in path.split('/') {
            match part {
                "" | "." => {}
                ".." => return None,
                // A backslash or colon could smuggle in a parent, a drive or a stream on Windows.
                part if part.contains(['\\', ':']) => return None,
                part => resolved.push(part),
            }
        }
        self.contains(&resolved).then_some(resolved)
    }

    /// Whether `path` really is under the root once symlinks are followed: the deepest part of
    /// it that exists must resolve inside.
    fn contains(&self, path: &Path) -> bool {
        let mut probe = path;
        loop {
            match probe.canonicalize() {
                Ok(real) => return real.starts_with(&self.root),
                Err(_) => match probe.parent() {
                    Some(parent) => probe = parent,
                    None => return false,
                },
            }
        }
    }

    async fn read(&self, path: &Path) -> Outcome {
        match tokio::fs::metadata(path).await {
            Ok(meta) if meta.is_dir() => return failed("that is a directory"),
            Ok(meta) if meta.len() > READ_CAP => {
                return failed(format!("the file is too large to read ({} bytes, limit {READ_CAP})", meta.len()));
            }
            Ok(_) => {}
            Err(err) => return failed(io_words(&err)),
        }
        match tokio::fs::read(path).await {
            Ok(bytes) => {
                let hash = crate::patch::hash(&bytes);
                match String::from_utf8(bytes) {
                    Ok(content) => Outcome::File { content, hash },
                    Err(_) => failed("the file is not UTF-8 text"),
                }
            }
            Err(err) => failed(io_words(&err)),
        }
    }

    /// Applies a patch: never through a symbolic link, all edits or none, and the file replaced
    /// at once with its mode kept.
    async fn patch(&self, path: &Path, shown: &str, edits: &[Edit], expected: &str) -> Outcome {
        let before = match tokio::fs::symlink_metadata(path).await {
            Ok(meta) if meta.file_type().is_symlink() => {
                return failed("that is a symbolic link, which a patch doesn't follow: patch the file it points to");
            }
            Ok(meta) if !meta.is_file() => return failed("that is not a regular file"),
            Ok(meta) if meta.len() > READ_CAP => {
                return failed(format!("the file is too large to patch ({} bytes, limit {READ_CAP})", meta.len()));
            }
            Ok(meta) => meta,
            Err(err) => return failed(io_words(&err)),
        };
        let old = match tokio::fs::read(path).await {
            Ok(old) => old,
            Err(err) => return failed(io_words(&err)),
        };
        let new = match crate::patch::apply(&old, edits, expected) {
            Ok(new) => new,
            Err(why) => return failed(why),
        };
        let Some(parent) = path.parent() else { return failed("that is the workspace root") };
        let n = self.temp.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let temp = parent.join(format!(".nixxa-patch-{}-{n}", std::process::id()));
        let replaced = async {
            // `create_new` never opens an existing file or follows a link.
            let mut file = tokio::fs::OpenOptions::new().write(true).create_new(true).open(&temp).await?;
            tokio::io::AsyncWriteExt::write_all(&mut file, &new).await?;
            tokio::io::AsyncWriteExt::flush(&mut file).await?;
            drop(file);
            tokio::fs::set_permissions(&temp, before.permissions()).await?;
            // The file must still be the one read: not changed or swapped meanwhile.
            let now = tokio::fs::symlink_metadata(path).await?;
            if now.file_type().is_symlink()
                || now.len() != before.len()
                || now.modified().ok() != before.modified().ok()
            {
                return Err(std::io::Error::other(
                    "the file changed while it was being patched, so nothing was changed",
                ));
            }
            tokio::fs::rename(&temp, path).await
        };
        if let Err(err) = replaced.await {
            let _ = tokio::fs::remove_file(&temp).await;
            return failed(io_words(&err));
        }
        Outcome::Patched { diff: crate::patch::unified(shown, &old, &new), hash: crate::patch::hash(&new) }
    }

    fn checkpoints(&self) -> Result<&Checkpoints, SandboxError> {
        self.checkpoints.as_ref().ok_or_else(|| SandboxError::Unsupported("checkpoints are off".into()))
    }

    async fn write(&self, path: &Path, content: String) -> Outcome {
        if path == self.root {
            return failed("that is the workspace root");
        }
        if let Some(parent) = path.parent() {
            if let Err(err) = tokio::fs::create_dir_all(parent).await {
                return failed(io_words(&err));
            }
        }
        let bytes = content.len() as u64;
        match tokio::fs::write(path, content).await {
            Ok(()) => Outcome::Written { bytes },
            Err(err) => failed(io_words(&err)),
        }
    }

    async fn list(&self, path: &Path) -> Outcome {
        let mut dir = match tokio::fs::read_dir(path).await {
            Ok(dir) => dir,
            Err(err) => return failed(io_words(&err)),
        };
        let mut entries = Vec::new();
        loop {
            match dir.next_entry().await {
                Ok(Some(entry)) => {
                    // Follows symlinks, so a link to a directory lists as one.
                    let meta = tokio::fs::metadata(entry.path()).await.ok();
                    let is_dir = meta.as_ref().is_some_and(|m| m.is_dir());
                    entries.push(DirEntry {
                        name: entry.file_name().to_string_lossy().into_owned(),
                        is_dir,
                        size: meta.filter(|_| !is_dir).map(|m| m.len()),
                    });
                }
                Ok(None) => break,
                Err(err) => return failed(io_words(&err)),
            }
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Outcome::Listing { entries }
    }

    /// Starts `command` as a background job in `cwd`.
    fn start_job(&self, command: &str, cwd: &Path, timeout_ms: Option<u64>) -> Outcome {
        if !cwd.is_dir() {
            return failed("the working directory doesn't exist");
        }
        let (job, handles) =
            match self.jobs.lock().map_err(|_| "jobs unavailable".to_string()).and_then(|mut t| t.add()) {
                Ok(added) => added,
                Err(message) => return Outcome::Failed { message },
            };
        let limits = &self.job_limits;
        let limit = timeout_ms.map_or(limits.default_timeout, Duration::from_millis).min(limits.max_timeout);
        let mut shell = shell(command);
        shell.current_dir(cwd).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
        #[cfg(unix)]
        shell.process_group(0);
        for name in &self.hidden_env {
            shell.env_remove(name);
        }
        let id = job.id.clone();
        let events = self.events.clone();
        let running = Arc::clone(&self.running);
        tokio::spawn(async move {
            let event = run_job(job, handles, shell, limit, running).await;
            let _ = events.send(event);
        });
        Outcome::Started { job: id }
    }

    /// Answers `JobStatus` (`tail_bytes: None`) or `JobOutput`.
    fn answer(&self, id: &JobId, tail_bytes: Option<u64>) -> Outcome {
        match self.jobs.lock() {
            Ok(jobs) => jobs.answer(id, tail_bytes),
            Err(_) => failed("jobs unavailable"),
        }
    }

    /// Stops a job and says how it ended.
    async fn stop_job(&self, id: &JobId) -> Outcome {
        let Some(job) = self.jobs.lock().ok().and_then(|jobs| jobs.find(id)) else { return no_such_job(id) };
        job.stop();
        job.wait().await;
        Outcome::Text { content: job.status() }
    }

    async fn exec(&self, command: &str, cwd: &Path, timeout_ms: Option<u64>) -> Result<Outcome, SandboxError> {
        if !cwd.is_dir() {
            return Ok(failed("the working directory doesn't exist"));
        }
        let limit = timeout_ms.map_or(DEFAULT_TIMEOUT, Duration::from_millis).min(MAX_TIMEOUT);
        let mut shell = shell(command);
        shell
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // An interrupt or a timeout drops the future, and with it the process.
            .kill_on_drop(true);
        for name in &self.hidden_env {
            shell.env_remove(name);
        }
        let mut child = match shell.spawn() {
            Ok(child) => child,
            Err(err) => return Ok(failed(format!("couldn't start the shell: {err}"))),
        };
        let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
            return Err(SandboxError::Other("the command's output wasn't captured".into()));
        };
        let run = async { tokio::join!(capped(stdout), capped(stderr), child.wait()) };
        let (stdout, stderr, status) = tokio::time::timeout(limit, run).await.map_err(|_| SandboxError::Timeout)?;
        let status = status.map_err(|err| SandboxError::Other(format!("waiting for the command: {err}")))?;
        Ok(Outcome::Exec { exit_code: status.code(), stdout, stderr })
    }
}

/// The platform shell running `command`.
fn shell(command: &str) -> Command {
    #[cfg(windows)]
    {
        let mut shell = Command::new("cmd");
        // `cmd` parses its own command line; quoting it as one argument would change it.
        shell.arg("/C").raw_arg(command);
        shell
    }
    #[cfg(not(windows))]
    {
        let mut shell = Command::new("sh");
        shell.arg("-c").arg(command);
        shell
    }
}

/// Runs a background job to its end: the shell exits (its leftovers get [`LINGER`]), the time
/// limit passes, or it is stopped. Returns its `JobFinished`.
async fn run_job(
    job: Arc<Job>,
    handles: Handles,
    mut shell: Command,
    limit: Duration,
    running: Groups,
) -> SandboxEvent {
    let started = Instant::now();
    let Handles { mut stop, done } = handles;
    let mut child = match shell.spawn() {
        Ok(child) => child,
        Err(err) => {
            return job.finish(done, End::Failed(format!("couldn't start the shell: {err}")), started.elapsed());
        }
    };
    if let (Some(pid), Ok(mut running)) = (child.id(), running.lock()) {
        running.insert(job.id.clone(), pid);
    }
    let out = tokio::spawn(keep_last(child.stdout.take(), Arc::clone(&job.stdout)));
    let err = tokio::spawn(keep_last(child.stderr.take(), Arc::clone(&job.stderr)));
    let aborts = [out.abort_handle(), err.abort_handle()];
    let collect = async {
        let _ = (out.await, err.await);
    };
    tokio::pin!(collect);
    let end = tokio::select! {
        status = child.wait() => End::Exited(status.ok().and_then(|status| status.code())),
        _ = tokio::time::sleep(limit) => End::TimedOut,
        _ = &mut stop => End::Stopped,
    };
    let mut drained = false;
    if matches!(end, End::Exited(_)) {
        drained = tokio::time::timeout(LINGER, &mut collect).await.is_ok();
    }
    kill_job(&mut child);
    let _ = child.wait().await;
    if let Ok(mut running) = running.lock() {
        running.remove(&job.id);
    }
    if !drained && tokio::time::timeout(DRAIN, &mut collect).await.is_err() {
        aborts.iter().for_each(|task| task.abort());
    }
    job.finish(done, end, started.elapsed())
}

/// Kills a job's process group (Unix) or its shell (elsewhere).
fn kill_job(child: &mut Child) {
    if let Some(pid) = child.id() {
        kill_group(pid);
    }
    let _ = child.start_kill();
}

/// Kills the process group led by the shell `pid`. Elsewhere than Unix there is no group to
/// kill: the shell itself is killed through its `Child` (`kill_on_drop` covers a quit).
fn kill_group(pid: u32) {
    #[cfg(unix)]
    if let Some(group) = rustix::process::Pid::from_raw(pid as i32) {
        let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
    }
    #[cfg(not(unix))]
    let _ = pid;
}

/// Reads a stream to its end into `tail`.
async fn keep_last(stream: Option<impl AsyncRead + Unpin>, tail: Arc<Mutex<Tail>>) {
    let Some(mut stream) = stream else { return };
    let mut chunk = [0u8; 8192];
    loop {
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if let Ok(mut tail) = tail.lock() {
                    tail.push(&chunk[..n]);
                }
            }
        }
    }
}

/// Reads a stream to its end, keeping the first [`OUTPUT_CAP`] bytes.
async fn capped(mut stream: impl AsyncRead + Unpin) -> String {
    let mut kept = Vec::new();
    let mut dropped = 0usize;
    let mut chunk = [0u8; 8192];
    loop {
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let room = OUTPUT_CAP.saturating_sub(kept.len()).min(n);
                kept.extend_from_slice(&chunk[..room]);
                dropped += n - room;
            }
        }
    }
    if dropped > 0 {
        // Never through a token (D-074).
        let keep = crate::cut::end(&kept);
        dropped += kept.len() - keep;
        kept.truncate(keep);
    }
    let mut text = String::from_utf8_lossy(&kept).into_owned();
    if dropped > 0 {
        text.push_str(&format!("\n… [{dropped} more bytes not shown]"));
    }
    text
}

fn failed(message: impl Into<String>) -> Outcome {
    Outcome::Failed { message: message.into() }
}

fn io_words(err: &std::io::Error) -> String {
    match err.kind() {
        std::io::ErrorKind::NotFound => "no such file or directory".into(),
        std::io::ErrorKind::PermissionDenied => "permission denied".into(),
        _ => err.to_string(),
    }
}

#[async_trait]
impl Sandbox for HostSandbox {
    fn capabilities(&self) -> SandboxCapabilities {
        SandboxCapabilities {
            backend: "host".into(),
            isolated: false,
            network: NetworkAccess::Unrestricted,
            jobs: true,
            terminal: cfg!(unix),
            checkpoints: self.checkpoints.is_some(),
            actions: SandboxActionKind::ALL.to_vec(),
        }
    }

    async fn checkpoint(&self) -> Result<CheckpointId, SandboxError> {
        self.checkpoints()?.checkpoint().await
    }

    async fn restore(&self, checkpoint: &CheckpointId) -> Result<WorkspaceDiff, SandboxError> {
        self.checkpoints()?.restore(checkpoint).await
    }

    async fn diff(&self, from: &CheckpointId, to: Option<&CheckpointId>) -> Result<WorkspaceDiff, SandboxError> {
        self.checkpoints()?.diff(from, to).await
    }

    #[cfg(unix)]
    async fn open_terminal(&self, size: TerminalSize) -> Result<OpenTerminal, SandboxError> {
        let shell = crate::host_terminal::shell_or_default(self.shell.as_ref());
        crate::host_terminal::open(&self.terminal, &self.root, &shell, &self.hidden_env, size)
    }

    #[cfg(not(unix))]
    async fn open_terminal(&self, size: TerminalSize) -> Result<OpenTerminal, SandboxError> {
        let _ = size;
        Err(SandboxError::Unsupported(
            "the terminal isn't available on Windows yet: it needs ConPTY, which comes with Stage 16".into(),
        ))
    }

    fn subscribe(&self) -> BoxStream<'static, SandboxEvent> {
        crate::events::stream(self.events.subscribe())
    }

    async fn execute(&self, id: ActionId, action: SandboxAction) -> Result<Observation, SandboxError> {
        let outside = || Ok(Observation::failed(id.clone(), "path is outside the workspace"));
        let outcome = match action {
            SandboxAction::Exec { command, cwd, timeout_ms, background } => {
                let Some(dir) = self.resolve(cwd.as_deref().unwrap_or("")) else { return outside() };
                if background {
                    self.start_job(&command, &dir, timeout_ms)
                } else {
                    self.exec(&command, &dir, timeout_ms).await?
                }
            }
            SandboxAction::JobStatus { job } => self.answer(&job, None),
            SandboxAction::JobOutput { job, tail_bytes } => self.answer(&job, Some(tail_bytes)),
            SandboxAction::JobStop { job } => self.stop_job(&job).await,
            SandboxAction::ReadFile { path } => {
                let Some(path) = self.resolve(&path) else { return outside() };
                self.read(&path).await
            }
            SandboxAction::WriteFile { path, content } => {
                let Some(path) = self.resolve(&path) else { return outside() };
                self.write(&path, content).await
            }
            SandboxAction::Patch { path: shown, edits, expected_hash } => {
                let Some(path) = self.resolve(&shown) else { return outside() };
                self.patch(&path, &shown, &edits, &expected_hash).await
            }
            SandboxAction::ListDir { path } => {
                let Some(path) = self.resolve(&path) else { return outside() };
                self.list(&path).await
            }
        };
        Ok(Observation::new(id, outcome))
    }

    async fn health(&self) -> Result<(), SandboxError> {
        if self.root.is_dir() {
            Ok(())
        } else {
            Err(SandboxError::Unavailable(format!("{} is gone", self.root.display())))
        }
    }

    async fn reset(&self) -> Result<(), SandboxError> {
        let stopped = self.jobs.lock().map(|jobs| jobs.stop_all()).unwrap_or_default();
        for job in stopped {
            job.wait().await;
        }
        #[cfg(unix)]
        crate::host_terminal::end(&self.terminal, "the sandbox was reset").await;
        // A running foreground command is killed when its future is dropped.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use nixxa_contracts::conformance::sandbox::run;

    use super::*;

    /// A fresh, empty directory under the system temp dir.
    fn scratch() -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "nixxa-host-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    fn host(root: &Path) -> HostSandbox {
        let mut store = root.as_os_str().to_owned();
        store.push("-checkpoints");
        HostSandbox::new(HostConfig {
            root: root.to_path_buf(),
            hidden_env: Vec::new(),
            jobs: JobLimits::default(),
            shell: Some("/bin/sh".into()),
            checkpoints: Some(CheckpointConfig { dir: store.into(), cap: crate::checkpoint::DEFAULT_CAP }),
        })
        .expect("host sandbox")
    }

    async fn run_action(sandbox: &HostSandbox, action: SandboxAction) -> Outcome {
        sandbox.execute(ActionId::new("t"), action).await.expect("execute").outcome
    }

    #[cfg(unix)]
    fn exec(command: &str) -> SandboxAction {
        SandboxAction::Exec { command: command.into(), cwd: None, timeout_ms: Some(10_000), background: false }
    }

    #[tokio::test]
    async fn host_sandbox_conforms() {
        run(|| host(&scratch())).await;
    }

    #[test]
    fn it_says_it_is_not_isolated() {
        let caps = host(&scratch()).capabilities();
        assert_eq!(caps.backend, "host");
        assert!(!caps.isolated);
    }

    #[test]
    fn a_missing_root_is_unavailable() {
        let missing = scratch().join("not-here");
        let config = HostConfig {
            root: missing,
            hidden_env: Vec::new(),
            jobs: JobLimits::default(),
            shell: None,
            checkpoints: None,
        };
        let err = HostSandbox::new(config).unwrap_err();
        assert!(matches!(err, SandboxError::Unavailable(_)), "{err:?}");
    }

    #[tokio::test]
    async fn writes_land_on_disk_with_their_folders() {
        let root = scratch();
        let sandbox = host(&root);
        let written = run_action(
            &sandbox,
            SandboxAction::WriteFile { path: "src/deep/new.txt".into(), content: "real\n".into() },
        )
        .await;
        assert_eq!(written, Outcome::Written { bytes: 5 });
        assert_eq!(std::fs::read_to_string(root.join("src").join("deep").join("new.txt")).unwrap(), "real\n");
    }

    #[tokio::test]
    async fn existing_files_are_read_and_listed() {
        let root = scratch();
        std::fs::create_dir(root.join("sub")).unwrap();
        std::fs::write(root.join("a.txt"), "alpha").unwrap();
        let sandbox = host(&root);
        assert_eq!(
            run_action(&sandbox, SandboxAction::ReadFile { path: "./a.txt".into() }).await,
            Outcome::File { content: "alpha".into(), hash: crate::patch::hash(b"alpha") }
        );
        let Outcome::Listing { entries } = run_action(&sandbox, SandboxAction::ListDir { path: ".".into() }).await
        else {
            panic!("expected a listing")
        };
        let names: Vec<_> = entries.iter().map(|e| (e.name.as_str(), e.is_dir, e.size)).collect();
        assert_eq!(names, vec![("a.txt", false, Some(5)), ("sub", true, None)]);
    }

    #[tokio::test]
    async fn binary_and_missing_files_fail_as_observations() {
        let root = scratch();
        std::fs::write(root.join("blob.bin"), [0xff, 0xfe, 0x00]).unwrap();
        let sandbox = host(&root);
        let binary = run_action(&sandbox, SandboxAction::ReadFile { path: "blob.bin".into() }).await;
        assert_eq!(binary, failed("the file is not UTF-8 text"));
        let missing = run_action(&sandbox, SandboxAction::ReadFile { path: "nope.txt".into() }).await;
        assert_eq!(missing, failed("no such file or directory"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlink_out_of_the_root_is_refused() {
        let root = scratch();
        let outside = scratch();
        std::fs::write(outside.join("secret.txt"), "no").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        let sandbox = host(&root);
        for action in [
            SandboxAction::ReadFile { path: "link/secret.txt".into() },
            SandboxAction::WriteFile { path: "link/new.txt".into(), content: "x".into() },
            SandboxAction::ListDir { path: "link".into() },
            SandboxAction::Exec { command: "ls".into(), cwd: Some("link".into()), timeout_ms: None, background: false },
        ] {
            let outcome = run_action(&sandbox, action).await;
            assert_eq!(outcome, failed("path is outside the workspace"));
        }
        assert!(!outside.join("new.txt").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn commands_run_in_the_root_or_the_given_folder() {
        let root = scratch();
        std::fs::create_dir(root.join("sub")).unwrap();
        let sandbox = host(&root);
        let Outcome::Exec { exit_code, stdout, .. } = run_action(&sandbox, exec("pwd -P")).await else {
            panic!("expected exec")
        };
        assert_eq!(exit_code, Some(0));
        assert_eq!(stdout.trim(), sandbox.root.display().to_string());
        let in_sub = SandboxAction::Exec {
            command: "touch made-here".into(),
            cwd: Some("sub".into()),
            timeout_ms: None,
            background: false,
        };
        run_action(&sandbox, in_sub).await;
        assert!(root.join("sub").join("made-here").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn exit_codes_and_stderr_come_back() {
        let sandbox = host(&scratch());
        let outcome = run_action(&sandbox, exec("echo oops >&2; exit 3")).await;
        assert_eq!(outcome, Outcome::Exec { exit_code: Some(3), stdout: String::new(), stderr: "oops\n".into() });
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn hidden_variables_are_not_passed_on() {
        // HOME is set in any test run; the command must not see it once it is hidden (the
        // harness hides ANTHROPIC_API_KEY this way).
        let sandbox = HostSandbox::new(HostConfig {
            root: scratch(),
            hidden_env: vec!["HOME".into()],
            jobs: JobLimits::default(),
            shell: None,
            checkpoints: None,
        })
        .expect("host sandbox");
        let Outcome::Exec { stdout, .. } = run_action(&sandbox, exec("echo \"[${HOME-unset}]\"")).await else {
            panic!("expected exec")
        };
        assert_eq!(stdout.trim(), "[unset]");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_command_past_its_limit_times_out() {
        let sandbox = host(&scratch());
        let slow =
            SandboxAction::Exec { command: "sleep 5".into(), cwd: None, timeout_ms: Some(100), background: false };
        let err = sandbox.execute(ActionId::new("t"), slow).await.unwrap_err();
        assert_eq!(err, SandboxError::Timeout);
    }

    #[cfg(unix)]
    fn background(command: &str) -> SandboxAction {
        SandboxAction::Exec { command: command.into(), cwd: None, timeout_ms: None, background: true }
    }

    #[cfg(unix)]
    async fn stdout_of(sandbox: &HostSandbox, command: &str) -> String {
        let Outcome::Exec { stdout, .. } = run_action(sandbox, exec(command)).await else { panic!("expected exec") };
        stdout
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stopping_a_job_kills_its_process_group() {
        let root = scratch();
        let sandbox = host(&root);
        let Outcome::Started { job } = run_action(&sandbox, background("sleep 4242 & echo $! > child.pid; wait")).await
        else {
            panic!("expected Started")
        };
        for _ in 0..250 {
            if std::fs::read_to_string(root.join("child.pid")).is_ok_and(|pid| pid.ends_with('\n')) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let alive = "kill -0 $(cat child.pid) 2>/dev/null && echo alive || echo dead";
        assert_eq!(stdout_of(&sandbox, alive).await, "alive\n");
        let Outcome::Text { content } = run_action(&sandbox, SandboxAction::JobStop { job }).await else {
            panic!("expected text")
        };
        assert!(content.contains("was stopped"), "{content}");
        // A killed child may linger as a zombie for a moment before it is reaped.
        let mut state = String::new();
        for _ in 0..100 {
            state = stdout_of(&sandbox, alive).await;
            if state == "dead\n" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(state, "dead\n", "the job's child survived its stop");
    }

    /// What quitting does: the sandbox is dropped inside the runtime, then the runtime is
    /// dropped, so the job's task never runs again.
    #[cfg(unix)]
    #[test]
    fn jobs_end_when_the_app_quits() {
        let root = scratch();
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        runtime.block_on(async {
            let sandbox = host(&root);
            let start = background("sleep 4243 & echo $! > child.pid; wait");
            assert!(matches!(run_action(&sandbox, start).await, Outcome::Started { .. }));
            for _ in 0..250 {
                if std::fs::read_to_string(root.join("child.pid")).is_ok_and(|pid| pid.ends_with('\n')) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            drop(sandbox);
        });
        drop(runtime);
        let pid: i32 = std::fs::read_to_string(root.join("child.pid")).unwrap().trim().parse().unwrap();
        let pid = rustix::process::Pid::from_raw(pid).unwrap();
        // A killed child may linger as a zombie until its new parent reaps it.
        let gone = (0..100).any(|_| {
            let alive = rustix::process::test_kill_process(pid).is_ok()
                && !std::fs::read_to_string(format!("/proc/{}/stat", pid.as_raw_nonzero())).is_ok_and(|stat| {
                    stat.rsplit_once(')').is_some_and(|(_, rest)| rest.trim_start().starts_with('Z'))
                });
            if alive {
                std::thread::sleep(Duration::from_millis(20));
            }
            !alive
        });
        assert!(gone, "the job's child {} outlived the session", pid.as_raw_nonzero());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn jobs_do_not_see_hidden_variables() {
        let sandbox = HostSandbox::new(HostConfig {
            root: scratch(),
            hidden_env: vec!["HOME".into()],
            jobs: JobLimits::default(),
            shell: None,
            checkpoints: None,
        })
        .expect("host sandbox");
        let mut events = sandbox.subscribe();
        let Outcome::Started { job } = run_action(&sandbox, background("echo \"[${HOME-unset}]\"")).await else {
            panic!("expected Started")
        };
        let event = tokio::time::timeout(Duration::from_secs(10), futures::StreamExt::next(&mut events)).await;
        assert!(matches!(event, Ok(Some(SandboxEvent::JobFinished { exit_code: Some(0), .. }))), "{event:?}");
        let Outcome::Text { content } = run_action(&sandbox, SandboxAction::JobOutput { job, tail_bytes: 100 }).await
        else {
            panic!("expected text")
        };
        assert!(content.contains("[unset]"), "{content}");
    }

    /// Reads a terminal's events until its output contains `want` (10 s at most).
    #[cfg(unix)]
    async fn shown(open: &mut nixxa_contracts::OpenTerminal, want: &str) -> String {
        use futures::StreamExt;
        let mut seen = String::new();
        let wait = async {
            while !seen.contains(want) {
                match open.events.next().await {
                    Some(nixxa_contracts::TerminalEvent::Output(bytes)) => {
                        seen.push_str(&String::from_utf8_lossy(&bytes))
                    }
                    other => panic!("the terminal ended before {want:?}: {other:?} after {seen:?}"),
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(10), wait).await.unwrap_or_else(|_| panic!("no {want:?}"));
        seen
    }

    const SIZE: TerminalSize = TerminalSize { cols: 80, rows: 24 };

    #[cfg(unix)]
    #[tokio::test]
    async fn the_terminal_hides_the_secret_variables_and_starts_in_the_root() {
        let root = scratch();
        let sandbox = HostSandbox::new(HostConfig {
            root: root.clone(),
            hidden_env: vec!["HOME".into()],
            jobs: JobLimits::default(),
            shell: Some("/bin/sh".into()),
            checkpoints: None,
        })
        .expect("host sandbox");
        let mut open = sandbox.open_terminal(SIZE).await.expect("open");
        open.terminal
            .write(b"echo \"[${HOME-unset}]-$((1+1))\" > seen.txt; pwd -P >> seen.txt; echo done-$((2+2))\r")
            .await
            .unwrap();
        shown(&mut open, "done-4").await;
        let seen = std::fs::read_to_string(root.join("seen.txt")).unwrap();
        assert_eq!(seen, format!("[unset]-2\n{}\n", sandbox.root.display()));
    }

    /// What quitting does: the sandbox is dropped inside the runtime, then the runtime. The
    /// terminal's handle is leaked here, so only the sandbox can end it.
    #[cfg(unix)]
    #[test]
    fn the_terminal_ends_when_the_app_quits() {
        let root = scratch();
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        runtime.block_on(async {
            let sandbox = host(&root);
            let mut open = sandbox.open_terminal(SIZE).await.expect("open");
            open.terminal.write(b"echo $$ > shell.pid; echo ready-$((3+3))\r").await.unwrap();
            shown(&mut open, "ready-6").await;
            std::mem::forget(open);
            drop(sandbox);
        });
        drop(runtime);
        let pid: i32 = std::fs::read_to_string(root.join("shell.pid")).unwrap().trim().parse().unwrap();
        let pid = rustix::process::Pid::from_raw(pid).unwrap();
        let gone = (0..100).any(|_| {
            let alive = rustix::process::test_kill_process(pid).is_ok()
                && !std::fs::read_to_string(format!("/proc/{}/stat", pid.as_raw_nonzero())).is_ok_and(|stat| {
                    stat.rsplit_once(')').is_some_and(|(_, rest)| rest.trim_start().starts_with('Z'))
                });
            if alive {
                std::thread::sleep(Duration::from_millis(20));
            }
            !alive
        });
        assert!(gone, "the terminal's shell {} outlived the session", pid.as_raw_nonzero());
    }

    #[cfg(not(unix))]
    #[tokio::test]
    async fn there_is_no_terminal_on_windows_and_it_says_why() {
        let sandbox = host(&scratch());
        assert!(!sandbox.capabilities().terminal);
        let err = sandbox.open_terminal(SIZE).await.map(|_| ()).unwrap_err();
        assert!(matches!(&err, SandboxError::Unsupported(why) if why.contains("ConPTY")), "{err:?}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn long_output_is_cut() {
        let sandbox = host(&scratch());
        let Outcome::Exec { stdout, .. } = run_action(&sandbox, exec("yes 'abcd efgh' | head -c 100000")).await else {
            panic!("expected exec")
        };
        let (kept, note) = stdout.split_once("\n… [").expect("a note on what was cut");
        assert!(kept.starts_with("abcd efgh\nabcd efgh\n"));
        // The cap falls inside a line: the cut moves back to the line break (D-074).
        assert_eq!(kept.len(), OUTPUT_CAP / 10 * 10);
        assert!(kept.ends_with("abcd efgh\n"), "{}", &kept[kept.len() - 20..]);
        assert_eq!(note, format!("{} more bytes not shown]", 100_000 - kept.len()));
    }
}
