//! `--limited` mode: the supervisor at the top of every job.
//!
//! The daemon starts each command as `nixxa-sandboxd --limited <rlimits> -- <command>`. That
//! process applies the rlimits to itself, makes itself a *child subreaper*, starts
//! `/bin/sh -c <command>`, and then only reaps. Because it is a subreaper, anything the command
//! orphans (a double fork, a `setsid` daemon) is re-parented to it instead of escaping, so the
//! job is exactly "the supervisor and its descendants" for as long as the supervisor lives.
//!
//! The supervisor's stdin is one end of a socket pair the daemon holds the other end of. When
//! the shell exits, the supervisor writes `exit <code>` or `signal <n>` there, so the daemon
//! knows the command finished while the supervisor stays alive to keep the leftovers
//! attributable. It never touches the command's output: it hands its stdout and stderr to the
//! shell and points its own at `/dev/null`, so the pipes close when the job's last process does.

use std::fs::OpenOptions;
use std::os::fd::AsFd;
use std::process::{Command, Stdio};

use rustix::io::Errno;
use rustix::process::{Resource, Rlimit, WaitOptions, setrlimit};

use crate::protocol::Rlimits;

/// Runs the supervisor. Returns the process's exit code.
pub(crate) fn limited(rlimits: &str, command: &str) -> i32 {
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
    if let Err(err) = rustix::process::set_child_subreaper(Some(rustix::process::getpid())) {
        eprintln!("nixxa-sandboxd: cannot become the job's subreaper: {err}");
        return 126;
    }
    let shell = match Command::new("/bin/sh").arg("-c").arg(command).stdin(Stdio::null()).spawn() {
        Ok(shell) => shell,
        Err(err) => {
            eprintln!("nixxa-sandboxd: could not run /bin/sh: {err}");
            return 127;
        }
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
