//! The user's terminal (D-066): a PTY job.
//!
//! The daemon opens a PTY (`/dev/ptmx`, in the sandbox's own `devpts`), and starts the shell
//! under a supervisor like any job: the same rlimits, cgroup scope and process-tree tracking,
//! so closing it kills exactly its tree. The supervisor makes the PTY the shell's controlling
//! terminal (see [`crate::exec`]). It isn't one of the agent's jobs: it has no time limit or
//! output cap, isn't in the job table, sends no `JobFinished`, and only `close`, a reset or the
//! daemon ending stop it.
//!
//! Output is read from the PTY at most [`WINDOW`] bytes ahead of the client's
//! acknowledgements; past that the reader waits, the PTY's buffer fills, and the shell's writes
//! block. Nothing is dropped and nothing piles up in memory.

use std::os::fd::OwnedFd;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rustix::io::Errno;
use rustix::pty::OpenptFlags;
use tokio::net::unix::pipe;
use tokio::sync::{Semaphore, mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use nixxa_contracts::TerminalSize;

use crate::job::End;
use crate::jobs::{self, Capture, Spec, Supervisors};
use crate::protocol::{FromDaemon, Reply};

/// Bytes of output sent ahead of the client's acknowledgements.
const WINDOW: usize = 64 * 1024;
/// The most output read from the PTY at once.
const CHUNK: u32 = 16 * 1024;
/// How long the output still in the PTY may take to be sent once the shell's tree is gone.
const DRAIN: Duration = Duration::from_secs(2);
/// How long to wait before reading again while no process has the terminal open yet.
const NOT_OPEN_YET: Duration = Duration::from_millis(20);

/// What the daemon passes on to a terminal it opens.
pub(crate) struct Setup {
    /// This binary, to run as the supervisor.
    pub self_exe: String,
    /// The rlimits of every process in the terminal.
    pub rlimits: crate::protocol::Rlimits,
    /// The environment beyond the daemon's own: the network proxy's variables, as commands get
    /// them now.
    pub env: Vec<(String, String)>,
}

/// An open terminal.
pub(crate) struct Terminal {
    /// The client's id for it.
    pub id: u64,
    /// The PTY's master side, for resizing (reading and writing have their own copies).
    master: OwnedFd,
    /// Input to write, with the request to answer once written.
    input: mpsc::UnboundedSender<(u64, Vec<u8>)>,
    /// Output bytes the daemon may still send ahead of the client.
    credit: Arc<Semaphore>,
    stop: Option<oneshot::Sender<()>>,
    /// Why it was stopped, for [`FromDaemon::TerminalClosed`].
    why: Arc<Mutex<Option<String>>>,
    /// Ends once `TerminalClosed` has been sent.
    task: JoinHandle<()>,
}

/// The shell: bash if the system has it, else `sh`.
fn shell() -> &'static str {
    if std::path::Path::new("/bin/bash").exists() { "/bin/bash" } else { "/bin/sh" }
}

/// The variables the terminal's shell gets beyond the commands' own.
fn terminal_env(mut env: Vec<(String, String)>) -> Vec<(String, String)> {
    env.push(("TERM".into(), "xterm-256color".into()));
    // A home of its own: a login shell sources start-up files from it and writes its history
    // there, and the commands' home (`/tmp`) is the agent's to write and read.
    env.push(("HOME".into(), crate::landlock::TERMINAL_HOME.into()));
    // Says where the shell is, in every prompt.
    env.push(("PS1".into(), "sandbox:\\w\\$ ".into()));
    env
}

impl Terminal {
    /// Opens a terminal of `size` and starts its shell in `/workspace`.
    pub(crate) fn open(
        id: u64,
        size: TerminalSize,
        setup: Setup,
        supervisors: &Supervisors,
        out: &mpsc::UnboundedSender<FromDaemon>,
    ) -> Result<Self, String> {
        let master = rustix::pty::openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC)
            .map_err(|err| format!("no terminal device: {err}"))?;
        rustix::pty::grantpt(&master).map_err(|err| format!("grantpt: {err}"))?;
        rustix::pty::unlockpt(&master).map_err(|err| format!("unlockpt: {err}"))?;
        let path = rustix::pty::ptsname(&master, Vec::new()).map_err(|err| format!("ptsname: {err}"))?;
        let path = path.to_string_lossy().into_owned();
        rustix::termios::tcsetwinsize(&master, winsize(size)).map_err(|err| format!("set the size: {err}"))?;
        rustix::io::ioctl_fionbio(&master, true).map_err(|err| format!("non-blocking: {err}"))?;
        // A PTY polls like a pipe: readable when the shell wrote, writable when it has room.
        let copy = || master.try_clone().map_err(|err| format!("dup: {err}"));
        let output = pipe::Receiver::from_owned_fd_unchecked(copy()?).map_err(|err| format!("poll: {err}"))?;
        let input_end = pipe::Sender::from_owned_fd_unchecked(copy()?).map_err(|err| format!("poll: {err}"))?;

        let spec = Spec {
            command: shell().into(),
            cwd: "/workspace".into(),
            limit: None,
            rlimits: setup.rlimits,
            self_exe: setup.self_exe,
            owner: "terminal".into(),
            env: terminal_env(setup.env),
            confine: false,
            pty: Some(path),
        };
        let credit = Arc::new(Semaphore::new(WINDOW));
        let (input, inbox) = mpsc::unbounded_channel();
        let (stop, stopped) = oneshot::channel();
        let why = Arc::new(Mutex::new(None));
        let (ended_tx, ended) = watch::channel(false);
        let reader = tokio::spawn(read_output(output, id, Arc::clone(&credit), out.clone(), ended.clone()));
        let writer = tokio::spawn(write_input(input_end, inbox, out.clone(), ended));
        let (supervisors, out, reason) = (supervisors.clone(), out.clone(), Arc::clone(&why));
        let task = tokio::spawn(async move {
            let ran = jobs::run(spec, Capture::Nothing, &supervisors, stopped).await;
            // Every process of the terminal is gone: send what is left of its output.
            let _ = ended_tx.send(true);
            let reader_abort = reader.abort_handle();
            if tokio::time::timeout(DRAIN, reader).await.is_err() {
                reader_abort.abort();
            }
            let _ = writer.await;
            let reason = match ran.end {
                End::Exited(Some(0)) => "the shell exited".to_string(),
                End::Exited(Some(code)) => format!("the shell exited with code {code}"),
                End::Exited(None) => "the shell was killed".to_string(),
                End::Stopped => {
                    reason.lock().ok().and_then(|mut why| why.take()).unwrap_or_else(|| "it was closed".into())
                }
                End::TimedOut => "it ran out of time".to_string(),
                End::Failed(why) => format!("the shell couldn't start: {why}"),
            };
            eprintln!("nixxa-sandboxd: terminal {id} closed ms={}", ran.duration.as_millis());
            let _ = out.send(FromDaemon::TerminalClosed { terminal: id, reason });
        });
        Ok(Self { id, master, input, credit, stop: Some(stop), why, task })
    }

    /// Queues input; `request` is answered once it is written.
    pub(crate) fn write(&self, request: u64, data: Vec<u8>) -> Result<(), ()> {
        self.input.send((request, data)).map_err(|_| ())
    }

    pub(crate) fn resize(&self, size: TerminalSize) -> Result<(), String> {
        rustix::termios::tcsetwinsize(&self.master, winsize(size)).map_err(|err| err.to_string())
    }

    /// The client consumed `bytes` of output (the client is the host: trusted to count right).
    pub(crate) fn acknowledge(&self, bytes: u32) {
        self.credit.add_permits(bytes as usize);
    }

    /// Whether it has ended.
    pub(crate) fn is_finished(&self) -> bool {
        self.task.is_finished()
    }

    /// Ends the shell and its whole tree; `why` goes into `TerminalClosed`. Returns once that
    /// has been sent.
    pub(crate) async fn close(mut self, why: &str) {
        if let Ok(mut reason) = self.why.lock() {
            reason.get_or_insert_with(|| why.to_string());
        }
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        let _ = self.task.await;
    }
}

fn winsize(size: TerminalSize) -> rustix::termios::Winsize {
    rustix::termios::Winsize { ws_row: size.rows, ws_col: size.cols, ws_xpixel: 0, ws_ypixel: 0 }
}

/// Sends the PTY's output to the client, at most the unacknowledged window ahead.
async fn read_output(
    master: pipe::Receiver,
    terminal: u64,
    credit: Arc<Semaphore>,
    out: mpsc::UnboundedSender<FromDaemon>,
    ended: watch::Receiver<bool>,
) {
    let mut buf = vec![0u8; CHUNK as usize];
    loop {
        let Ok(permit) = credit.acquire_many(CHUNK).await else { return };
        permit.forget();
        if master.readable().await.is_err() {
            return;
        }
        let n = match master.try_read(&mut buf) {
            Ok(n) if n > 0 => n,
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                credit.add_permits(CHUNK as usize);
                continue;
            }
            // EIO: no process has the terminal open, before the shell has opened it or after the
            // tree is gone.
            Err(err) if err.raw_os_error() == Some(Errno::IO.raw_os_error()) && !*ended.borrow() => {
                credit.add_permits(CHUNK as usize);
                tokio::time::sleep(NOT_OPEN_YET).await;
                continue;
            }
            _ => return,
        };
        credit.add_permits((CHUNK as usize) - n);
        if out.send(FromDaemon::TerminalOutput { terminal, data: buf[..n].to_vec() }).is_err() {
            return;
        }
    }
}

/// Writes queued input to the PTY and answers each request; once the terminal has ended,
/// answers what is left with an error.
async fn write_input(
    master: pipe::Sender,
    mut inbox: mpsc::UnboundedReceiver<(u64, Vec<u8>)>,
    out: mpsc::UnboundedSender<FromDaemon>,
    mut ended: watch::Receiver<bool>,
) {
    let answer = |id: u64, reply: Reply| {
        let _ = out.send(FromDaemon::Reply { id, reply });
    };
    let gone = || Reply::Error { message: "the terminal has ended".into() };
    loop {
        let (id, data) = tokio::select! {
            next = inbox.recv() => match next {
                Some(next) => next,
                None => return,
            },
            _ = ended.wait_for(|ended| *ended) => break,
        };
        let written = tokio::select! {
            written = write_all(&master, &data) => written,
            _ = ended.wait_for(|ended| *ended) => Err("the terminal has ended".to_string()),
        };
        match written {
            Ok(()) => answer(id, Reply::Done),
            Err(message) => answer(id, Reply::Error { message }),
        }
    }
    inbox.close();
    while let Ok((id, _)) = inbox.try_recv() {
        answer(id, gone());
    }
}

async fn write_all(master: &pipe::Sender, mut data: &[u8]) -> Result<(), String> {
    while !data.is_empty() {
        master.writable().await.map_err(|err| err.to_string())?;
        match master.try_write(data) {
            Ok(n) => data = &data[n..],
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(err) => return Err(format!("the terminal didn't take the input: {err}")),
        }
    }
    Ok(())
}
