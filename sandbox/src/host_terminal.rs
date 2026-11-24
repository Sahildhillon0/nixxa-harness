//! The user's terminal in This directory (D-066): the user's own shell, on a PTY, in the root.
//!
//! Not isolated, like everything in [`HostSandbox`](crate::host::HostSandbox): the shell is the
//! user's, with the user's reach. The harness's secret variables are removed from its
//! environment, as for commands. The shell leads its own session and process group; ending the
//! terminal sends it `SIGHUP` (so a shell passes the hangup on to its jobs, as when a terminal
//! window closes), then kills its process group. A process that leaves the group and ignores
//! the hangup (`nohup`, `setsid`) outlives it, one more way this mode isn't isolated.
//!
//! The PTY is read on a thread of its own, at most a few chunks ahead of the reader of the
//! events: past that the thread waits, the PTY's buffer fills and the shell's writes block, so
//! nothing is dropped and memory doesn't grow.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use tokio::sync::{mpsc, watch};

use nixxa_contracts::{OpenTerminal, SandboxError, Terminal, TerminalEvent, TerminalSize};

/// Chunks of output read ahead of the events' reader.
const AHEAD: usize = 4;
/// How long the output may take to finish once the shell has exited.
const DRAIN: Duration = Duration::from_secs(2);
/// How long the shell gets to pass a hangup on to its jobs before its group is killed.
const HANGUP: Duration = Duration::from_millis(50);
/// How long `close` and `reset` wait for the terminal to end.
const PATIENCE: Duration = Duration::from_secs(10);

/// The open terminal, as the sandbox sees it (to end it on reset or drop).
#[derive(Debug)]
pub(crate) struct Running {
    pid: u32,
    why: Why,
    done: watch::Receiver<bool>,
}

/// The sandbox's terminal slot: one at a time.
pub(crate) type Slot = Arc<Mutex<Option<Running>>>;

/// Why the terminal was ended, once it was.
type Why = Arc<Mutex<Option<String>>>;

/// Ends the open terminal, if any, saying `why`, and waits for it to end.
pub(crate) async fn end(slot: &Slot, why: &str) {
    let running = slot.lock().ok().and_then(|mut slot| slot.take());
    if let Some(mut running) = running {
        hang_up(running.pid, &running.why, why);
        let _ = tokio::time::timeout(PATIENCE, running.done.wait_for(|done| *done)).await;
    }
}

/// Ends the open terminal now, without waiting (the sandbox is being dropped).
pub(crate) fn end_now(slot: &Slot) {
    if let Some(running) = slot.lock().ok().and_then(|mut slot| slot.take()) {
        hang_up(running.pid, &running.why, "the session ended");
    }
}

/// Records `why`, sends the shell `SIGHUP`, and kills its process group a moment later.
fn hang_up(pid: u32, why: &Why, reason: &str) {
    if let Ok(mut why) = why.lock() {
        why.get_or_insert_with(|| reason.to_string());
    }
    let Some(shell) = rustix::process::Pid::from_raw(pid as i32) else { return };
    let _ = rustix::process::kill_process(shell, rustix::process::Signal::HUP);
    std::thread::sleep(HANGUP);
    let _ = rustix::process::kill_process_group(shell, rustix::process::Signal::KILL);
}

/// Opens a terminal running `shell` as a login shell in `root`, without `hidden_env`.
pub(crate) fn open(
    slot: &Slot,
    root: &Path,
    shell: &Path,
    hidden_env: &[String],
    size: TerminalSize,
) -> Result<OpenTerminal, SandboxError> {
    let unavailable = |what: &str, err: &dyn std::fmt::Display| SandboxError::Unavailable(format!("{what}: {err}"));
    let mut taken = slot.lock().map_err(|_| SandboxError::Unavailable("the terminal broke".into()))?;
    if taken.as_ref().is_some_and(|running| !*running.done.borrow()) {
        return Err(SandboxError::Unavailable("a terminal is already open".into()));
    }
    let pair = native_pty_system().openpty(pty_size(size)).map_err(|err| unavailable("no terminal device", &err))?;
    let mut command = CommandBuilder::new(shell);
    command.arg("-l");
    command.cwd(root);
    for name in hidden_env {
        command.env_remove(name);
    }
    command.env("TERM", "xterm-256color");
    let mut child = pair
        .slave
        .spawn_command(command)
        .map_err(|err| unavailable(&format!("couldn't start {}", shell.display()), &err))?;
    drop(pair.slave);
    let pid = child.process_id().ok_or_else(|| SandboxError::Unavailable("the shell exited at once".into()))?;
    let reader = pair.master.try_clone_reader().map_err(|err| unavailable("read the terminal", &err))?;
    let writer = pair.master.take_writer().map_err(|err| unavailable("write the terminal", &err))?;

    let (events, rx) = mpsc::channel(AHEAD);
    let (done_tx, done) = watch::channel(false);
    let why = Why::default();
    let (chunks, output) = std::sync::mpsc::sync_channel::<Vec<u8>>(AHEAD);
    std::thread::spawn(move || read_pty(reader, chunks));
    let reason = Arc::clone(&why);
    std::thread::spawn(move || {
        // The only sender of events, so nothing can follow `Closed`.
        let mut exited_at = None;
        let mut status = None;
        loop {
            match output.recv_timeout(Duration::from_millis(50)) {
                Ok(chunk) => {
                    if events.blocking_send(TerminalEvent::Output(chunk)).is_err() {
                        // Nobody reads the events: keep the shell from blocking on its output.
                        continue;
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            }
            if status.is_none() {
                if let Ok(Some(exit)) = child.try_wait() {
                    status = Some(exit);
                    exited_at = Some(Instant::now());
                    // What it left in its group goes with it.
                    if let Some(group) = rustix::process::Pid::from_raw(pid as i32) {
                        let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
                    }
                }
            }
            // Something outside its group may hold the terminal open: don't wait on it forever.
            if exited_at.is_some_and(|at: Instant| at.elapsed() > DRAIN) {
                break;
            }
        }
        let status = status.or_else(|| child.wait().ok());
        let given = reason.lock().ok().and_then(|mut why| why.take());
        let reason = given.unwrap_or_else(|| match status {
            Some(status) if status.success() => "the shell exited".to_string(),
            Some(status) if status.signal().is_some() => "the shell was killed".to_string(),
            Some(status) => format!("the shell exited with code {}", status.exit_code()),
            None => "the shell ended".to_string(),
        });
        let _ = events.blocking_send(TerminalEvent::Closed { reason });
        let _ = done_tx.send(true);
    });
    *taken = Some(Running { pid, why: Arc::clone(&why), done: done.clone() });
    let terminal = HostTerminal {
        pid,
        why,
        done,
        writer: Arc::new(Mutex::new(writer)),
        master: Mutex::new(pair.master),
        slot: Arc::clone(slot),
    };
    let events = futures::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|event| (event, rx)) });
    Ok(OpenTerminal { terminal: Box::new(terminal), events: Box::pin(events) })
}

/// Reads the PTY until it ends, handing each chunk on (and waiting while `AHEAD` are queued).
fn read_pty(mut reader: Box<dyn Read + Send>, chunks: std::sync::mpsc::SyncSender<Vec<u8>>) {
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                if chunks.send(buf[..n].to_vec()).is_err() {
                    return;
                }
            }
        }
    }
}

fn pty_size(size: TerminalSize) -> PtySize {
    PtySize { rows: size.rows, cols: size.cols, pixel_width: 0, pixel_height: 0 }
}

/// The handle of a host terminal.
struct HostTerminal {
    pid: u32,
    why: Why,
    done: watch::Receiver<bool>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    slot: Slot,
}

impl HostTerminal {
    fn live(&self) -> Result<(), SandboxError> {
        if *self.done.borrow() || self.why.lock().is_ok_and(|why| why.is_some()) {
            Err(SandboxError::Unavailable("the terminal has ended".into()))
        } else {
            Ok(())
        }
    }

    /// Clears the sandbox's slot if it still holds this terminal.
    fn leave_slot(&self) {
        if let Ok(mut slot) = self.slot.lock() {
            if slot.as_ref().is_some_and(|running| running.pid == self.pid) {
                *slot = None;
            }
        }
    }
}

#[async_trait]
impl Terminal for HostTerminal {
    async fn write(&self, input: &[u8]) -> Result<(), SandboxError> {
        self.live()?;
        let (writer, input) = (Arc::clone(&self.writer), input.to_vec());
        // Blocks while the shell isn't reading, so off the runtime's threads.
        let written = tokio::task::spawn_blocking(move || {
            let mut writer = writer.lock().map_err(|_| std::io::Error::other("the terminal broke"))?;
            writer.write_all(&input).and_then(|()| writer.flush())
        })
        .await;
        match written {
            Ok(Ok(())) => Ok(()),
            Ok(Err(err)) => Err(SandboxError::Unavailable(format!("the terminal didn't take the input: {err}"))),
            Err(err) => Err(SandboxError::Unavailable(format!("the terminal didn't take the input: {err}"))),
        }
    }

    async fn resize(&self, size: TerminalSize) -> Result<(), SandboxError> {
        self.live()?;
        let master = self.master.lock().map_err(|_| SandboxError::Unavailable("the terminal broke".into()))?;
        master.resize(pty_size(size)).map_err(|err| SandboxError::Unavailable(format!("resize: {err}")))
    }

    async fn close(&self) -> Result<(), SandboxError> {
        self.leave_slot();
        if !*self.done.borrow() {
            let (pid, why) = (self.pid, Arc::clone(&self.why));
            let _ = tokio::task::spawn_blocking(move || hang_up(pid, &why, "it was closed")).await;
            let mut done = self.done.clone();
            let _ = tokio::time::timeout(PATIENCE, done.wait_for(|done| *done)).await;
        }
        Ok(())
    }
}

impl Drop for HostTerminal {
    /// Synchronous, like the jobs' groups: when the app quits, no task runs again.
    fn drop(&mut self) {
        self.leave_slot();
        if !*self.done.borrow() {
            hang_up(self.pid, &self.why, "it was closed");
        }
    }
}

/// The shell to run: the user's `$SHELL` as the harness passes it, else `/bin/sh`.
pub(crate) fn shell_or_default(shell: Option<&PathBuf>) -> PathBuf {
    shell.cloned().unwrap_or_else(|| PathBuf::from("/bin/sh"))
}
