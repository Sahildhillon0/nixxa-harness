//! `nixxa-sandbox` — executes actions and returns observations.
//!
//! Backends: [`fake::FakeSandbox`] (in memory, for `--fake` and tests), [`host::HostSandbox`]
//! (the real files of one directory, not isolated, for "This directory") and, on Linux,
//! [`bwrap::BwrapSandbox`] (`nixxa-sandboxd` out of process under bubblewrap). All three run
//! background jobs under [`JobLimits`], and open the user's terminal (`ctrl+t`) in the same
//! place as the agent's commands: a PTY job in the daemon, the user's shell on the host (Unix),
//! a scripted one in the fake.

#[cfg(target_os = "linux")]
pub mod bwrap;
pub mod checkpoint;
mod events;
pub mod fake;
pub mod host;
#[cfg(unix)]
mod host_terminal;
mod job;
pub mod network;
mod patch;
#[cfg(target_os = "linux")]
mod protocol;
#[cfg(target_os = "linux")]
mod proxy;

use std::time::Duration;

/// Limits on background jobs, for every backend that runs them. The harness sets them; the
/// defaults are 30 minutes per job (2 hours at most), the last 1 MiB of each output stream, and
/// 4 jobs at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JobLimits {
    /// The time limit of a job that asks for none.
    pub default_timeout: Duration,
    /// The most time any job gets, whatever it asks for.
    pub max_timeout: Duration,
    /// Bytes kept from the end of each of a job's stdout and stderr.
    pub output_cap: u64,
    /// Jobs running at once.
    pub max_running: u64,
}

impl Default for JobLimits {
    fn default() -> Self {
        Self {
            default_timeout: Duration::from_secs(30 * 60),
            max_timeout: Duration::from_secs(2 * 60 * 60),
            output_cap: 1024 * 1024,
            max_running: 4,
        }
    }
}
