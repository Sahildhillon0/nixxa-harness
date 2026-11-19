//! Shared helpers for the tests that run the real sandbox (Linux only).

#![allow(dead_code)] // Each test binary uses a different part.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use nixxa_contracts::{ActionId, Observation, Outcome, Sandbox, SandboxAction, SandboxError};
use nixxa_sandbox::bwrap::{BwrapConfig, BwrapSandbox};

/// A directory tree under the system temp dir, removed on drop.
pub struct Scratch {
    pub base: PathBuf,
}

impl Scratch {
    pub fn new(tag: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let base = std::env::temp_dir().join(format!("nixxa-bwrap-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("ws")).expect("scratch workspace");
        Self { base }
    }

    pub fn workspace(&self) -> PathBuf {
        self.base.join("ws")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

/// Test defaults: the real daemon binary, `PATH` and `XDG_RUNTIME_DIR` from the test's own
/// environment, and limits small enough to hit quickly.
pub fn config(workspace: &Path) -> BwrapConfig {
    let mut config = BwrapConfig::new(workspace, env!("CARGO_BIN_EXE_nixxa-sandboxd"));
    if let Some(path) = std::env::var_os("PATH") {
        config.search_path = std::env::split_paths(&path).collect();
    }
    config.runtime_dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
    config.default_timeout = Duration::from_secs(20);
    config.memory_max = 256 * 1024 * 1024;
    config.data_max = 1024 * 1024 * 1024;
    config.pids_max = 64;
    config
}

/// The sandbox for `workspace`, or `None` (after saying so loudly) when bubblewrap is not
/// installed. Any other start-up problem fails the test.
pub fn sandbox(workspace: &Path) -> Option<BwrapSandbox> {
    sandbox_with(config(workspace))
}

pub fn sandbox_with(config: BwrapConfig) -> Option<BwrapSandbox> {
    match BwrapSandbox::new(config) {
        Ok(sandbox) => Some(sandbox),
        Err(SandboxError::Unavailable(why)) if why.contains("not installed") => {
            eprintln!();
            eprintln!("==================================================================");
            eprintln!("SKIPPED: bubblewrap is not installed, so the real sandbox was NOT");
            eprintln!("tested. Install `bwrap` to run the escape tests. ({why})");
            eprintln!("==================================================================");
            None
        }
        Err(err) => panic!("the sandbox could not be set up: {err}"),
    }
}

static IDS: AtomicU32 = AtomicU32::new(0);

pub fn next_id() -> ActionId {
    ActionId::new(format!("test-{}", IDS.fetch_add(1, Ordering::SeqCst)))
}

pub async fn run(sandbox: &BwrapSandbox, action: SandboxAction) -> Result<Observation, SandboxError> {
    sandbox.execute(next_id(), action).await
}

pub fn exec(command: impl Into<String>) -> SandboxAction {
    SandboxAction::Exec { command: command.into(), cwd: None, timeout_ms: Some(20_000) }
}

/// Runs a command that must complete, returning (exit code, stdout, stderr).
pub async fn sh(sandbox: &BwrapSandbox, command: &str) -> (Option<i32>, String, String) {
    match run(sandbox, exec(command)).await.expect("exec").outcome {
        Outcome::Exec { exit_code, stdout, stderr } => (exit_code, stdout, stderr),
        other => panic!("`{command}`: expected an Exec outcome, got {other:?}"),
    }
}

pub async fn sh_out(sandbox: &BwrapSandbox, command: &str) -> String {
    let (_, stdout, stderr) = sh(sandbox, command).await;
    format!("{stdout}{stderr}")
}

pub fn is_failed(obs: &Observation) -> bool {
    matches!(obs.outcome, Outcome::Failed { .. } | Outcome::Denied { .. })
}
