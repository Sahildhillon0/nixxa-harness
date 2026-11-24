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
//! Requests carry ids and may overlap: a writer task sends whole frames, and a reader task
//! routes each reply to its caller and each event to [`Sandbox::subscribe`]rs. The connection
//! lock is held only to start a daemon, so `reset`, the job actions and `subscribe` work while
//! a command runs. Foreground `Exec`s still run one at a time (a second one waits its turn).
//! Dropping an `execute` future cancels its command: the dropped call queues a `Cancel` for the
//! daemon, which kills the command and still answers. If the daemon dies, waiting calls fail
//! with [`SandboxError::Unavailable`], every job it was running is reported as finished, and the
//! next call starts a new daemon.
//!
//! Background jobs run in the daemon under the same isolation as foreground commands; see
//! `nixxa-sandboxd`'s `jobs.rs` for how each job's process tree is tracked and ended.
//!
//! The user's terminal (D-066) is a PTY job in the daemon. Its output arrives as frames routed
//! to its own stream (never the shared event broadcast, where a flood would make other
//! subscribers skip events); each chunk is acknowledged as the stream yields it, and the daemon
//! sends at most a window of unacknowledged output, so a slow reader makes the shell wait
//! instead of losing output or growing memory.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, Weak};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::stream::BoxStream;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdout, Command};
use tokio::sync::{Mutex, broadcast, mpsc, oneshot};

use nixxa_contracts::{
    ActionId, JobId, NetworkAccess, NetworkPolicy, Observation, OpenTerminal, Outcome, Sandbox, SandboxAction,
    SandboxActionKind, SandboxCapabilities, SandboxError, SandboxEvent, Terminal, TerminalEvent, TerminalSize,
};

use crate::JobLimits;
use crate::protocol::{FromDaemon, Limits, Op, Reply, Rlimits, ToDaemon, VERSION, encode, read_frame, write_frame};
use crate::proxy::Proxy;

/// Where the network proxy's directory is mounted inside the sandbox.
const PROXY_INSIDE: &str = "/run/nixxa";
/// The home of the user's terminal inside the sandbox; agent commands can't reach it.
const TERMINAL_HOME: &str = "/nixxa-home";
/// Where the daemon binary is mounted inside the sandbox.
const DAEMON_INSIDE: &str = "/.nixxa/nixxa-sandboxd";
/// The few `/etc` entries commands need (user names, the dynamic linker cache, the time zone,
/// Debian-style `alternatives` symlinks). Bound read-only if they exist.
const ETC: [&str; 10] = [
    "passwd",
    "group",
    "nsswitch.conf",
    "ld.so.cache",
    "ld.so.conf",
    "localtime",
    "alternatives",
    // The public CA trust store, so TLS through the network proxy can verify servers.
    "ssl",
    "ca-certificates",
    "pki",
];
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
    /// Limits on background jobs.
    pub jobs: JobLimits,
    /// Tests only: names the network proxy resolves from this table instead of asking the
    /// system resolver. The harness leaves it empty.
    #[doc(hidden)]
    pub hosts: std::collections::HashMap<String, Vec<std::net::IpAddr>>,
}

impl BwrapConfig {
    /// Defaults for a workspace and daemon binary: 120 s per command (600 s at most), 64 KiB per
    /// output stream, 1 MiB files, 2 GiB of memory, 512 processes, and [`JobLimits::default`].
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
            jobs: JobLimits::default(),
            hosts: std::collections::HashMap::new(),
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
            background_default_timeout_ms: self.jobs.default_timeout.as_millis() as u64,
            background_max_timeout_ms: self.jobs.max_timeout.as_millis() as u64,
            background_output_cap: self.jobs.output_cap,
            max_background_jobs: self.jobs.max_running,
            job_prefix: String::new(),
            network: false,
        }
    }
}

/// Runs actions in `nixxa-sandboxd` under bubblewrap.
pub struct BwrapSandbox {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for BwrapSandbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BwrapSandbox").field("workspace", &self.inner.workspace).finish_non_exhaustive()
    }
}

struct Inner {
    workspace: PathBuf,
    daemon: PathBuf,
    bwrap: PathBuf,
    /// `systemd-run` and the runtime dir, when the sandbox gets a cgroup scope.
    scope: Option<(PathBuf, Option<PathBuf>)>,
    config: BwrapConfig,
    /// The live daemon. Locked only to start or replace it, never while waiting for a reply.
    conn: Mutex<Option<Arc<Conn>>>,
    /// Held while a foreground `Exec` runs, so the next one waits its turn.
    foreground: Mutex<()>,
    /// Whether the daemon reported Landlock as active.
    landlock: AtomicBool,
    /// Whether the daemon reported seccomp as active.
    seccomp: AtomicBool,
    /// Whether a daemon has answered the handshake (so `landlock` is known).
    started: AtomicBool,
    /// Events from every daemon this sandbox starts.
    events: broadcast::Sender<SandboxEvent>,
    /// Jobs started and not yet finished, with when they started, so they can be reported as
    /// finished if their daemon dies.
    jobs: Jobs,
    /// The network proxy (D-064). Its policy is the sandbox's network policy.
    proxy: Proxy,
    /// Daemons started so far. Each one's job ids get a different prefix (`j1`, then `2-j1`, …),
    /// so a restarted daemon never reuses an id the agent already has.
    generation: AtomicU64,
    /// The last id given to a terminal.
    terminals: AtomicU64,
}

type Jobs = Arc<StdMutex<HashMap<JobId, Instant>>>;
type Pending = Arc<StdMutex<HashMap<u64, oneshot::Sender<Reply>>>>;
/// The open terminals' streams, by id.
type Terminals = Arc<StdMutex<HashMap<u64, mpsc::UnboundedSender<TerminalEvent>>>>;

/// One running daemon.
struct Conn {
    /// bubblewrap (or `systemd-run`); killed when the connection is dropped.
    child: StdMutex<Child>,
    /// Encoded frames for the writer task, which writes each one whole.
    frames: mpsc::UnboundedSender<Vec<u8>>,
    /// Callers waiting for a reply, by request id.
    pending: Pending,
    /// Where each open terminal's output goes.
    terminals: Terminals,
    next_id: AtomicU64,
    /// False once either pipe has failed.
    alive: Arc<AtomicBool>,
    tasks: [tokio::task::AbortHandle; 2],
}

impl Drop for Conn {
    fn drop(&mut self) {
        self.tasks.iter().for_each(|task| task.abort());
    }
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
        let events = broadcast::channel(crate::events::CAPACITY).0;
        // The proxy's private directory goes where the user's runtime files do.
        let base = config.runtime_dir.clone().unwrap_or_else(std::env::temp_dir);
        let proxy = Proxy::new(&base, config.hosts.clone(), events.clone());
        Ok(Self {
            inner: Arc::new(Inner {
                workspace,
                daemon,
                bwrap,
                scope,
                config,
                conn: Mutex::new(None),
                foreground: Mutex::new(()),
                landlock: AtomicBool::new(false),
                seccomp: AtomicBool::new(false),
                started: AtomicBool::new(false),
                events,
                jobs: Jobs::default(),
                proxy,
                generation: AtomicU64::new(0),
                terminals: AtomicU64::new(0),
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
        args.extend(["--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp"].map(str::to_owned));
        // The terminal's home (D-066): only the user's shell may read or write it, so the agent
        // can't plant start-up files there or read its history.
        args.extend(["--tmpfs".into(), TERMINAL_HOME.into(), "--bind".into()]);
        args.extend([host_str(&self.workspace), "/workspace".into(), "--chdir".into(), "/workspace".into()]);
        args.extend(["--ro-bind".into(), host_str(&self.daemon), DAEMON_INSIDE.into()]);
        if let Some(dir) = self.proxy.dir() {
            args.extend(["--ro-bind".into(), host_str(dir), PROXY_INSIDE.into()]);
        }
        args.push(DAEMON_INSIDE.into());
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

        let generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        let mut limits = self.config.limits();
        limits.job_prefix = if generation == 1 { String::new() } else { format!("{generation}-") };
        limits.network = self.proxy.policy() != NetworkPolicy::Off;
        if limits.network {
            self.proxy.serve().map_err(SandboxError::Unavailable)?;
        }
        let hello = async {
            write_frame(&mut stdin, &ToDaemon::Hello { version: VERSION, limits }).await?;
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
                self.started.store(true, Ordering::Relaxed);
            }
            Ok(Ok(FromDaemon::Hello { version, .. })) => {
                return Err(failure(format!("daemon speaks version {version}")));
            }
            Ok(Ok(other)) => return Err(failure(format!("unexpected first message {other:?}"))),
            Ok(Err(err)) => {
                let _ = tokio::time::timeout(Duration::from_millis(500), child.wait()).await;
                tokio::time::sleep(Duration::from_millis(20)).await;
                return Err(failure(err.to_string()));
            }
            Err(_) => return Err(failure("no answer to the handshake".into())),
        }

        let pending = Pending::default();
        let terminals = Terminals::default();
        let alive = Arc::new(AtomicBool::new(true));
        let (frames, outgoing) = mpsc::unbounded_channel();
        let writer = tokio::spawn(write_frames(stdin, outgoing, Arc::clone(&alive)));
        let reader = tokio::spawn(read_frames(
            stdout,
            Arc::clone(&pending),
            Arc::clone(&alive),
            self.events.clone(),
            Arc::clone(&self.jobs),
            Arc::clone(&terminals),
        ));
        Ok(Conn {
            child: StdMutex::new(child),
            frames,
            pending,
            terminals,
            next_id: AtomicU64::new(1),
            alive,
            tasks: [writer.abort_handle(), reader.abort_handle()],
        })
    }

    /// The live daemon, started (or restarted) if needed.
    async fn connect(&self) -> Result<Arc<Conn>, SandboxError> {
        let mut slot = self.conn.lock().await;
        if let Some(conn) = slot.as_ref().filter(|conn| conn.usable()) {
            return Ok(Arc::clone(conn));
        }
        // A dead daemon took its jobs with it.
        if slot.take().is_some() {
            report_lost(&self.jobs, &self.events);
        }
        let conn = Arc::new(self.spawn().await?);
        *slot = Some(Arc::clone(&conn));
        Ok(conn)
    }

    /// The live daemon, if there is one; never starts one.
    async fn current(&self) -> Option<Arc<Conn>> {
        self.conn.lock().await.as_ref().filter(|conn| conn.usable()).cloned()
    }

    /// Kills `conn` if it is still the live daemon, so the next call starts a fresh one.
    async fn discard(&self, conn: &Arc<Conn>) {
        let mut slot = self.conn.lock().await;
        if slot.as_ref().is_some_and(|live| Arc::ptr_eq(live, conn)) {
            *slot = None;
            conn.kill();
            report_lost(&self.jobs, &self.events);
        }
    }

    /// Sends `op` to the live daemon (starting one if needed) and waits up to `limit` for the
    /// reply.
    async fn request(&self, op: Op, limit: Duration) -> Result<Reply, SandboxError> {
        let conn = self.connect().await?;
        self.request_on(&conn, op, limit).await
    }

    /// Sends `op` to `conn` and waits up to `limit` for the reply. A daemon that dies or stops
    /// answering is discarded.
    async fn request_on(&self, conn: &Arc<Conn>, op: Op, limit: Duration) -> Result<Reply, SandboxError> {
        let mut call = conn.call(op)?;
        let reply = tokio::time::timeout(limit, &mut call.reply).await;
        call.answered = true;
        match reply {
            Ok(Ok(reply)) => Ok(reply),
            Ok(Err(_)) => {
                self.discard(conn).await;
                Err(SandboxError::Unavailable("the sandbox daemon stopped".into()))
            }
            Err(_) => {
                self.discard(conn).await;
                Err(SandboxError::Unavailable("the sandbox daemon stopped answering".into()))
            }
        }
    }
}

impl Conn {
    fn usable(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
            && self.child.lock().is_ok_and(|mut child| matches!(child.try_wait(), Ok(None)))
    }

    fn kill(&self) {
        self.alive.store(false, Ordering::Relaxed);
        if let Ok(mut child) = self.child.lock() {
            let _ = child.start_kill();
        }
    }

    /// Queues a request; the returned call cancels it if dropped before its reply.
    fn call(self: &Arc<Self>, op: Op) -> Result<Call, SandboxError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let frame = encode(&ToDaemon::Request { id, op }).map_err(gone)?;
        let (tx, reply) = oneshot::channel();
        if let Ok(mut pending) = self.pending.lock() {
            pending.insert(id, tx);
        }
        self.frames.send(frame).map_err(|_| gone("its input is closed"))?;
        Ok(Call { conn: Arc::clone(self), id, reply, answered: false })
    }

    /// Queues a request nobody waits for (its answer is dropped). Synchronous.
    fn send_only(&self, op: Op) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        if let Ok(frame) = encode(&ToDaemon::Request { id, op }) {
            let _ = self.frames.send(frame);
        }
    }
}

/// A request waiting for its reply.
struct Call {
    conn: Arc<Conn>,
    id: u64,
    reply: oneshot::Receiver<Reply>,
    answered: bool,
}

impl Drop for Call {
    /// A caller that went away cancels its request (the daemon still answers it, to no one).
    /// Synchronous, so it works without a runtime.
    fn drop(&mut self) {
        if self.answered {
            return;
        }
        if let Ok(mut pending) = self.conn.pending.lock() {
            pending.remove(&self.id);
        }
        if let Ok(frame) = encode(&ToDaemon::Cancel { id: self.id }) {
            let _ = self.conn.frames.send(frame);
        }
    }
}

/// Writes queued frames, each one whole.
async fn write_frames(
    mut stdin: tokio::process::ChildStdin,
    mut frames: mpsc::UnboundedReceiver<Vec<u8>>,
    alive: Arc<AtomicBool>,
) {
    while let Some(frame) = frames.recv().await {
        if stdin.write_all(&frame).await.is_err() || stdin.flush().await.is_err() {
            break;
        }
    }
    alive.store(false, Ordering::Relaxed);
}

/// Routes replies to their callers and events to subscribers until the daemon's output ends,
/// then fails the waiting callers and reports the daemon's jobs as finished.
async fn read_frames(
    mut stdout: ChildStdout,
    pending: Pending,
    alive: Arc<AtomicBool>,
    events: broadcast::Sender<SandboxEvent>,
    jobs: Jobs,
    terminals: Terminals,
) {
    loop {
        match read_frame::<_, FromDaemon>(&mut stdout).await {
            Ok(FromDaemon::Reply { id, reply }) => {
                // Recorded here, in frame order, so the job's `JobFinished` (which the daemon
                // sends after this reply) always finds it.
                if let Reply::Outcome { outcome: Outcome::Started { job }, .. } = &reply {
                    if let Ok(mut jobs) = jobs.lock() {
                        jobs.insert(job.clone(), Instant::now());
                    }
                }
                let waiting = pending.lock().ok().and_then(|mut pending| pending.remove(&id));
                if let Some(tx) = waiting {
                    let _ = tx.send(reply);
                }
            }
            Ok(FromDaemon::Event { event }) => {
                if let SandboxEvent::JobFinished { job, .. } = &event {
                    if let Ok(mut jobs) = jobs.lock() {
                        jobs.remove(job);
                    }
                }
                let _ = events.send(event);
            }
            Ok(FromDaemon::TerminalOutput { terminal, data }) => {
                if let Some(tx) = terminals.lock().ok().and_then(|open| open.get(&terminal).cloned()) {
                    let _ = tx.send(TerminalEvent::Output(data));
                }
            }
            Ok(FromDaemon::TerminalClosed { terminal, reason }) => {
                if let Some(tx) = terminals.lock().ok().and_then(|mut open| open.remove(&terminal)) {
                    let _ = tx.send(TerminalEvent::Closed { reason });
                }
            }
            Ok(FromDaemon::Hello { .. }) => {}
            Err(err) => {
                tracing::debug!(target: "nixxa_sandbox", "the sandbox daemon's output ended: {err}");
                break;
            }
        }
    }
    alive.store(false, Ordering::Relaxed);
    if let Ok(mut pending) = pending.lock() {
        pending.clear();
    }
    report_lost(&jobs, &events);
    // A terminal can't outlive its daemon.
    let open: Vec<_> = terminals.lock().map(|mut open| open.drain().collect()).unwrap_or_default();
    for (_, tx) in open {
        let _ = tx.send(TerminalEvent::Closed { reason: "the sandbox stopped".into() });
    }
}

/// The user's terminal in the daemon (D-066).
struct BwrapTerminal {
    /// The daemon it runs in. Weak: an open terminal doesn't keep a discarded daemon alive.
    conn: Weak<Conn>,
    id: u64,
    /// Set once `close` was called, so dropping it doesn't ask again.
    closed: AtomicBool,
}

impl BwrapTerminal {
    /// Sends `op` and waits for the answer (up to `limit`). A terminal that doesn't answer in
    /// time isn't a reason to discard the daemon.
    async fn ask(&self, op: Op, limit: Option<Duration>) -> Result<(), SandboxError> {
        let ended = || SandboxError::Unavailable("the terminal has ended".into());
        let conn = self.conn.upgrade().ok_or_else(ended)?;
        let mut call = conn.call(op)?;
        let reply = match limit {
            Some(limit) => tokio::time::timeout(limit, &mut call.reply)
                .await
                .map_err(|_| SandboxError::Unavailable("the terminal didn't answer".into()))?,
            None => (&mut call.reply).await,
        };
        call.answered = true;
        match reply.map_err(|_| ended())? {
            Reply::Done => Ok(()),
            Reply::Error { message } => Err(SandboxError::Unavailable(message)),
            other => Err(SandboxError::Unavailable(format!("unexpected answer: {other:?}"))),
        }
    }
}

#[async_trait]
impl Terminal for BwrapTerminal {
    async fn write(&self, input: &[u8]) -> Result<(), SandboxError> {
        // Answered once the PTY took it, which waits while the shell isn't reading.
        self.ask(Op::TerminalInput { terminal: self.id, data: input.to_vec() }, None).await
    }

    async fn resize(&self, size: TerminalSize) -> Result<(), SandboxError> {
        self.ask(Op::TerminalResize { terminal: self.id, size }, Some(QUICK_TIMEOUT)).await
    }

    async fn close(&self) -> Result<(), SandboxError> {
        self.closed.store(true, Ordering::Relaxed);
        match self.ask(Op::TerminalClose { terminal: self.id }, Some(QUICK_TIMEOUT)).await {
            // Gone with its daemon: nothing left to end.
            Err(SandboxError::Unavailable(why)) if why == "the terminal has ended" => Ok(()),
            other => other,
        }
    }
}

impl Drop for BwrapTerminal {
    /// Asks the daemon to end it, without waiting (no runtime needed).
    fn drop(&mut self) {
        if self.closed.load(Ordering::Relaxed) {
            return;
        }
        if let Some(conn) = self.conn.upgrade() {
            conn.send_only(Op::TerminalClose { terminal: self.id });
        }
    }
}

/// A terminal's events, acknowledging each chunk of output to the daemon as it is consumed.
fn terminal_events(
    rx: mpsc::UnboundedReceiver<TerminalEvent>,
    frames: mpsc::UnboundedSender<Vec<u8>>,
    terminal: u64,
) -> BoxStream<'static, TerminalEvent> {
    Box::pin(futures::stream::unfold((rx, frames), move |(mut rx, frames)| async move {
        let event = rx.recv().await?;
        if let TerminalEvent::Output(data) = &event {
            let bytes = u32::try_from(data.len()).unwrap_or(u32::MAX);
            if let Ok(frame) = encode(&ToDaemon::TerminalAck { terminal, bytes }) {
                let _ = frames.send(frame);
            }
        }
        Some((event, (rx, frames)))
    }))
}

/// Reports every job still running as finished (killed): their daemon is gone.
fn report_lost(jobs: &Jobs, events: &broadcast::Sender<SandboxEvent>) {
    let lost: Vec<(JobId, Instant)> = jobs.lock().map(|mut jobs| jobs.drain().collect()).unwrap_or_default();
    for (job, started) in lost {
        let duration_ms = started.elapsed().as_millis() as u64;
        let _ = events.send(SandboxEvent::JobFinished { job, exit_code: None, duration_ms, truncated: false });
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
            network: NetworkAccess::Controlled(self.inner.proxy.policy()),
            jobs: true,
            // Not once the daemon has said Landlock isn't there: the agent's commands couldn't be
            // kept away from the terminal (D-066).
            terminal: landlock || !self.inner.started.load(Ordering::Relaxed),
            actions: SandboxActionKind::ALL.to_vec(),
        }
    }

    async fn execute(&self, id: ActionId, action: SandboxAction) -> Result<Observation, SandboxError> {
        let started = Instant::now();
        let config = &self.inner.config;
        let (limit, foreground) = match &action {
            SandboxAction::Exec { timeout_ms, background: false, .. } => {
                let asked = timeout_ms.map_or(config.default_timeout, Duration::from_millis);
                (asked.min(config.max_timeout) + GRACE, true)
            }
            SandboxAction::WriteFile { content, .. } if content.len() as u64 > config.file_cap => {
                let message =
                    format!("the content is too large to write ({} bytes, limit {})", content.len(), config.file_cap);
                return Ok(Observation::failed(id, message));
            }
            _ => (QUICK_TIMEOUT, false),
        };
        let _turn = if foreground { Some(self.inner.foreground.lock().await) } else { None };
        if foreground {
            self.inner.proxy.begin_foreground();
        }
        let reply = self.inner.request(Op::Action { action }, limit).await;
        let network = if foreground { self.inner.proxy.end_foreground().await } else { Vec::new() };
        let duration_ms = started.elapsed().as_millis() as u64;
        match reply? {
            Reply::Outcome { outcome, truncated } => {
                Ok(Observation { action_id: id, outcome, duration_ms, truncated, network })
            }
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
        match self.inner.request(Op::Ping, QUICK_TIMEOUT).await? {
            Reply::Done => Ok(()),
            other => Err(SandboxError::Unavailable(format!("unexpected answer to a ping: {other:?}"))),
        }
    }

    async fn reset(&self) -> Result<(), SandboxError> {
        // No daemon: nothing is running.
        let Some(conn) = self.inner.current().await else { return Ok(()) };
        // If the daemon fails, discarding it has killed everything anyway.
        let _ = self.inner.request_on(&conn, Op::Reset, QUICK_TIMEOUT).await;
        Ok(())
    }

    async fn set_network(&self, policy: NetworkPolicy) -> Result<(), SandboxError> {
        let on = policy != NetworkPolicy::Off;
        if on {
            self.inner.proxy.usable().map_err(SandboxError::Unavailable)?;
            self.inner.proxy.serve().map_err(SandboxError::Unavailable)?;
        } else {
            // Refuse at once, before the daemon hears of it.
            self.inner.proxy.set_policy(NetworkPolicy::Off);
        }
        // A daemon started later reads it from the handshake.
        if let Some(conn) = self.inner.current().await {
            match self.inner.request_on(&conn, Op::Network { on }, QUICK_TIMEOUT).await? {
                Reply::Done => {}
                Reply::Error { message } => return Err(SandboxError::Unavailable(message)),
                other => return Err(SandboxError::Unavailable(format!("unexpected answer: {other:?}"))),
            }
        }
        self.inner.proxy.set_policy(policy);
        Ok(())
    }

    fn subscribe(&self) -> BoxStream<'static, SandboxEvent> {
        crate::events::stream(self.inner.events.subscribe())
    }

    async fn open_terminal(&self, size: TerminalSize) -> Result<OpenTerminal, SandboxError> {
        let conn = self.inner.connect().await?;
        if !self.inner.landlock.load(Ordering::Relaxed) {
            return Err(SandboxError::Unsupported(
                "no terminal without Landlock: the agent's commands couldn't be kept away from it".into(),
            ));
        }
        let id = self.inner.terminals.fetch_add(1, Ordering::Relaxed) + 1;
        // Registered first, so no output can arrive before its stream exists.
        let (tx, rx) = mpsc::unbounded_channel();
        if let Ok(mut open) = conn.terminals.lock() {
            open.insert(id, tx);
        }
        let terminal = BwrapTerminal { conn: Arc::downgrade(&conn), id, closed: AtomicBool::new(false) };
        if let Err(err) = terminal.ask(Op::TerminalOpen { terminal: id, size }, Some(QUICK_TIMEOUT)).await {
            terminal.closed.store(true, Ordering::Relaxed);
            if let Ok(mut open) = conn.terminals.lock() {
                open.remove(&id);
            }
            return Err(err);
        }
        let events = terminal_events(rx, conn.frames.clone(), id);
        Ok(OpenTerminal { terminal: Box::new(terminal), events })
    }
}
