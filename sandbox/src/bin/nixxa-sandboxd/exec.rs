//! `--limited` mode: the supervisor at the top of every job.
//!
//! The daemon starts each job as `nixxa-sandboxd --limited <spec> -- <command>`. That process
//! applies the rlimits to itself, makes itself a *child subreaper*, starts the job's shell, and
//! then only reaps. Because it is a subreaper, anything the job orphans (a double fork, a
//! `setsid` daemon) is re-parented to it instead of escaping, so the job is exactly "the
//! supervisor and its descendants" for as long as the supervisor lives.
//!
//! The supervisor's stdin is one end of a socket pair the daemon holds the other end of. When
//! the shell exits, the supervisor writes `exit <code>` or `signal <n>` there, so the daemon
//! knows the command finished while the supervisor stays alive to keep the leftovers
//! attributable. It never touches the job's output: it hands its stdout and stderr to the
//! shell and points its own at `/dev/null`, so the pipes close when the job's last process does.
//!
//! The agent's commands get a second Landlock layer here, before their shell starts
//! ([`crate::landlock::confine_job`]). The user's terminal doesn't: the supervisor instead
//! starts a new session, makes the terminal (a PTY the daemon opened) its controlling
//! terminal, and runs an interactive shell on it.

use std::fs::OpenOptions;
use std::os::fd::{AsFd, OwnedFd};
use std::process::{Command, Stdio};

use rustix::io::Errno;
use rustix::process::{Resource, Rlimit, WaitOptions, setrlimit};
use serde::{Deserialize, Serialize};

use crate::protocol::Rlimits;

/// What the daemon tells a supervisor, as JSON on its command line.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Supervise {
    /// The rlimits of every process of the job.
    pub rlimits: Rlimits,
    /// Add the agent's Landlock layer (the daemon's own layer is enforced).
    pub confine: bool,
    /// For the user's terminal: the PTY to run the shell on, e.g. `/dev/pts/0`. The command is
    /// then the shell itself, run as a login shell.
    pub pty: Option<String>,
}

/// Runs the supervisor. Returns the process's exit code.
pub(crate) fn limited(spec: &str, command: &str) -> i32 {
    let spec: Supervise = match serde_json::from_str(spec) {
        Ok(spec) => spec,
        Err(err) => {
            eprintln!("nixxa-sandboxd: bad supervisor spec: {err}");
            return 126;
        }
    };
    let limits = spec.rlimits;
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
    if let Err(err) = rustix::process::set_child_subreaper(Some(rustix::process::getpid())) {
        eprintln!("nixxa-sandboxd: cannot become the job's subreaper: {err}");
        return 126;
    }
    // Never less isolated than asked: a job that can't be confined doesn't run.
    if spec.confine {
        if let Err(why) = crate::landlock::confine_job() {
            eprintln!("nixxa-sandboxd: cannot confine the job: {why}");
            return 126;
        }
    }
    let started = match &spec.pty {
        Some(pty) => shell_on(pty, command),
        None => Command::new("/bin/sh").arg("-c").arg(command).stdin(Stdio::null()).spawn().map_err(|err| {
            eprintln!("nixxa-sandboxd: could not run /bin/sh: {err}");
            127
        }),
    };
    let shell = match started {
        Ok(shell) => shell,
        Err(code) => return code,
    };
    // Let go of the command's output, so its pipes close with the job's last process.
    match OpenOptions::new().write(true).open("/dev/null") {
        Ok(null) => {
            let _ = rustix::stdio::dup2_stdout(&null);
            let _ = rustix::stdio::dup2_stderr(&null);
        }
        Err(_) => return 126,
    }
    let shell_pid = shell.id() as i32;
    let report = loop {
        match rustix::process::wait(WaitOptions::empty()) {
            Ok(Some((pid, status))) if pid.as_raw_nonzero().get() == shell_pid => {
                break match (status.exit_status(), status.terminating_signal()) {
                    (Some(code), _) => format!("exit {code}\n"),
                    (None, Some(signal)) => format!("signal {signal}\n"),
                    (None, None) => "signal 0\n".to_string(),
                };
            }
            // A leftover of the command, reaped.
            Ok(_) | Err(Errno::INTR) => continue,
            Err(_) => break "signal 0\n".to_string(),
        }
    };
    let _ = rustix::io::write(std::io::stdin().as_fd(), report.as_bytes());
    // Keep reaping whatever the command left behind until it is gone or the daemon ends the job.
    loop {
        match rustix::process::wait(WaitOptions::empty()) {
            Ok(Some(_)) | Err(Errno::INTR) => continue,
            _ => return 0,
        }
    }
}

/// Starts `shell` as an interactive login shell with the terminal `pty` as its controlling
/// terminal: the supervisor leads a new session (it is not a process-group leader, so it may),
/// takes the terminal, and passes it to the shell as stdin, stdout and stderr. The shell then
/// runs job control on it (its own process groups, `ctrl+c`, `SIGWINCH`).
fn shell_on(pty: &str, shell: &str) -> Result<std::process::Child, i32> {
    let fail = |what: &str, err: &dyn std::fmt::Display| {
        eprintln!("nixxa-sandboxd: the terminal: {what}: {err}");
        126
    };
    rustix::process::setsid().map_err(|err| fail("setsid", &err))?;
    let terminal: OwnedFd =
        OpenOptions::new().read(true).write(true).open(pty).map_err(|err| fail("open", &err))?.into();
    rustix::process::ioctl_tiocsctty(&terminal).map_err(|err| fail("take the terminal", &err))?;
    let io = |fd: &OwnedFd| fd.try_clone().map(Stdio::from).map_err(|err| fail("dup", &err));
    let (stdin, stdout, stderr) = (io(&terminal)?, io(&terminal)?, io(&terminal)?);
    // The supervisor keeps no copy: the terminal closes when the job's last process does.
    drop(terminal);
    Command::new(shell).arg("-l").stdin(stdin).stdout(stdout).stderr(stderr).spawn().map_err(|err| {
        eprintln!("nixxa-sandboxd: could not run {shell}: {err}");
        127
    })
}
