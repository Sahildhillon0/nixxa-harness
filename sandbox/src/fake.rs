//! Fakes for the sandbox contract.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use futures::stream::BoxStream;
use tokio::sync::broadcast;

use nixxa_contracts::{
    ActionId, JobId, NetworkAccess, NetworkPolicy, Observation, Outcome, Sandbox, SandboxAction, SandboxActionKind,
    SandboxCapabilities, SandboxError, SandboxEvent,
};

use crate::job::{End, Table};

/// An in-memory fake sandbox. It never touches the host filesystem, network or environment.
///
/// Commands don't run: `Exec` answers with the command line as its output. A background job
/// ends at once the same way, with the code of a final `exit N` (after `;` or `&&`, else 0) as its
/// exit code, so a demo, and the contract's scenarios, can tell jobs apart.
#[derive(Debug)]
pub struct FakeSandbox {
    files: Mutex<HashMap<String, String>>,
    jobs: Mutex<Table>,
    events: broadcast::Sender<SandboxEvent>,
    /// The policy set, reported back; the fake has no network to apply it to.
    network: Mutex<NetworkPolicy>,
}

impl Default for FakeSandbox {
    fn default() -> Self {
        Self {
            files: Mutex::default(),
            jobs: Mutex::new(Table::new(4, 64 * 1024, "")),
            events: broadcast::channel(crate::events::CAPACITY).0,
            network: Mutex::default(),
        }
    }
}

/// The code of a final `exit N` in `command`, else 0.
fn final_exit_code(command: &str) -> i32 {
    command
        .rsplit([';', '&'])
        .next()
        .and_then(|last| last.trim().strip_prefix("exit ")?.trim().parse().ok())
        .unwrap_or(0)
}

impl FakeSandbox {
    fn start_job(&self, command: String) -> Outcome {
        let Ok(mut jobs) = self.jobs.lock() else { return Outcome::Failed { message: "jobs unavailable".into() } };
        let (job, handles) = match jobs.add() {
            Ok(added) => added,
            Err(message) => return Outcome::Failed { message },
        };
        if let Ok(mut stdout) = job.stdout.lock() {
            stdout.push(command.as_bytes());
        }
        let event = job.finish(handles.done, End::Exited(Some(final_exit_code(&command))), Duration::ZERO);
        let _ = self.events.send(event);
        Outcome::Started { job: job.id.clone() }
    }

    /// Answers `JobStatus` / `JobStop` (`tail_bytes: None`) or `JobOutput`. Fake jobs have
    /// always ended, so stopping one only reports how.
    fn job_action(&self, job: &JobId, tail_bytes: Option<u64>) -> Outcome {
        match self.jobs.lock() {
            Ok(jobs) => jobs.answer(job, tail_bytes),
            Err(_) => Outcome::Failed { message: "jobs unavailable".into() },
        }
    }

    /// Rejects paths that escape the workspace.
    fn confine(&self, path: &str) -> Option<String> {
        if path.starts_with('/') || path.split('/').any(|c| c == "..") {
            return None;
        }
        Some(path.trim_start_matches("./").trim_end_matches('/').to_owned())
    }
}

#[async_trait]
impl Sandbox for FakeSandbox {
    fn capabilities(&self) -> SandboxCapabilities {
        SandboxCapabilities {
            backend: "fake".into(),
            isolated: false,
            network: NetworkAccess::Controlled(self.network.lock().map(|p| p.clone()).unwrap_or_default()),
            jobs: true,
            actions: SandboxActionKind::ALL.to_vec(),
        }
    }

    async fn execute(&self, id: ActionId, action: SandboxAction) -> Result<Observation, SandboxError> {
        let path = match action {
            SandboxAction::Exec { command, background: true, .. } => {
                return Ok(Observation::new(id, self.start_job(command)));
            }
            SandboxAction::Exec { command, .. } => {
                let outcome = Outcome::Exec { exit_code: Some(0), stdout: command, stderr: String::new() };
                return Ok(Observation::new(id, outcome));
            }
            SandboxAction::JobStatus { job } | SandboxAction::JobStop { job } => {
                return Ok(Observation::new(id, self.job_action(&job, None)));
            }
            SandboxAction::JobOutput { job, tail_bytes } => {
                return Ok(Observation::new(id, self.job_action(&job, Some(tail_bytes))));
            }
            SandboxAction::ReadFile { ref path }
            | SandboxAction::WriteFile { ref path, .. }
            | SandboxAction::ListDir { ref path } => path.clone(),
        };
        let Some(path) = self.confine(&path) else {
            return Ok(Observation::failed(id, "path is outside the workspace"));
        };
        let mut files = self.files.lock().unwrap();
        let outcome = match action {
            SandboxAction::ReadFile { .. } => match files.get(&path) {
                Some(content) => Outcome::Text { content: content.clone() },
                None => Outcome::Failed { message: "no such file".into() },
            },
            SandboxAction::WriteFile { content, .. } => {
                let bytes = content.len() as u64;
                files.insert(path, content);
                Outcome::Written { bytes }
            }
            SandboxAction::ListDir { .. } => {
                let prefix = if path.is_empty() || path == "." { String::new() } else { format!("{path}/") };
                let mut entries: Vec<nixxa_contracts::DirEntry> = Vec::new();
                for (p, content) in files.iter() {
                    if let Some(rest) = p.strip_prefix(&prefix) {
                        let (name, is_dir) = match rest.split_once('/') {
                            Some((d, _)) => (d.to_owned(), true),
                            None => (rest.to_owned(), false),
                        };
                        if !entries.iter().any(|e| e.name == name) {
                            entries.push(nixxa_contracts::DirEntry {
                                name,
                                is_dir,
                                size: (!is_dir).then_some(content.len() as u64),
                            });
                        }
                    }
                }
                Outcome::Listing { entries }
            }
            _ => unreachable!("answered above"),
        };
        Ok(Observation::new(id, outcome))
    }

    async fn health(&self) -> Result<(), SandboxError> {
        Ok(())
    }

    async fn reset(&self) -> Result<(), SandboxError> {
        // Fake jobs end at once, so there is nothing to kill. The workspace is kept.
        if let Ok(jobs) = self.jobs.lock() {
            jobs.stop_all();
        }
        Ok(())
    }

    async fn set_network(&self, policy: NetworkPolicy) -> Result<(), SandboxError> {
        if let Ok(mut network) = self.network.lock() {
            *network = policy;
        }
        Ok(())
    }

    fn subscribe(&self) -> BoxStream<'static, SandboxEvent> {
        crate::events::stream(self.events.subscribe())
    }
}

#[cfg(test)]
mod tests {
    use nixxa_contracts::conformance::sandbox::run;
    use nixxa_contracts::{ActionId, Outcome, Sandbox, SandboxAction};

    use super::FakeSandbox;

    #[tokio::test]
    async fn fake_sandbox_conforms() {
        run(FakeSandbox::default).await;
    }

    #[tokio::test]
    async fn reset_keeps_the_workspace() {
        let sandbox = FakeSandbox::default();
        let id = ActionId::new("unit-reset");
        sandbox
            .execute(id.clone(), SandboxAction::WriteFile { path: "keep.txt".into(), content: "kept".into() })
            .await
            .expect("write");
        sandbox.reset().await.expect("reset");
        let read = sandbox.execute(id, SandboxAction::ReadFile { path: "keep.txt".into() }).await.expect("read");
        assert!(matches!(read.outcome, Outcome::Text { .. }), "reset must keep the workspace");
    }

    #[test]
    fn a_final_exit_sets_the_code() {
        assert_eq!(super::final_exit_code("echo hi; exit 3"), 3);
        assert_eq!(super::final_exit_code("exit 7"), 7);
        assert_eq!(super::final_exit_code("echo hi && exit 4"), 4);
        assert_eq!(super::final_exit_code("cargo build"), 0);
    }
}
