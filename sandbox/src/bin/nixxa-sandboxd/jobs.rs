//! The daemon's side of a job: start its supervisor, capture its output, and end it.
//!
//! Every job (the agent's foreground command, each background job, the user's terminal) runs
//! under its own supervisor (`--limited`, see [`crate::exec`]), a child subreaper. A job is therefore exactly
//! the supervisor's descendants, found by following parent links in `/proc`, which also covers
//! processes that left the process group or called `setsid`. Ending a job kills those
//! descendants until none is left, then the supervisor; nothing outside the tree is touched.
//!
//! The daemon is a child subreaper too, as a backstop: if a supervisor dies before its job
//! (a command can `kill -9 $PPID`), its processes are re-parented to the daemon. That job has
//! ended, so [`sweep_orphans`] kills them and reaps what is left. The daemon's only legitimate
//! children are the supervisors it started ([`Supervisors`]); it waits on specific pids only,
//! never `-1`, so it can't steal a supervisor's exit status from tokio.

use std::collections::{HashMap, HashSet};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream as StdUnixStream;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal, WaitOptions};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};

use crate::exec::Supervise;
use crate::job::{End, Tail};
use crate::protocol::Rlimits;

/// How long a finished shell's pipes may stay open (held by something it started in the
/// background) before the leftovers are killed.
const LINGER: Duration = Duration::from_millis(250);
/// How long killing a job's processes may take (a fork bomb may still be forking).
const KILL_PATIENCE: Duration = Duration::from_secs(10);
/// How long the output readers get once every process of the job is dead.
const DRAIN: Duration = Duration::from_secs(2);

/// The live supervisors, the daemon's only legitimate children, with who each job is for
/// (`fg` for the foreground command, `terminal` for the user's terminal, else the job's id).
#[derive(Debug, Clone, Default)]
pub(crate) struct Supervisors(Arc<Mutex<HashMap<i32, String>>>);

impl Supervisors {
    fn register(&self, pid: i32, owner: &str) -> Registered {
        if let Ok(mut set) = self.0.lock() {
            set.insert(pid, owner.to_string());
        }
        Registered { pid, set: self.clone() }
    }

    fn contains(&self, pid: i32) -> bool {
        self.0.lock().is_ok_and(|set| set.contains_key(&pid))
    }

    /// Whose job the process `pid` belongs to: its nearest supervisor's owner.
    pub(crate) fn owner_of(&self, pid: i32) -> Option<String> {
        let table = processes();
        let set = self.0.lock().ok()?;
        let mut at = pid;
        for _ in 0..table.len() + 1 {
            if let Some(owner) = set.get(&at) {
                return Some(owner.clone());
            }
            at = table.get(&at)?.parent;
        }
        None
    }
}

/// Removes a supervisor from the set when its job is over (or its task is dropped).
struct Registered {
    pid: i32,
    set: Supervisors,
}

impl Drop for Registered {
    fn drop(&mut self) {
        if let Ok(mut set) = self.set.0.lock() {
            set.remove(&self.pid);
        }
    }
}

/// What to run.
#[derive(Debug, Clone)]
pub(crate) struct Spec {
    /// The command line for `/bin/sh -c`, or for the terminal, the shell to run.
    pub command: String,
    /// The working directory (already checked to be inside the workspace).
    pub cwd: String,
    /// The time limit, if any (the terminal has none).
    pub limit: Option<Duration>,
    /// The rlimits of every process of the job.
    pub rlimits: Rlimits,
    /// This binary, to run as the supervisor.
    pub self_exe: String,
    /// Who the job is for: `fg`, `terminal` or the job's id.
    pub owner: String,
    /// Variables added to the job's environment (the network proxy's; the terminal's `TERM`).
    pub env: Vec<(String, String)>,
    /// Add the agent's Landlock layer (D-066). False only for the user's terminal, and when the
    /// daemon's own layer isn't enforced either.
    pub confine: bool,
    /// The user's terminal: the PTY its shell runs on.
    pub pty: Option<String>,
}

/// How to keep the job's output.
pub(crate) enum Capture {
    /// The first `cap` bytes of each stream; the job is stopped once one goes past it
    /// (foreground commands).
    First { cap: usize },
    /// The last bytes of each stream, readable while it runs; the job keeps running
    /// (background jobs).
    Last { stdout: Arc<Mutex<Tail>>, stderr: Arc<Mutex<Tail>> },
    /// Nothing: the output goes to a PTY (the user's terminal).
    Nothing,
}

/// How a job went.
pub(crate) struct Ran {
    /// How it ended.
    pub end: End,
    /// Its stdout, for [`Capture::First`].
    pub stdout: Vec<u8>,
    /// Its stderr, for [`Capture::First`].
    pub stderr: Vec<u8>,
    /// Whether output was dropped (or, for [`Capture::First`], the job was stopped for it).
    pub truncated: bool,
    /// How long it ran.
    pub duration: Duration,
}

/// Runs one job to its end: the shell exits (then leftovers get [`LINGER`]), the time limit
/// passes, or `stop` fires (or its sender is dropped). Every process of the job is dead when
/// this returns.
pub(crate) async fn run(
    spec: Spec,
    capture: Capture,
    supervisors: &Supervisors,
    mut stop: oneshot::Receiver<()>,
) -> Ran {
    let started = Instant::now();
    let failed = |why: String| Ran {
        end: End::Failed(why),
        stdout: Vec::new(),
        stderr: Vec::new(),
        truncated: false,
        duration: started.elapsed(),
    };
    let supervise = Supervise { rlimits: spec.rlimits, confine: spec.confine, pty: spec.pty.clone() };
    let supervise = match serde_json::to_string(&supervise) {
        Ok(json) => json,
        Err(err) => return failed(err.to_string()),
    };
    let (ours, theirs) = match StdUnixStream::pair() {
        Ok(pair) => pair,
        Err(err) => return failed(format!("could not start the shell: {err}")),
    };
    let mut supervisor = Command::new(&spec.self_exe);
    supervisor
        .arg("--limited")
        .arg(supervise)
        .arg("--")
        .arg(&spec.command)
        .current_dir(&spec.cwd)
        .envs(spec.env.iter().map(|(name, value)| (name, value)))
        .stdin(Stdio::from(OwnedFd::from(theirs)))
        .kill_on_drop(true);
    if matches!(capture, Capture::Nothing) {
        // The terminal's supervisor starts a session of its own, which a process-group
        // leader can't.
        supervisor.stdout(Stdio::null()).stderr(Stdio::null());
    } else {
        supervisor.stdout(Stdio::piped()).stderr(Stdio::piped()).process_group(0);
    }
    let spawned = supervisor.spawn();
    // The command holds the daemon's copy of the supervisor's end of the socket: without this,
    // a supervisor that dies (`kill -9 $PPID`) would never be noticed.
    drop(supervisor);
    let mut child = match spawned {
        Ok(child) => child,
        Err(err) => return failed(format!("could not start the shell: {err}")),
    };
    let Some(pid) = child.id().map(|pid| pid as i32) else {
        return failed("the shell exited at once".into());
    };
    // Registered before the next await, so the orphan sweep never mistakes it for an orphan.
    let registered = supervisors.register(pid, &spec.owner);
    let status = shell_status(ours);
    tokio::pin!(status);

    let (overflow_tx, mut overflow) = mpsc::channel::<()>(2);
    let (out_task, err_task) = match &capture {
        Capture::First { cap } => (
            tokio::spawn(keep_first(child.stdout.take(), *cap, overflow_tx.clone())),
            tokio::spawn(keep_first(child.stderr.take(), *cap, overflow_tx)),
        ),
        Capture::Last { stdout, stderr } => (
            tokio::spawn(keep_last(child.stdout.take(), Arc::clone(stdout))),
            tokio::spawn(keep_last(child.stderr.take(), Arc::clone(stderr))),
        ),
        Capture::Nothing => (tokio::spawn(async { (Vec::new(), false) }), tokio::spawn(async { (Vec::new(), false) })),
    };
    let aborts = [out_task.abort_handle(), err_task.abort_handle()];
    let collect = async { (out_task.await.unwrap_or_default(), err_task.await.unwrap_or_default()) };
    tokio::pin!(collect);
    let deadline = async {
        match spec.limit {
            Some(limit) => tokio::time::sleep(limit).await,
            None => std::future::pending().await,
        }
    };
    tokio::pin!(deadline);

    let mut killed_for_output = false;
    let end = loop {
        tokio::select! {
            code = &mut status => break End::Exited(code),
            _ = &mut deadline => break End::TimedOut,
            _ = &mut stop => break End::Stopped,
            Some(()) = overflow.recv(), if !killed_for_output => {
                // Past the output cap: stop the command instead of letting it run on unseen.
                killed_for_output = true;
                kill_tree(pid).await;
            }
        }
    };
    let mut output = None;
    if matches!(end, End::Exited(_)) {
        // The shell is gone; give anything still holding its pipes a moment.
        output = tokio::time::timeout(LINGER, &mut collect).await.ok();
    }
    kill_tree(pid).await;
    let _ = child.wait().await;
    drop(registered);
    let ((stdout, out_cut), (stderr, err_cut)) = match output {
        Some(both) => both,
        // Every process of the job is dead now, so the pipes close, unless one was passed out
        // of the job.
        None => match tokio::time::timeout(DRAIN, &mut collect).await {
            Ok(both) => both,
            Err(_) => {
                aborts.iter().for_each(|task| task.abort());
                ((Vec::new(), true), (Vec::new(), true))
            }
        },
    };
    let truncated = out_cut || err_cut || killed_for_output;
    Ran { end, stdout, stderr, truncated, duration: started.elapsed() }
}

/// The shell's exit code as the supervisor reports it: `None` if it was killed by a signal, or
/// if the supervisor died without saying.
async fn shell_status(stream: StdUnixStream) -> Option<i32> {
    stream.set_nonblocking(true).ok()?;
    let mut stream = tokio::net::UnixStream::from_std(stream).ok()?;
    // One short line; the supervisor keeps the socket open afterwards, so don't wait for EOF.
    let mut line = Vec::new();
    let mut buf = [0u8; 32];
    while !line.contains(&b'\n') && line.len() < 64 {
        match stream.read(&mut buf).await {
            Ok(0) | Err(_) => return None,
            Ok(n) => line.extend_from_slice(&buf[..n]),
        }
    }
    let line = String::from_utf8_lossy(&line);
    line.lines().next()?.strip_prefix("exit ")?.parse().ok()
}

/// Reads a stream to its end, keeping the first `cap` bytes. Signals `overflow` once past it.
async fn keep_first(stream: Option<impl AsyncRead + Unpin>, cap: usize, overflow: mpsc::Sender<()>) -> (Vec<u8>, bool) {
    let Some(mut stream) = stream else { return (Vec::new(), false) };
    let mut kept = Vec::new();
    let mut cut = false;
    let mut buf = [0u8; 16 * 1024];
    loop {
        match stream.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let room = cap.saturating_sub(kept.len());
                kept.extend_from_slice(&buf[..n.min(room)]);
                if n > room && !cut {
                    cut = true;
                    let _ = overflow.try_send(());
                }
            }
        }
    }
    if cut {
        // Never through a token (D-074).
        kept.truncate(crate::cut::end(&kept));
    }
    (kept, cut)
}

/// Reads a stream to its end into `tail`.
async fn keep_last(stream: Option<impl AsyncRead + Unpin>, tail: Arc<Mutex<Tail>>) -> (Vec<u8>, bool) {
    let Some(mut stream) = stream else { return (Vec::new(), false) };
    let mut buf = [0u8; 16 * 1024];
    loop {
        match stream.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if let Ok(mut tail) = tail.lock() {
                    tail.push(&buf[..n]);
                }
            }
        }
    }
    let cut = tail.lock().is_ok_and(|tail| tail.cut());
    (Vec::new(), cut)
}

/// Kills the job under the supervisor `root`: its descendants, repeatedly until none is left
/// alive (they are re-parented to the supervisor as their parents die, so they stay in the
/// tree), then the supervisor itself.
async fn kill_tree(root: i32) {
    let started = Instant::now();
    loop {
        let alive = descendants(&processes(), root);
        if alive.is_empty() || started.elapsed() > KILL_PATIENCE {
            break;
        }
        alive.into_iter().for_each(kill);
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    kill(root);
}

/// Kills (and reaps) every child of the daemon that isn't a live supervisor, with everything
/// under it: the remains of a job whose supervisor died first.
pub(crate) fn sweep_orphans(supervisors: &Supervisors) {
    let me = rustix::process::getpid().as_raw_nonzero().get();
    let table = processes();
    for process in table.values().filter(|p| p.parent == me && !supervisors.contains(p.pid)) {
        if process.zombie {
            if let Some(pid) = Pid::from_raw(process.pid) {
                let _ = rustix::process::waitpid(Some(pid), WaitOptions::NOHANG);
            }
        } else {
            descendants(&table, process.pid).into_iter().for_each(kill);
            kill(process.pid);
        }
    }
}

fn kill(pid: i32) {
    if let Some(pid) = Pid::from_raw(pid) {
        let _ = rustix::process::kill_process(pid, Signal::KILL);
    }
}

/// One process, from `/proc/<pid>/stat`.
struct Process {
    pid: i32,
    parent: i32,
    zombie: bool,
}

/// Every process in the sandbox's PID namespace, by pid.
fn processes() -> HashMap<i32, Process> {
    let mut table = HashMap::new();
    let Ok(dir) = std::fs::read_dir("/proc") else { return table };
    for entry in dir.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|name| name.parse::<i32>().ok()) else { continue };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else { continue };
        // The state and parent follow the last `)`, which closes the command name.
        let Some((_, rest)) = stat.rsplit_once(')') else { continue };
        let mut fields = rest.split_whitespace();
        let zombie = fields.next().is_some_and(|state| state.starts_with(['Z', 'X']));
        let Some(parent) = fields.next().and_then(|ppid| ppid.parse().ok()) else { continue };
        table.insert(pid, Process { pid, parent, zombie });
    }
    table
}

/// The live (not zombie) descendants of `root`, without `root` itself.
fn descendants(table: &HashMap<i32, Process>, root: i32) -> Vec<i32> {
    let mut children: HashMap<i32, Vec<&Process>> = HashMap::new();
    for process in table.values() {
        children.entry(process.parent).or_default().push(process);
    }
    let mut found = Vec::new();
    let mut queue = vec![root];
    let mut seen = HashSet::from([root]);
    while let Some(pid) = queue.pop() {
        for child in children.get(&pid).into_iter().flatten() {
            if seen.insert(child.pid) {
                queue.push(child.pid);
                if !child.zombie {
                    found.push(child.pid);
                }
            }
        }
    }
    found
}
