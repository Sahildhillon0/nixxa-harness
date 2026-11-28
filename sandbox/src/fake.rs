//! Fakes for the sandbox contract.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::stream::BoxStream;
use tokio::sync::{broadcast, mpsc};

use nixxa_contracts::{
    ActionId, CheckpointId, EntryState, FileChange, JobId, NetworkAccess, NetworkPolicy, Observation, OpenTerminal,
    Outcome, Protection, ProtectionKind, Sandbox, SandboxAction, SandboxActionKind, SandboxCapabilities, SandboxError,
    SandboxEvent, Terminal, TerminalEvent, TerminalSize, WorkspaceDiff, glob_matches,
};

use crate::job::{End, Table};

/// An in-memory fake sandbox. It never touches the host filesystem, network or environment.
///
/// Commands don't run: `Exec` answers with the command line as its output. A background job
/// ends at once the same way, with the code of a final `exit N` (after `;` or `&&`, else 0) as its
/// exit code, so a demo, and the contract's scenarios, can tell jobs apart.
///
/// Its terminal is scripted too: it echoes what is typed and runs nothing; `stty size` answers
/// the terminal's size and `exit` ends it.
///
/// Patches and checkpoints work on its files (a checkpoint is a copy of them), so undo can be
/// shown with `--fake`.
#[derive(Debug)]
pub struct FakeSandbox {
    files: Mutex<HashMap<String, String>>,
    checkpoints: Mutex<Vec<HashMap<String, String>>>,
    jobs: Mutex<Table>,
    events: broadcast::Sender<SandboxEvent>,
    /// The policy set, reported back; the fake has no network to apply it to.
    network: Mutex<NetworkPolicy>,
    terminal: Arc<Mutex<Scripted>>,
    /// Paths file actions must not change (#5).
    protected: Mutex<crate::protect::Rules>,
}

impl Default for FakeSandbox {
    fn default() -> Self {
        Self {
            files: Mutex::default(),
            checkpoints: Mutex::default(),
            jobs: Mutex::new(Table::new(4, 64 * 1024, "")),
            events: broadcast::channel(crate::events::CAPACITY).0,
            network: Mutex::default(),
            terminal: Arc::default(),
            protected: Mutex::default(),
        }
    }
}

/// The scripted terminal's prompt.
const PROMPT: &str = "fake$ ";

/// The scripted terminal's state.
#[derive(Debug, Default)]
struct Scripted {
    /// The open terminal: its number, size and event channel.
    open: Option<(u64, TerminalSize, mpsc::UnboundedSender<TerminalEvent>)>,
    opened: u64,
    line: String,
    /// Inside an escape sequence (an arrow key), which is ignored.
    escape: bool,
}

impl Scripted {
    fn send(&self, text: &str) {
        if let Some((.., tx)) = &self.open {
            let _ = tx.send(TerminalEvent::Output(text.as_bytes().to_vec()));
        }
    }

    /// Ends terminal `number` if it is the open one.
    fn end(&mut self, number: u64, reason: &str) {
        if self.open.as_ref().is_some_and(|(open, ..)| *open == number) {
            if let Some((.., tx)) = self.open.take() {
                let _ = tx.send(TerminalEvent::Closed { reason: reason.into() });
            }
        }
    }

    /// Takes typed input: echoes it, and runs `stty size` and `exit` at the end of a line.
    fn take(&mut self, number: u64, input: &[u8]) {
        for c in String::from_utf8_lossy(input).chars() {
            if self.open.as_ref().is_none_or(|(open, ..)| *open != number) {
                return;
            }
            match c {
                '\x1b' => self.escape = true,
                c if self.escape => self.escape = !c.is_ascii_alphabetic() && c != '~',
                '\r' | '\n' => {
                    self.send("\r\n");
                    let line = std::mem::take(&mut self.line);
                    match line.trim() {
                        "" => {}
                        "exit" => return self.end(number, "the shell exited"),
                        "stty size" => {
                            let size = self.open.as_ref().map(|(_, size, _)| *size);
                            if let Some(size) = size {
                                self.send(&format!("{} {}\r\n", size.rows, size.cols));
                            }
                        }
                        _ => self.send("(the demo's terminal runs nothing)\r\n"),
                    }
                    self.send(PROMPT);
                }
                '\x7f' | '\x08' => {
                    if self.line.pop().is_some() {
                        self.send("\x08 \x08");
                    }
                }
                '\x03' => {
                    self.line.clear();
                    self.send(&format!("^C\r\n{PROMPT}"));
                }
                '\x04' if self.line.is_empty() => return self.end(number, "the shell exited"),
                c if c.is_control() => {}
                c => {
                    self.line.push(c);
                    self.send(c.encode_utf8(&mut [0; 4]));
                }
            }
        }
    }
}

/// The scripted terminal's handle.
struct ScriptedTerminal {
    state: Arc<Mutex<Scripted>>,
    number: u64,
}

impl ScriptedTerminal {
    fn live(&self) -> Result<std::sync::MutexGuard<'_, Scripted>, SandboxError> {
        let state = self.state.lock().map_err(|_| SandboxError::Unavailable("the terminal broke".into()))?;
        if state.open.as_ref().is_some_and(|(open, ..)| *open == self.number) {
            Ok(state)
        } else {
            Err(SandboxError::Unavailable("the terminal has ended".into()))
        }
    }
}

#[async_trait]
impl Terminal for ScriptedTerminal {
    async fn write(&self, input: &[u8]) -> Result<(), SandboxError> {
        self.live()?.take(self.number, input);
        Ok(())
    }

    async fn resize(&self, size: TerminalSize) -> Result<(), SandboxError> {
        if let Some((_, current, _)) = self.live()?.open.as_mut() {
            *current = size;
        }
        Ok(())
    }

    async fn close(&self) -> Result<(), SandboxError> {
        if let Ok(mut state) = self.state.lock() {
            state.end(self.number, "it was closed");
        }
        Ok(())
    }
}

impl Drop for ScriptedTerminal {
    fn drop(&mut self) {
        if let Ok(mut state) = self.state.lock() {
            state.end(self.number, "it was closed");
        }
    }
}

/// What differs between two sets of the fake's files.
fn diff_files(before: &HashMap<String, String>, after: &HashMap<String, String>) -> WorkspaceDiff {
    let state = |content: &String| EntryState::File {
        size: content.len() as u64,
        mode: 0o644,
        hash: crate::patch::hash(content.as_bytes()),
    };
    let mut paths: Vec<&String> = before.keys().chain(after.keys()).collect();
    paths.sort();
    paths.dedup();
    let changes = paths
        .into_iter()
        .filter(|path| before.get(*path) != after.get(*path))
        .map(|path| {
            let (old, new) = (before.get(path), after.get(path));
            FileChange {
                path: path.clone(),
                before: old.map(state),
                after: new.map(state),
                diff: match (old, new) {
                    (Some(old), Some(new)) => Some(crate::patch::unified(path, old.as_bytes(), new.as_bytes())),
                    _ => None,
                },
            }
        })
        .collect();
    WorkspaceDiff { changes, not_covered: Vec::new() }
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

    /// The files a checkpoint recorded.
    fn taken(&self, checkpoint: &CheckpointId) -> Result<HashMap<String, String>, SandboxError> {
        let taken = self.checkpoints.lock().map_err(|_| SandboxError::Other("the fake broke".into()))?;
        checkpoint
            .as_str()
            .strip_prefix("fake.")
            .and_then(|n| n.parse::<usize>().ok())
            .and_then(|n| taken.get(n).cloned())
            .ok_or_else(|| SandboxError::NoCheckpoint(checkpoint.clone()))
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
            terminal: true,
            checkpoints: true,
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
            | SandboxAction::Patch { ref path, .. }
            | SandboxAction::ListDir { ref path } => path.clone(),
        };
        let Some(path) = self.confine(&path) else {
            return Ok(Observation::failed(id, "path is outside the workspace"));
        };
        let mut files = self.files.lock().unwrap();
        let outcome = match action {
            SandboxAction::ReadFile { .. } => match files.get(&path) {
                Some(content) => {
                    Outcome::File { content: content.clone(), hash: crate::patch::hash(content.as_bytes()) }
                }
                None => Outcome::Failed { message: "no such file".into() },
            },
            SandboxAction::WriteFile { .. } | SandboxAction::Patch { .. }
                if self.protected.lock().is_ok_and(|rules| rules.covering(&path).is_some()) =>
            {
                Outcome::Failed {
                    message: format!("`{path}` is protected by the project policy, so it can't be changed"),
                }
            }
            SandboxAction::Patch { edits, expected_hash, .. } => match files.get(&path) {
                None => Outcome::Failed { message: "no such file".into() },
                Some(old) => match crate::patch::apply(old.as_bytes(), &edits, &expected_hash) {
                    Err(message) => Outcome::Failed { message },
                    Ok(new) => {
                        let new = String::from_utf8_lossy(&new).into_owned();
                        let diff = crate::patch::unified(&path, old.as_bytes(), new.as_bytes());
                        let hash = crate::patch::hash(new.as_bytes());
                        files.insert(path, new);
                        Outcome::Patched { diff, hash }
                    }
                },
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

    async fn checkpoint(&self) -> Result<CheckpointId, SandboxError> {
        let files = self.files.lock().map_err(|_| SandboxError::Other("the fake broke".into()))?.clone();
        let mut taken = self.checkpoints.lock().map_err(|_| SandboxError::Other("the fake broke".into()))?;
        if taken.last() != Some(&files) {
            taken.push(files);
        }
        Ok(CheckpointId::new(format!("fake.{}", taken.len() - 1)))
    }

    async fn restore(&self, checkpoint: &CheckpointId) -> Result<WorkspaceDiff, SandboxError> {
        let mut target = self.taken(checkpoint)?;
        let mut files = self.files.lock().map_err(|_| SandboxError::Other("the fake broke".into()))?;
        let rules = self.protected.lock().map_err(|_| SandboxError::Other("the fake broke".into()))?.clone();
        // Protected paths stay as they are.
        target.retain(|path, _| rules.covering(path).is_none());
        target.extend(
            files.iter().filter(|(path, _)| rules.covering(path).is_some()).map(|(p, c)| (p.clone(), c.clone())),
        );
        let undone = diff_files(&target, &files);
        *files = target;
        Ok(undone)
    }

    async fn diff(&self, from: &CheckpointId, to: Option<&CheckpointId>) -> Result<WorkspaceDiff, SandboxError> {
        let before = self.taken(from)?;
        let after = match to {
            Some(to) => self.taken(to)?,
            None => self.files.lock().map_err(|_| SandboxError::Other("the fake broke".into()))?.clone(),
        };
        Ok(diff_files(&before, &after))
    }

    async fn protect(&self, rules: Vec<String>) -> Result<Protection, SandboxError> {
        let files = self.files.lock().map_err(|_| SandboxError::Other("the fake broke".into()))?;
        let matching =
            |rule: &String| files.keys().filter(|path| glob_matches(rule, path)).cloned().collect::<Vec<_>>();
        let mut kept: Vec<String> = rules.iter().flat_map(matching).collect();
        kept.sort();
        let unmatched = rules.iter().filter(|rule| matching(rule).is_empty()).cloned().collect();
        *self.protected.lock().map_err(|_| SandboxError::Other("the fake broke".into()))? =
            crate::protect::Rules::new(rules);
        Ok(Protection { how: ProtectionKind::FileActions, kept, unmatched, gaps: Vec::new() })
    }

    async fn reset(&self) -> Result<(), SandboxError> {
        // Fake jobs end at once, so there is nothing to kill. The workspace is kept.
        if let Ok(jobs) = self.jobs.lock() {
            jobs.stop_all();
        }
        if let Ok(mut terminal) = self.terminal.lock() {
            let number = terminal.opened;
            terminal.end(number, "the sandbox was reset");
        }
        Ok(())
    }

    async fn open_terminal(&self, size: TerminalSize) -> Result<OpenTerminal, SandboxError> {
        let mut state = self.terminal.lock().map_err(|_| SandboxError::Unavailable("the terminal broke".into()))?;
        if state.open.is_some() {
            return Err(SandboxError::Unavailable("a terminal is already open".into()));
        }
        state.opened += 1;
        let number = state.opened;
        let (tx, rx) = mpsc::unbounded_channel();
        state.open = Some((number, size, tx));
        state.line.clear();
        state.escape = false;
        state.send(&format!(
            "The demo's scripted terminal: it echoes what you type and runs nothing (`stty size` and `exit` work).\r\n{PROMPT}"
        ));
        let events = futures::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|event| (event, rx)) });
        let terminal = ScriptedTerminal { state: Arc::clone(&self.terminal), number };
        Ok(OpenTerminal { terminal: Box::new(terminal), events: Box::pin(events) })
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
        assert!(matches!(read.outcome, Outcome::File { .. }), "reset must keep the workspace");
    }

    #[tokio::test]
    async fn the_scripted_terminal_echoes_answers_stty_and_exits() {
        use futures::StreamExt;
        use nixxa_contracts::{TerminalEvent, TerminalSize};

        let sandbox = FakeSandbox::default();
        let mut open = sandbox.open_terminal(TerminalSize { cols: 90, rows: 20 }).await.expect("open");
        assert!(sandbox.open_terminal(TerminalSize { cols: 1, rows: 1 }).await.is_err(), "one at a time");
        open.terminal.write(b"ls\x1b[A\x7fs\r").await.unwrap();
        open.terminal.write(b"stty size\rexit\r").await.unwrap();
        let mut seen = String::new();
        let mut reason = None;
        while let Some(event) = open.events.next().await {
            match event {
                TerminalEvent::Output(bytes) => seen.push_str(&String::from_utf8_lossy(&bytes)),
                TerminalEvent::Closed { reason: why } => reason = Some(why),
            }
        }
        assert!(seen.contains("ls\x08 \x08s\r\n(the demo's terminal runs nothing)"), "{seen:?}");
        assert!(!seen.contains('['), "the arrow key's sequence isn't echoed: {seen:?}");
        assert!(seen.contains("20 90\r\n"), "{seen:?}");
        assert_eq!(reason.as_deref(), Some("the shell exited"));
        assert!(open.terminal.write(b"x").await.is_err(), "an ended terminal refuses input");
    }

    #[test]
    fn a_final_exit_sets_the_code() {
        assert_eq!(super::final_exit_code("echo hi; exit 3"), 3);
        assert_eq!(super::final_exit_code("exit 7"), 7);
        assert_eq!(super::final_exit_code("echo hi && exit 4"), 4);
        assert_eq!(super::final_exit_code("cargo build"), 0);
    }
}
