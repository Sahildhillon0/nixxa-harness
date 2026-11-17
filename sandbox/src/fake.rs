//! Fakes for the sandbox contract.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;

use nixxa_contracts::{
    ActionId, Observation, Outcome, Sandbox, SandboxAction, SandboxActionKind, SandboxCapabilities, SandboxError,
};

/// An in-memory fake sandbox. It never touches the host filesystem, network or environment.
#[derive(Debug, Default)]
pub struct FakeSandbox {
    files: Mutex<HashMap<String, String>>,
}

impl FakeSandbox {
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
            network: false,
            actions: vec![
                SandboxActionKind::Exec,
                SandboxActionKind::ReadFile,
                SandboxActionKind::WriteFile,
                SandboxActionKind::ListDir,
            ],
        }
    }

    async fn execute(&self, id: ActionId, action: SandboxAction) -> Result<Observation, SandboxError> {
        if let SandboxAction::Exec { command, .. } = &action {
            let stdout = command.clone();
            return Ok(Observation::new(id, Outcome::Exec { exit_code: Some(0), stdout, stderr: String::new() }));
        }
        let path = match &action {
            SandboxAction::Exec { .. } => unreachable!(),
            SandboxAction::ReadFile { path }
            | SandboxAction::WriteFile { path, .. }
            | SandboxAction::ListDir { path } => path.clone(),
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
            SandboxAction::Exec { .. } => unreachable!(),
        };
        Ok(Observation::new(id, outcome))
    }

    async fn health(&self) -> Result<(), SandboxError> {
        Ok(())
    }

    async fn reset(&self) -> Result<(), SandboxError> {
        self.files.lock().unwrap().clear();
        Ok(())
    }
}

#[cfg(all(test, feature = "conformance"))]
mod tests {
    use nixxa_contracts::conformance::sandbox::run;

    use super::FakeSandbox;

    #[tokio::test]
    async fn fake_sandbox_conforms() {
        run(FakeSandbox::default).await;
    }
}
