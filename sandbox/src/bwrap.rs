//! The Linux backend: `nixxa-sandboxd`, run out of process inside bubblewrap.
//!
//! [`BwrapSandbox`] starts the daemon on first use and talks to it over the framed protocol in
//! [`crate::protocol`]. The daemon runs in fresh user, PID, network, IPC, UTS and cgroup
//! namespaces with every capability dropped, a clean environment, a read-only `/usr`, a private
//! `/tmp`, `/proc` and `/dev`, and the workspace bound read-write at `/workspace`. Nothing else
//! of the host is mounted: no home directory, `/run/user`, D-Bus, keyring, X11 or Wayland
//! sockets, `/sys` or host `/tmp`.
//!
//! When the user's systemd instance can create scopes (cgroup v2 delegation), the whole sandbox
//! runs in a transient scope (`systemd-run --user --scope`) with `MemoryMax`, `MemorySwapMax=0`
//! and `TasksMax`. Otherwise only the per-process rlimits apply, and the backend name says so.
//!
//! Requests are handled one at a time. Dropping an `execute` future cancels its command: the
//! next user of the connection (a task spawned on drop, or the next call) tells the daemon to
//! kill it and waits for its answer. If the daemon dies, the call fails with
//! [`SandboxError::Unavailable`] and the next call starts a new one.
//!
//! Not yet isolated in the D-008 sense: Landlock and seccomp come next, so
//! [`SandboxCapabilities::isolated`] is `false`.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

use nixxa_contracts::{
    ActionId, Observation, Outcome, Sandbox, SandboxAction, SandboxActionKind, SandboxCapabilities, SandboxError,
};

use crate::protocol::{FromDaemon, Limits, Op, Reply, Rlimits, ToDaemon, VERSION, read_frame, write_frame};

/// Where the daemon binary is mounted inside the sandbox.
const DAEMON_INSIDE: &str = "/.nixxa/nixxa-sandboxd";
/// The few `/etc` entries commands need (user names, the dynamic linker cache, the time zone,
/// Debian-style `alternatives` symlinks). Bound read-only if they exist.
const ETC: [&str; 7] = ["passwd", "group", "nsswitch.conf", "ld.so.cache", "ld.so.conf", "localtime", "alternatives"];
/// How long the daemon gets to start and answer the handshake.
const START_TIMEOUT: Duration = Duration::from_secs(15);
/// Extra time the client allows past a command's own limit before giving up on the daemon.
const GRACE: Duration = Duration::from_secs(15);
/// How long a non-`Exec` request may take.
const QUICK_TIMEOUT: Duration = Duration::from_secs(30);
/// Daemon log lines kept to explain a failed start.
const KEPT_LOG_LINES: usize = 4;

/// Settings for [`BwrapSandbox`]. The harness fills them in; this module reads no config.
#[derive(Debug, Clone)]
pub struct BwrapConfig {
    /// The host directory mounted read-write at `/workspace`. Must exist.
    pub workspace: PathBuf,
    /// The `nixxa-sandboxd` binary on the host (it is mounted read-only into the sandbox).
    pub daemon: PathBuf,
    /// The `bwrap` binary: a path, or a name looked up in `search_path`.
    pub bwrap: PathBuf,
    /// Directories to look for `bwrap` and `systemd-run` in (the harness passes `PATH`).
    pub search_path: Vec<PathBuf>,
    /// `XDG_RUNTIME_DIR`, which `systemd-run --user` needs to reach the user's systemd. Only
    /// `systemd-run` sees it; the sandbox's environment is cleared.
    pub runtime_dir: Option<PathBuf>,
    /// Whether to run the sandbox in a cgroup v2 scope when the user's systemd allows it.
    pub cgroup: bool,
    /// Time limit for a command that asks for none.
    pub default_timeout: Duration,
    /// The most time any command gets.
    pub max_timeout: Duration,
    /// Bytes kept from each of stdout and stderr.
    pub output_cap: u64,
    /// The largest file `ReadFile` returns, and the largest content `WriteFile` accepts.
    ///
    /// This must stay comfortably below `MAX_FRAME` (8 MiB): the file content is JSON-encoded
    /// and wrapped in a frame, and worst-case escaping can expand it by up to about 6×, so the
    /// safe ceiling is roughly 1.3 MiB. The default of 1 MiB leaves ample headroom.
    pub file_cap: u64,
    /// `MemoryMax` of the scope (the whole sandbox), in bytes.
    pub memory_max: u64,
    /// `TasksMax` of the scope, and `RLIMIT_NPROC` of every command.
    pub pids_max: u64,
    /// `RLIMIT_DATA` of every command, in bytes.
    pub data_max: u64,
    /// `RLIMIT_FSIZE` of every command, in bytes.
    pub file_size_max: u64,
    /// `RLIMIT_NOFILE` of every command.
    pub open_files_max: u64,
}

impl BwrapConfig {
    /// Defaults for a workspace and daemon binary: 120 s per command (600 s at most), 64 KiB per
    /// output stream, 1 MiB files, 2 GiB of memory, 512 processes.
    pub fn new(workspace: impl Into<PathBuf>, daemon: impl Into<PathBuf>) -> Self {
        Self {
            workspace: workspace.into(),
            daemon: daemon.into(),
            bwrap: "bwrap".into(),
            search_path: vec!["/usr/local/bin".into(), "/usr/bin".into(), "/bin".into()],
            runtime_dir: None,
            cgroup: true,
            default_timeout: Duration::from_secs(120),
            max_timeout: Duration::from_secs(600),
            output_cap: 64 * 1024,
            file_cap: 1024 * 1024,
            memory_max: 2 * 1024 * 1024 * 1024,
            pids_max: 512,
            data_max: 4 * 1024 * 1024 * 1024,
            file_size_max: 1024 * 1024 * 1024,
            open_files_max: 1024,
        }
    }

    fn limits(&self) -> Limits {
        Limits {
            default_timeout_ms: self.default_timeout.as_millis() as u64,
            max_timeout_ms: self.max_timeout.as_millis() as u64,
            output_cap: self.output_cap,
            read_cap: self.file_cap,
            write_cap: self.file_cap,
            rlimits: Rlimits {
                nproc: self.pids_max,
                fsize: self.file_size_max,
                nofile: self.open_files_max,
                cpu_secs: self.max_timeout.as_secs().max(1),
                data: self.data_max,
            },
        }
    }
}

/// Runs actions in `nixxa-sandboxd` under bubblewrap.
#[derive(Debug)]
pub struct BwrapSandbox {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    workspace: PathBuf,
    daemon: PathBuf,
    bwrap: PathBuf,
    /// `systemd-run` and the runtime dir, when the sandbox gets a cgroup scope.
    scope: Option<(PathBuf, Option<PathBuf>)>,
    config: BwrapConfig,
    conn: Mutex<Option<Conn>>,
    /// The request being waited for and the pipe to the daemon, so `reset` can cancel it
    /// without waiting for the connection.
    in_flight: StdMutex<Option<(Arc<Mutex<ChildStdin>>, u64)>>,
    /// Whether the daemon reported Landlock as active (defaults to true until the
    /// first handshake says otherwise).
    landlock: AtomicBool,
    /// Whether the daemon reported seccomp as active.
    seccomp: AtomicBool,
}

#[derive(Debug)]
struct Conn {
    child: Child,
    stdin: Arc<Mutex<ChildStdin>>,
    stdout: ChildStdout,
    next_id: u64,
    /// A request whose caller went away before its reply arrived.
    stale: Option<u64>,
    /// True while a request frame is being written: dropping mid-write must drop the
    /// connection rather than try to cancel, since a half-written frame can't be recovered.
    sending: bool,
}

impl BwrapSandbox {
    /// Checks the workspace, the daemon binary and `bwrap` exist, and whether a cgroup scope is
    /// possible. The daemon starts on first use.
    pub fn new(config: BwrapConfig) -> Result<Self, SandboxError> {
        let workspace = config
            .workspace
            .canonicalize()
            .map_err(|err| SandboxError::Unavailable(format!("workspace {}: {err}", config.workspace.display())))?;
        if !workspace.is_dir() {
            return Err(SandboxError::Unavailable(format!("{} is not a directory", workspace.display())));
        }
        let daemon = config.daemon.canonicalize().map_err(|err| {
            SandboxError::Unavailable(format!("the sandbox daemon {}: {err}", config.daemon.display()))
        })?;
        let bwrap = find(&config.bwrap, &config.search_path).ok_or_else(|| {
            SandboxError::Unavailable(
                "bubblewrap (`bwrap`) is not installed; install it, e.g. `pacman -S bubblewrap`".into(),
            )
        })?;
        let scope = if config.cgroup { probe_scope(&config) } else { None };
        Ok(Self {
            inner: Arc::new(Inner {
                workspace,
                daemon,
                bwrap,
                scope,
                config,
                conn: Mutex::new(None),
                in_flight: StdMutex::new(None),
                landlock: AtomicBool::new(false),
                seccomp: AtomicBool::new(false),
            }),
        })
    }

    /// Whether the sandbox runs in a cgroup v2 scope (memory and process limits for the whole
    /// sandbox), rather than with per-process rlimits only.
    pub fn has_cgroup(&self) -> bool {
        self.inner.scope.is_some()
    }
}

/// Finds `name` in `dirs` unless it is already a path.
fn find(name: &Path, dirs: &[PathBuf]) -> Option<PathBuf> {
    if name.components().count() > 1 {
        return name.is_file().then(|| name.to_path_buf());
    }
    dirs.iter().map(|dir| dir.join(name)).find(|path| path.is_file())
}

/// Whether `systemd-run --user --scope` works here, with the limits we need. It runs
/// `/bin/true` (never anything of the agent's).
fn probe_scope(config: &BwrapConfig) -> Option<(PathBuf, Option<PathBuf>)> {
    let systemd_run = find(Path::new("systemd-run"), &config.search_path)?;
    let mut probe = std::process::Command::new(&systemd_run);
    probe
        .env_clear()
        .args(["--user", "--scope", "--quiet", "--collect"])
        .args(scope_properties(config))
        .args(["--", "/bin/true"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(dir) = &config.runtime_dir {
        probe.env("XDG_RUNTIME_DIR", dir);
    }
    let ok = probe.status().is_ok_and(|status| status.success());
    ok.then(|| (systemd_run, config.runtime_dir.clone()))
}

fn scope_properties(config: &BwrapConfig) -> Vec<String> {
    vec![
        "-p".into(),
        format!("MemoryMax={}", config.memory_max),
        "-p".into(),
        "MemorySwapMax=0".into(),
        "-p".into(),
        format!("TasksMax={}", config.pids_max),
        // Only the process the kernel picks dies on OOM, not the whole scope (systemd's default
        // `stop` would take the daemon down with it).
        "-p".into(),
        "OOMPolicy=continue".into(),
    ]
}

impl Inner {
    fn set_in_flight(&self, value: Option<(Arc<Mutex<ChildStdin>>, u64)>) {
        if let Ok(mut slot) = self.in_flight.lock() {
            *slot = value;
        }
    }

    /// The `bwrap` arguments, up to and including the daemon path.
    fn bwrap_args(&self) -> Vec<String> {
        let mut args: Vec<String> = [
            "--unshare-all",
            "--unshare-user",
            "--disable-userns",
            "--die-with-parent",
            "--new-session",
            "--cap-drop",
            "ALL",
            "--hostname",
            "nixxa",
            "--clearenv",
            "--setenv",
            "PATH",
            "/usr/local/bin:/usr/bin:/bin",
            "--setenv",
            "HOME",
            "/tmp",
            "--setenv",
            "LANG",
            "C.UTF-8",
            "--setenv",
            "TERM",
            "dumb",
            "--ro-bind",
            "/usr",
            "/usr",
        ]
        .map(str::to_owned)
        .into();
        // Merged-/usr systems have these as symlinks into /usr; older ones as directories.
        for top in ["bin", "sbin", "lib", "lib32", "lib64"] {
            let host = Path::new("/").join(top);
            if let Ok(target) = std::fs::read_link(&host) {
                args.extend(["--symlink".into(), target.to_string_lossy().into_owned(), host_str(&host)]);
            } else if host.is_dir() {
                args.extend(["--ro-bind".into(), host_str(&host), host_str(&host)]);
            }
        }
        for name in ETC {
            let path = format!("/etc/{name}");
            args.extend(["--ro-bind-try".into(), path.clone(), path]);
        }
        args.extend(
            ["--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp", "--bind"].map(str::to_owned),
        );
        args.extend([host_str(&self.workspace), "/workspace".into(), "--chdir".into(), "/workspace".into()]);
        args.extend(["--ro-bind".into(), host_str(&self.daemon), DAEMON_INSIDE.into(), DAEMON_INSIDE.into()]);
        args
    }

    /// Starts the daemon and does the handshake.
    async fn spawn(&self) -> Result<Conn, SandboxError> {
        let mut cmd = match &self.scope {
            Some((systemd_run, runtime_dir)) => {
                let mut cmd = Command::new(systemd_run);
                cmd.env_clear().args(["--user", "--scope", "--quiet", "--collect"]);
                cmd.args(scope_properties(&self.config)).arg("--").arg(&self.bwrap);
                if let Some(dir) = runtime_dir {
                    cmd.env("XDG_RUNTIME_DIR", dir);
                }
                cmd
            }
            None => {
                let mut cmd = Command::new(&self.bwrap);
                cmd.env_clear();
                cmd
            }
        };
        cmd.args(self.bwrap_args())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child =
            cmd.spawn().map_err(|err| SandboxError::Unavailable(format!("could not start bubblewrap: {err}")))?;
        let (Some(mut stdin), Some(mut stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            return Err(SandboxError::Unavailable("bubblewrap started without pipes".into()));
        };
        let recent = Arc::new(StdMutex::new(VecDeque::new()));
        tokio::spawn(forward_logs(stderr, Arc::clone(&recent)));

        let hello = async {
            write_frame(&mut stdin, &ToDaemon::Hello { version: VERSION, limits: self.config.limits() }).await?;
            read_frame::<_, FromDaemon>(&mut stdout).await
        };
        let failure = |why: String| {
            let logs: Vec<String> = recent.lock().map(|l| l.iter().cloned().collect()).unwrap_or_default();
            let logs = if logs.is_empty() { String::new() } else { format!(" ({})", logs.join("; ")) };
            SandboxError::Unavailable(format!("the sandbox did not start: {why}{logs}"))
        };
        match tokio::time::timeout(START_TIMEOUT, hello).await {
            Ok(Ok(FromDaemon::Hello { version, layers })) if version == VERSION => {
                self.landlock.store(layers.landlock, Ordering::Relaxed);
                self.seccomp.store(layers.seccomp, Ordering::Relaxed);
            }
            Ok(Ok(FromDaemon::Hello { version, .. })) => {
                return Err(failure(format!("daemon speaks version {version}")));
            }
            Ok(Ok(other)) => return Err(failure(format!("unexpected first message {other:?}"))),
            Ok(Err(err)) => {
                // Let the log forwarder catch bubblewrap's last words.
                let _ = tokio::time::timeout(Duration::from_millis(500), child.wait()).await;
                tokio::time::sleep(Duration::from_millis(20)).await;
                return Err(failure(err.to_string()));
            }
            Err(_) => return Err(failure("no answer to the handshake".into())),
        }
        Ok(Conn { child, stdin: Arc::new(Mutex::new(stdin)), stdout, next_id: 1, stale: None, sending: false })
    }

    /// The live connection, started (or restarted) if needed.
    async fn connect<'a>(&self, slot: &'a mut Option<Conn>) -> Result<&'a mut Conn, SandboxError> {
        if let Some(conn) = slot {
            // A connection that is mid-write, or whose child has exited, can't be reused.
            if conn.sending || !matches!(conn.child.try_wait(), Ok(None)) {
                *slot = None;
            }
        }
        if slot.is_none() {
            *slot = Some(self.spawn().await?);
        }
        let conn = slot.as_mut().expect("just set");
        if let Err(err) = conn.settle().await {
            *slot = None;
            return Err(err);
        }
        Ok(slot.as_mut().expect("still set"))
    }

    /// Sends `op` and waits for its reply. On any failure the connection is dropped (killing the
    /// daemon), so the next call starts a fresh one.
    async fn request(&self, slot: &mut Option<Conn>, op: Op, limit: Duration) -> Result<Reply, SandboxError> {
        let conn = self.connect(slot).await?;
        let id = conn.next_id;
        conn.next_id += 1;
        conn.sending = true;
        if let Err(err) = conn.send(&ToDaemon::Request { id, op }).await {
            conn.sending = false;
            conn.stale = None;
            *slot = None;
            return Err(err);
        }
        conn.sending = false;
        conn.stale = Some(id);
        self.set_in_flight(Some((Arc::clone(&conn.stdin), id)));
        let reply = conn.await_reply(id, limit).await;
        self.set_in_flight(None);
        match reply {
            Ok(reply) => {
                conn.stale = None;
                Ok(reply)
            }
            Err(err) => {
                self.set_in_flight(None);
                *slot = None;
                Err(err)
            }
        }
    }
}

impl Conn {
    async fn send(&self, msg: &ToDaemon) -> Result<(), SandboxError> {
        write_frame(&mut *self.stdin.lock().await, msg).await.map_err(gone)
    }

    async fn await_reply(&mut self, id: u64, limit: Duration) -> Result<Reply, SandboxError> {
        let deadline = Instant::now() + limit;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match tokio::time::timeout(left, read_frame::<_, FromDaemon>(&mut self.stdout)).await {
                Ok(Ok(FromDaemon::Reply { id: got, reply })) if got == id => return Ok(reply),
                // A late reply to something already given up on.
                Ok(Ok(_)) => continue,
                Ok(Err(err)) => return Err(gone(err)),
                Err(_) => return Err(SandboxError::Unavailable("the sandbox daemon stopped answering".into())),
            }
        }
    }

    /// If a caller went away mid-request, cancels that request and waits for its reply.
    async fn settle(&mut self) -> Result<(), SandboxError> {
        if let Some(id) = self.stale {
            self.send(&ToDaemon::Cancel { id }).await?;
            self.await_reply(id, GRACE).await?;
            self.stale = None;
        }
        Ok(())
    }
}

fn gone(err: impl std::fmt::Display) -> SandboxError {
    SandboxError::Unavailable(format!("the sandbox daemon stopped: {err}"))
}

fn host_str(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Forwards the daemon's (and bubblewrap's) stderr to `tracing`, keeping the last few lines to
/// explain a failed start. The daemon only logs metadata.
async fn forward_logs(stderr: tokio::process::ChildStderr, recent: Arc<StdMutex<VecDeque<String>>>) {
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let line: String = line.chars().filter(|c| !c.is_control()).take(300).collect();
        tracing::debug!(target: "nixxa_sandboxd", "{line}");
        if let Ok(mut recent) = recent.lock() {
            if recent.len() == KEPT_LOG_LINES {
                recent.pop_front();
            }
            recent.push_back(line);
        }
    }
}

/// Cancels the in-flight request if the `execute` future is dropped before it finishes.
struct CancelOnDrop {
    inner: Option<Arc<Inner>>,
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        let Some(inner) = self.inner.take() else { return };
        // Clear the stale in-flight id so a later `reset()` doesn't send a pointless Cancel.
        inner.set_in_flight(None);
        // Without a runtime the next call settles it instead.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let mut slot = inner.conn.lock().await;
                if let Some(conn) = slot.as_mut() {
                    if conn.sending {
                        // A frame was only partly written: the connection can't be resynchronised.
                        *slot = None;
                        return;
                    }
                    if conn.settle().await.is_err() {
                        *slot = None;
                    }
                }
            });
        }
    }
}

#[async_trait]
impl Sandbox for BwrapSandbox {
    fn capabilities(&self) -> SandboxCapabilities {
        let landlock = self.inner.landlock.load(Ordering::Relaxed);
        let seccomp = self.inner.seccomp.load(Ordering::Relaxed);
        let mut backend = if self.has_cgroup() {
            "bubblewrap".to_string()
        } else {
            "bubblewrap (rlimits only, no cgroup)".to_string()
        };
        if !landlock {
            backend.push_str(" (no Landlock)");
        }
        if !seccomp {
            backend.push_str(" (no seccomp)");
        }
        SandboxCapabilities {
            backend,
            // Isolated only when namespaces, Landlock and seccomp are all active.
            isolated: landlock && seccomp,
            network: false,
            actions: SandboxActionKind::ALL.to_vec(),
        }
    }

    async fn execute(&self, id: ActionId, action: SandboxAction) -> Result<Observation, SandboxError> {
        let started = Instant::now();
        let limit = match &action {
            SandboxAction::Exec { timeout_ms, .. } => {
                let asked = timeout_ms.map_or(self.inner.config.default_timeout, Duration::from_millis);
                asked.min(self.inner.config.max_timeout) + GRACE
            }
            SandboxAction::WriteFile { content, .. } if content.len() as u64 > self.inner.config.file_cap => {
                let message = format!(
                    "the content is too large to write ({} bytes, limit {})",
                    content.len(),
                    self.inner.config.file_cap
                );
                return Ok(Observation::failed(id, message));
            }
            _ => QUICK_TIMEOUT,
        };
        let mut slot = self.inner.conn.lock().await;
        let mut guard = CancelOnDrop { inner: Some(Arc::clone(&self.inner)) };
        let reply = self.inner.request(&mut slot, Op::Action { action }, limit).await;
        guard.inner = None;
        let duration_ms = started.elapsed().as_millis() as u64;
        match reply? {
            Reply::Outcome { outcome, truncated } => Ok(Observation { action_id: id, outcome, duration_ms, truncated }),
            Reply::Timeout => Err(SandboxError::Timeout),
            // Stopped by a concurrent `reset`.
            Reply::Cancelled => {
                Ok(Observation { duration_ms, ..Observation::failed(id, "the command was stopped by a reset") })
            }
            Reply::Done => Ok(Observation::new(id, Outcome::Failed { message: "no result".into() })),
            Reply::Error { message } => Err(SandboxError::Unavailable(message)),
        }
    }

    async fn health(&self) -> Result<(), SandboxError> {
        let mut slot = self.inner.conn.lock().await;
        match self.inner.request(&mut slot, Op::Ping, QUICK_TIMEOUT).await? {
            Reply::Done => Ok(()),
            other => Err(SandboxError::Unavailable(format!("unexpected answer to a ping: {other:?}"))),
        }
    }

    async fn reset(&self) -> Result<(), SandboxError> {
        // A command may be running under the connection lock: ask the daemon to stop it first.
        let running = self.inner.in_flight.lock().ok().and_then(|slot| slot.clone());
        if let Some((stdin, id)) = running {
            let _ = write_frame(&mut *stdin.lock().await, &ToDaemon::Cancel { id }).await;
        }
        let mut slot = self.inner.conn.lock().await;
        if slot.is_none() {
            return Ok(()); // Nothing is running.
        }
        // If the daemon is gone or fails, dropping it has killed everything anyway.
        let _ = self.inner.request(&mut slot, Op::Reset, QUICK_TIMEOUT).await;
        Ok(())
    }
}
