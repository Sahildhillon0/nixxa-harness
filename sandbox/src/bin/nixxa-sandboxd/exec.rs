//! `Exec`: runs `/bin/sh -c <command>` in its own process group, under rlimits, with a time
//! limit and an output cap, and kills everything it left behind.
//!
//! The daemon runs as PID 2 of the sandbox's PID namespace (bubblewrap's init is PID 1), and runs
//! one command at a time, so "everything the command started" is simply every process in the
//! namespace except those two. [`kill_all`] kills exactly that, which also catches processes that
//! left the command's process group or session.

use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::protocol::{Limits, Reply, Rlimits};
use nixxa_contracts::Outcome;

/// How long a finished command's pipes may stay open (held by a background process) before the
/// leftovers are killed.
const LINGER: Duration = Duration::from_millis(250);

/// Why a running command must stop early.
pub(crate) enum Stop {
    /// The client cancelled it.
    Cancelled,
}

/// Runs one command. `stop` delivers an early stop from the client.
pub(crate) async fn run(
    command: &str,
    cwd: &str,
    timeout_ms: Option<u64>,
    limits: &Limits,
    self_exe: &str,
    stop: &mut mpsc::Receiver<Stop>,
) -> (Reply, Option<i32>) {
    let limit = Duration::from_millis(timeout_ms.unwrap_or(limits.default_timeout_ms).min(limits.max_timeout_ms));
    let rlimits = match serde_json::to_string(&limits.rlimits) {
        Ok(json) => json,
        Err(err) => return (Reply::Error { message: err.to_string() }, None),
    };
    // The daemon re-executes itself in `--limited` mode, which applies the rlimits to itself and
    // then becomes `/bin/sh -c <command>` (see `limited`).
    let mut child = match Command::new(self_exe)
        .arg("--limited")
        .arg(rlimits)
        .arg("--")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(err) => {
            let outcome = Outcome::Failed { message: format!("could not start the shell: {err}") };
            return (Reply::Outcome { outcome, truncated: false }, None);
        }
    };
    let cap = limits.output_cap as usize;
    let (overflow_tx, mut overflow) = mpsc::channel::<()>(2);
    let out = tokio::spawn(capture(child.stdout.take(), cap, overflow_tx.clone()));
    let err = tokio::spawn(capture(child.stderr.take(), cap, overflow_tx));
    let deadline = tokio::time::sleep(limit);
    tokio::pin!(deadline);

    let mut killed_for_output = false;
    let status = loop {
        tokio::select! {
            status = child.wait() => break status.ok(),
            _ = &mut deadline => {
                kill_all().await;
                let _ = child.wait().await;
                return (Reply::Timeout, None);
            }
            Some(Stop::Cancelled) = stop.recv() => {
                kill_all().await;
                let _ = child.wait().await;
                return (Reply::Cancelled, None);
            }
            Some(()) = overflow.recv(), if !killed_for_output => {
                // Past the output cap: stop the command instead of letting it run on unseen.
                killed_for_output = true;
                kill_all().await;
            }
        }
    };
    // The shell is gone; give anything still holding its pipes a moment, then kill it.
    let collect = async { (out.await, err.await) };
    tokio::pin!(collect);
    let (out, err) = match tokio::time::timeout(LINGER, &mut collect).await {
        Ok(both) => both,
        Err(_) => {
            kill_all().await;
            collect.await
        }
    };
    kill_all().await;
    let (stdout, out_cut) = out.unwrap_or_default();
    let (stderr, err_cut) = err.unwrap_or_default();
    let exit_code = status.and_then(|s| s.code());
    let outcome = Outcome::Exec {
        exit_code,
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
    };
    (Reply::Outcome { outcome, truncated: out_cut || err_cut || killed_for_output }, exit_code)
}

/// Reads a stream to its end, keeping the first `cap` bytes. Signals `overflow` once past it.
async fn capture(stream: Option<impl AsyncRead + Unpin>, cap: usize, overflow: mpsc::Sender<()>) -> (Vec<u8>, bool) {
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
    (kept, cut)
}

/// Kills every process in the sandbox except bubblewrap's init (PID 1) and the daemon itself,
/// repeating until none is left (a fork bomb may still be forking during the first passes).
pub(crate) async fn kill_all() {
    let me = rustix::process::getpid().as_raw_nonzero().get();
    let started = Instant::now();
    loop {
        let mut alive = 0;
        if let Ok(dir) = std::fs::read_dir("/proc") {
            for entry in dir.flatten() {
                let Some(pid) = entry.file_name().to_str().and_then(|s| s.parse::<i32>().ok()) else { continue };
                if pid == 1 || pid == me || is_zombie(pid) {
                    continue;
                }
                alive += 1;
                if let Some(pid) = rustix::process::Pid::from_raw(pid) {
                    let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
                }
            }
        }
        if alive == 0 || started.elapsed() > Duration::from_secs(10) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn is_zombie(pid: i32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        // The state follows the last `)`, which closes the command name.
        Ok(stat) => stat.rsplit_once(')').is_some_and(|(_, rest)| rest.trim_start().starts_with(['Z', 'X'])),
        // Already gone.
        Err(_) => true,
    }
}

/// `--limited` mode: applies the rlimits to this process, then becomes `/bin/sh -c <command>`.
/// Only returns (with an exit code) if that fails.
pub(crate) fn limited(rlimits: &str, command: &str) -> i32 {
    use rustix::process::{Resource, Rlimit, setrlimit};
    use std::os::unix::process::CommandExt;

    let limits: Rlimits = match serde_json::from_str(rlimits) {
        Ok(limits) => limits,
        Err(err) => {
            eprintln!("nixxa-sandboxd: bad rlimits: {err}");
            return 126;
        }
    };
    let set = [
        (Resource::Nproc, limits.nproc),
        (Resource::Fsize, limits.fsize),
        (Resource::Nofile, limits.nofile),
        (Resource::Cpu, limits.cpu_secs),
        (Resource::Data, limits.data),
    ];
    for (resource, value) in set {
        if let Err(err) = setrlimit(resource, Rlimit { current: Some(value), maximum: Some(value) }) {
            eprintln!("nixxa-sandboxd: setrlimit {resource:?}: {err}");
            return 126;
        }
    }
    let err = std::process::Command::new("/bin/sh").arg("-c").arg(command).exec();
    eprintln!("nixxa-sandboxd: could not run /bin/sh: {err}");
    127
}
