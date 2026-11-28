//! Proves every conformance suite is satisfiable (minimal in-memory reference
//! implementations pass) and has teeth (deliberately broken implementations fail).
//!
//! These reference implementations are test-only. Stage 2's fakes live in their own modules.

use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Mutex;

use futures::executor::block_on;
use nixxa_contracts::conformance;
use nixxa_contracts::*;

// ---------- Store ----------

#[derive(Default)]
struct MemStore {
    sessions: Mutex<Vec<SessionRecord>>,
    newest_last: bool,      // bug switch
    drop_summary: bool,     // bug switch: stores a `Done` summary as plain agent text
    no_first_message: bool, // bug switch: never fills `first_message`
}

impl MemStore {
    fn with<T>(&self, id: &SessionId, f: impl FnOnce(&mut SessionRecord) -> T) -> Result<T, StoreError> {
        let mut all = self.sessions.lock().unwrap();
        let s = all.iter_mut().find(|s| &s.info.id == id).ok_or_else(|| StoreError::NotFound(id.clone()))?;
        Ok(f(s))
    }
}

#[async_trait]
impl Store for MemStore {
    async fn create_session(&self, new: NewSession) -> Result<SessionInfo, StoreError> {
        let mut all = self.sessions.lock().unwrap();
        let info = SessionInfo {
            id: SessionId::new(format!("s{}", all.len() + 1)),
            workspace: new.workspace,
            account: new.account,
            started_at: new.started_at,
            first_message: None,
        };
        all.push(SessionRecord { info: info.clone(), messages: vec![], actions: vec![] });
        Ok(info)
    }
    async fn load_session(&self, id: &SessionId) -> Result<SessionRecord, StoreError> {
        self.with(id, |s| s.clone())
    }
    async fn append_message(&self, id: &SessionId, mut message: Message) -> Result<(), StoreError> {
        if let (true, Content::Summary { text }) = (self.drop_summary, &message.content) {
            message.content = Content::Text { text: text.clone() };
        }
        let first = match (&message.role, &message.content) {
            (Role::User, Content::Text { text }) if !self.no_first_message => Some(text.clone()),
            _ => None,
        };
        self.with(id, |s| {
            if s.info.first_message.is_none() {
                s.info.first_message = first;
            }
            s.messages.push(message)
        })
    }
    async fn append_action(&self, id: &SessionId, record: ActionRecord) -> Result<(), StoreError> {
        self.with(id, |s| s.actions.push(record))
    }
    async fn list_sessions(&self) -> Result<Vec<SessionInfo>, StoreError> {
        let mut list: Vec<_> = self.sessions.lock().unwrap().iter().map(|s| s.info.clone()).collect();
        list.sort_by_key(|s| std::cmp::Reverse(s.started_at));
        if self.newest_last {
            list.reverse();
        }
        Ok(list)
    }
    async fn search(&self, query: &str) -> Result<Vec<SearchHit>, StoreError> {
        let q = query.to_lowercase();
        let mut hits = vec![];
        for s in self.sessions.lock().unwrap().iter() {
            for m in &s.messages {
                if let Content::Text { text } = &m.content {
                    if text.to_lowercase().contains(&q) {
                        hits.push(SearchHit { session: s.info.id.clone(), snippet: text.clone() });
                    }
                }
            }
        }
        Ok(hits)
    }
}

#[test]
fn store_reference_passes() {
    block_on(conformance::store::run(MemStore::default));
}

#[test]
#[should_panic(expected = "newest first")]
fn store_suite_catches_wrong_order() {
    block_on(conformance::store::run(|| MemStore { newest_last: true, ..Default::default() }));
}

#[test]
#[should_panic(expected = "keep its marker")]
fn store_suite_catches_a_dropped_summary_marker() {
    block_on(conformance::store::run(|| MemStore { drop_summary: true, ..Default::default() }));
}

#[test]
#[should_panic(expected = "first user text")]
fn store_suite_catches_a_missing_first_message() {
    block_on(conformance::store::run(|| MemStore { no_first_message: true, ..Default::default() }));
}

// ---------- Sandbox ----------

#[derive(Default)]
struct MemSandbox {
    files: Mutex<HashMap<String, String>>,
    no_confinement: bool, // bug switch
    wipe_on_reset: bool,  // bug switch
    echo_exec: bool,      // bug switch: claims isolation but "runs" Exec by echoing the command
    wedge_on_drop: bool,  // bug switch: a dropped Exec leaves the sandbox stuck
    wedged: std::sync::atomic::AtomicBool,
    /// Runs the tiny shell below, with background jobs. Not a bug.
    jobs: bool,
    job_bug: Option<JobBug>,
    job_state: Mutex<MemJobs>,
    net: Mutex<NetworkPolicy>,
    net_bug: Option<NetBug>,
    /// Has a scripted terminal. Not a bug.
    terminal: bool,
    terminal_bug: Option<TermBug>,
    term: std::sync::Arc<Mutex<MemTerm>>,
    patch_bug: Option<PatchBug>,
    /// Keeps checkpoints (copies of the files). Not a bug.
    checkpoints: bool,
    checkpoint_bug: Option<CheckpointBug>,
    taken: Mutex<Vec<HashMap<String, String>>>,
    /// Keeps protected paths from file actions. Not a bug.
    protects: bool,
    protect_bug: Option<ProtectBug>,
    protected: Mutex<Vec<String>>,
}

/// One way to get protected paths wrong.
#[derive(Clone, Copy, PartialEq)]
enum ProtectBug {
    /// Compares the path as written, so `./kept//a.txt` or `KEPT/a.txt` gets through.
    Lexical,
    /// A restore puts protected files back as they were in the checkpoint.
    RestoreOverwrites,
    /// Clearing the rules leaves the paths kept.
    KeepsAfterClear,
}

/// One way to get patches wrong.
#[derive(Clone, Copy, PartialEq)]
enum PatchBug {
    /// Applies the edits whatever the file's hash.
    IgnoresHash,
    /// Replaces the first match even when there are several.
    FirstMatch,
    /// Writes each edit as it goes, so a later failing edit leaves the earlier ones applied.
    Partial,
}

/// One way to get checkpoints wrong.
#[derive(Clone, Copy, PartialEq)]
enum CheckpointBug {
    /// Restores the files the checkpoint had, but leaves files created since.
    KeepsCreated,
    /// Treats an unknown checkpoint as an empty one.
    UnknownIsEmpty,
    /// Restoring a checkpoint forgets the newer ones.
    ForgetsNewer,
}

fn sha256(bytes: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// `-`/`+` lines for what differs: enough of a diff for the suite.
fn mem_diff(old: &str, new: &str) -> String {
    let gone = old.lines().filter(|line| !new.lines().any(|l| l == *line)).map(|line| format!("-{line}\n"));
    let came = new.lines().filter(|line| !old.lines().any(|l| l == *line)).map(|line| format!("+{line}\n"));
    gone.chain(came).collect()
}

/// One way to get the terminal wrong.
#[derive(Clone, Copy, PartialEq)]
enum TermBug {
    /// Says it has no terminal but opens one anyway.
    UndeclaredTerminal,
    /// Ignores resizes.
    NoResize,
    /// `close` returns but the terminal never ends.
    CloseKeepsOpen,
    /// A closed terminal still takes input.
    WritesAfterClose,
    /// Dropping the handle leaves the terminal open (and the next open is refused).
    DropWedges,
    /// `reset` leaves the terminal open.
    ResetKeepsTerminal,
    /// The terminal ends when a command runs (as if it were the foreground command).
    DiesWithCommands,
}

/// The scripted terminal: echoes input; `stty size` answers the size.
#[derive(Default)]
struct MemTerm {
    /// The open terminal's generation, its size and its event channel.
    open: Option<(u64, TerminalSize, futures::channel::mpsc::UnboundedSender<TerminalEvent>)>,
    line: String,
    generation: u64,
}

impl MemTerm {
    fn end(&mut self, generation: u64, reason: &str) {
        if self.open.as_ref().is_some_and(|(g, ..)| *g == generation) {
            if let Some((.., tx)) = self.open.take() {
                let _ = tx.unbounded_send(TerminalEvent::Closed { reason: reason.into() });
            }
        }
    }
}

struct MemTerminal {
    state: std::sync::Arc<Mutex<MemTerm>>,
    generation: u64,
    bug: Option<TermBug>,
}

impl MemTerminal {
    fn live(&self) -> bool {
        self.bug == Some(TermBug::WritesAfterClose)
            || self.state.lock().unwrap().open.as_ref().is_some_and(|(g, ..)| *g == self.generation)
    }
}

#[async_trait]
impl Terminal for MemTerminal {
    async fn write(&self, input: &[u8]) -> Result<(), SandboxError> {
        if !self.live() {
            return Err(SandboxError::Unavailable("closed".into()));
        }
        let mut state = self.state.lock().unwrap();
        let Some((_, size, tx)) = state.open.clone() else { return Ok(()) };
        let _ = tx.unbounded_send(TerminalEvent::Output(input.to_vec()));
        for c in String::from_utf8_lossy(input).chars() {
            if c == '\r' {
                let line = std::mem::take(&mut state.line);
                if line.trim() == "stty size" {
                    let answer = format!("\r\n{} {}\r\n", size.rows, size.cols);
                    let _ = tx.unbounded_send(TerminalEvent::Output(answer.into_bytes()));
                }
            } else {
                state.line.push(c);
            }
        }
        Ok(())
    }
    async fn resize(&self, size: TerminalSize) -> Result<(), SandboxError> {
        if !self.live() {
            return Err(SandboxError::Unavailable("closed".into()));
        }
        if self.bug != Some(TermBug::NoResize) {
            if let Some((_, current, _)) = self.state.lock().unwrap().open.as_mut() {
                *current = size;
            }
        }
        Ok(())
    }
    async fn close(&self) -> Result<(), SandboxError> {
        if self.bug != Some(TermBug::CloseKeepsOpen) {
            self.state.lock().unwrap().end(self.generation, "closed");
        }
        Ok(())
    }
}

impl Drop for MemTerminal {
    fn drop(&mut self) {
        if self.bug != Some(TermBug::DropWedges) && self.bug != Some(TermBug::CloseKeepsOpen) {
            if let Ok(mut state) = self.state.lock() {
                state.end(self.generation, "closed");
            }
        }
    }
}

/// One way to get the network policy wrong.
#[derive(Clone, Copy, PartialEq)]
enum NetBug {
    /// Starts with the network open.
    StartsOpen,
    /// Accepts a policy but keeps reporting the old one.
    NotReported,
    /// Claims it can't restrict the network, then accepts a policy anyway.
    UnrestrictedAcceptsPolicy,
}

/// One way to get background jobs wrong.
#[derive(Clone, Copy, PartialEq)]
enum JobBug {
    /// Without jobs, still runs a background Exec (in the foreground).
    BackgroundWithoutJobs,
    /// Never sends `JobFinished`.
    NoEvents,
    /// Always reports exit code 0.
    WrongExitCode,
    /// Gives every job the same id.
    ReusedIds,
    /// Answers for a job it never started.
    UnknownJobSucceeds,
    /// `reset` leaves jobs running.
    ResetKeepsJobs,
    /// `JobFinished` doesn't say how the job ended.
    NoStatus,
}

#[derive(Default)]
struct MemJobs {
    next: u32,
    /// stdout, and the exit code once it has ended.
    jobs: HashMap<JobId, (String, Option<Option<i32>>)>,
    subscribers: Vec<futures::channel::mpsc::UnboundedSender<SandboxEvent>>,
}

impl MemJobs {
    fn finish(&mut self, job: &JobId, exit_code: Option<i32>, bug: Option<JobBug>) {
        if let Some((_, end)) = self.jobs.get_mut(job) {
            *end = Some(exit_code);
        }
        if bug == Some(JobBug::NoEvents) {
            return;
        }
        let status = if bug == Some(JobBug::NoStatus) { String::new() } else { format!("job {job} ended") };
        let event = SandboxEvent::JobFinished { job: job.clone(), exit_code, duration_ms: 0, truncated: false, status };
        self.subscribers.retain(|tx| tx.unbounded_send(event.clone()).is_ok());
    }
}

/// The tiny shell the job scenarios use: `echo X`, `exit N` and `sleep N`, joined by `;` or `&&`.
/// Returns stdout, the exit code, and whether it sleeps (a background job then runs until
/// stopped).
fn mini_shell(command: &str) -> (String, i32, bool) {
    let (mut stdout, mut sleeps) = (String::new(), false);
    for part in command.split([';', '&']).map(str::trim).filter(|part| !part.is_empty()) {
        if let Some(text) = part.strip_prefix("echo ") {
            stdout.push_str(text);
            stdout.push('\n');
        } else if let Some(code) = part.strip_prefix("exit ") {
            return (stdout, code.parse().unwrap_or(1), sleeps);
        } else if part.starts_with("sleep ") {
            sleeps = true;
        }
    }
    (stdout, 0, sleeps)
}

/// Pending on its first poll (waking itself), ready on the second; flags `wedged` if dropped
/// before it finishes.
struct WedgingExec<'a> {
    polled: bool,
    done: bool,
    wedged: &'a std::sync::atomic::AtomicBool,
}

impl std::future::Future for WedgingExec<'_> {
    type Output = ();
    fn poll(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<()> {
        if self.polled {
            self.done = true;
            return std::task::Poll::Ready(());
        }
        self.polled = true;
        cx.waker().wake_by_ref();
        std::task::Poll::Pending
    }
}

impl Drop for WedgingExec<'_> {
    fn drop(&mut self) {
        if !self.done {
            self.wedged.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }
}

impl MemSandbox {
    /// Whether a file action on `path` (as confined) or `written` (as the agent wrote it) is
    /// kept by the protected paths.
    fn is_protected(&self, path: &str, written: &str) -> bool {
        let rules = self.protected.lock().unwrap();
        if self.protect_bug == Some(ProtectBug::Lexical) {
            return rules.iter().any(|rule| written.starts_with(rule.trim_end_matches("/**")));
        }
        let policy = ProjectPolicy {
            protect: rules
                .iter()
                .map(|pattern| Rule {
                    pattern: pattern.clone(),
                    source: RuleSource { file: "test".into(), line: 1, origin: RuleOrigin::User },
                })
                .collect(),
            ..ProjectPolicy::default()
        };
        policy.protecting(path).is_some()
    }

    fn confine(&self, path: &str) -> Option<String> {
        if self.no_confinement {
            return Some(path.to_owned());
        }
        if path.starts_with('/') || path.split('/').any(|c| c == "..") {
            return None;
        }
        Some(path.trim_start_matches("./").trim_end_matches('/').to_owned())
    }
}

#[async_trait]
impl Sandbox for MemSandbox {
    fn capabilities(&self) -> SandboxCapabilities {
        SandboxCapabilities {
            backend: "memory".into(),
            isolated: true,
            network: match (self.net_bug, self.net.lock().unwrap().clone()) {
                (Some(NetBug::UnrestrictedAcceptsPolicy), _) => NetworkAccess::Unrestricted,
                (Some(NetBug::StartsOpen), NetworkPolicy::Off) => NetworkAccess::Controlled(NetworkPolicy::Open),
                (_, policy) => NetworkAccess::Controlled(policy),
            },
            jobs: self.jobs,
            terminal: self.terminal,
            checkpoints: self.checkpoints,
            actions: if self.jobs {
                SandboxActionKind::ALL.to_vec()
            } else if self.echo_exec || self.wedge_on_drop || self.job_bug.is_some() {
                vec![
                    SandboxActionKind::Exec,
                    SandboxActionKind::ReadFile,
                    SandboxActionKind::WriteFile,
                    SandboxActionKind::Patch,
                    SandboxActionKind::ListDir,
                ]
            } else {
                vec![
                    SandboxActionKind::ReadFile,
                    SandboxActionKind::WriteFile,
                    SandboxActionKind::Patch,
                    SandboxActionKind::ListDir,
                ]
            },
        }
    }
    async fn set_network(&self, policy: NetworkPolicy) -> Result<(), SandboxError> {
        if self.net_bug != Some(NetBug::NotReported) {
            *self.net.lock().unwrap() = policy;
        }
        Ok(())
    }
    async fn open_terminal(&self, size: TerminalSize) -> Result<OpenTerminal, SandboxError> {
        if !self.terminal && self.terminal_bug != Some(TermBug::UndeclaredTerminal) {
            return Err(SandboxError::Unsupported("no terminal".into()));
        }
        let mut state = self.term.lock().unwrap();
        if state.open.is_some() {
            return Err(SandboxError::Unavailable("a terminal is already open".into()));
        }
        state.generation += 1;
        let (tx, rx) = futures::channel::mpsc::unbounded();
        state.open = Some((state.generation, size, tx));
        state.line.clear();
        let terminal = MemTerminal { state: self.term.clone(), generation: state.generation, bug: self.terminal_bug };
        Ok(OpenTerminal { terminal: Box::new(terminal), events: Box::pin(rx) })
    }
    fn subscribe(&self) -> futures::stream::BoxStream<'static, SandboxEvent> {
        let (tx, rx) = futures::channel::mpsc::unbounded();
        if self.jobs && self.job_bug != Some(JobBug::NoEvents) {
            self.job_state.lock().unwrap().subscribers.push(tx);
        }
        Box::pin(rx)
    }
    async fn execute(&self, id: ActionId, action: SandboxAction) -> Result<Observation, SandboxError> {
        if self.wedged.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(SandboxError::Unavailable("wedged".into()));
        }
        if self.terminal_bug == Some(TermBug::DiesWithCommands) && matches!(action, SandboxAction::Exec { .. }) {
            let mut term = self.term.lock().unwrap();
            let generation = term.generation;
            term.end(generation, "killed with the command");
        }
        if self.jobs || self.job_bug.is_some() {
            if let Some(obs) = self.run_job_action(&id, &action) {
                return Ok(obs);
            }
        }
        let path = match &action {
            SandboxAction::Exec { command, .. } if self.echo_exec || self.wedge_on_drop => {
                if self.wedge_on_drop {
                    WedgingExec { polled: false, done: false, wedged: &self.wedged }.await;
                }
                let (exit_code, stdout) =
                    if self.echo_exec { (Some(0), command.clone()) } else { (Some(0), "nixxa\n".into()) };
                return Ok(Observation::new(id, Outcome::Exec { exit_code, stdout, stderr: String::new() }));
            }
            SandboxAction::Exec { .. } => return Err(SandboxError::Unsupported("exec".into())),
            SandboxAction::JobStatus { .. } | SandboxAction::JobOutput { .. } | SandboxAction::JobStop { .. } => {
                return Err(SandboxError::Unsupported("job".into()));
            }
            SandboxAction::ReadFile { path }
            | SandboxAction::WriteFile { path, .. }
            | SandboxAction::Patch { path, .. }
            | SandboxAction::ListDir { path } => path,
        };
        let written = path.clone();
        let Some(path) = self.confine(path) else {
            return Ok(Observation::failed(id, "path is outside the workspace"));
        };
        let mut files = self.files.lock().unwrap();
        let outcome = match action {
            SandboxAction::JobStatus { .. } | SandboxAction::JobOutput { .. } | SandboxAction::JobStop { .. } => {
                return Err(SandboxError::Unsupported("job".into()));
            }
            SandboxAction::ReadFile { .. } => match files.get(&path) {
                Some(c) => Outcome::File { content: c.clone(), hash: sha256(c.as_bytes()) },
                None if self.no_confinement => {
                    Outcome::File { content: "host file".into(), hash: sha256(b"host file") }
                }
                None => Outcome::Failed { message: "no such file".into() },
            },
            SandboxAction::WriteFile { .. } | SandboxAction::Patch { .. } if self.is_protected(&path, &written) => {
                Outcome::Failed { message: "that path is protected".into() }
            }
            SandboxAction::Patch { edits, expected_hash, .. } => match files.get(&path).cloned() {
                None => Outcome::Failed { message: "no such file".into() },
                Some(old) => {
                    let hash = sha256(old.as_bytes());
                    if hash != expected_hash && self.patch_bug != Some(PatchBug::IgnoresHash) {
                        Outcome::Failed { message: format!("the file changed: its hash is now {hash}") }
                    } else {
                        let mut new = old.clone();
                        let mut failed = None;
                        for (n, edit) in edits.iter().enumerate() {
                            let count = new.matches(edit.search.as_str()).count();
                            if count == 1 || (count > 1 && self.patch_bug == Some(PatchBug::FirstMatch)) {
                                new = new.replacen(edit.search.as_str(), &edit.replace, 1);
                                if self.patch_bug == Some(PatchBug::Partial) {
                                    files.insert(path.clone(), new.clone());
                                }
                            } else {
                                failed = Some(format!("edit {}: the search matches {count} places", n + 1));
                                break;
                            }
                        }
                        match failed {
                            Some(message) => Outcome::Failed { message },
                            None => {
                                files.insert(path.clone(), new.clone());
                                Outcome::Patched { diff: mem_diff(&old, &new), hash: sha256(new.as_bytes()) }
                            }
                        }
                    }
                }
            },
            SandboxAction::WriteFile { content, .. } => {
                let bytes = content.len() as u64;
                files.insert(path, content);
                Outcome::Written { bytes }
            }
            SandboxAction::ListDir { .. } => {
                let prefix = if path.is_empty() || path == "." { String::new() } else { format!("{path}/") };
                let mut entries: Vec<DirEntry> = vec![];
                for (p, c) in files.iter() {
                    if let Some(rest) = p.strip_prefix(&prefix) {
                        let (name, is_dir) = match rest.split_once('/') {
                            Some((d, _)) => (d.to_owned(), true),
                            None => (rest.to_owned(), false),
                        };
                        if !entries.iter().any(|e| e.name == name) {
                            entries.push(DirEntry { name, is_dir, size: (!is_dir).then_some(c.len() as u64) });
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
    async fn protect(&self, rules: Vec<String>) -> Result<Protection, SandboxError> {
        if !self.protects {
            return Err(SandboxError::Unsupported("no protection".into()));
        }
        let files = self.files.lock().unwrap();
        let matching =
            |rule: &String| files.keys().filter(|path| glob_matches(rule, path)).cloned().collect::<Vec<_>>();
        let kept = rules.iter().flat_map(matching).collect();
        let unmatched = rules.iter().filter(|rule| matching(rule).is_empty()).cloned().collect();
        if !(rules.is_empty() && self.protect_bug == Some(ProtectBug::KeepsAfterClear)) {
            *self.protected.lock().unwrap() = rules;
        }
        Ok(Protection { how: ProtectionKind::FileActions, kept, unmatched, gaps: Vec::new() })
    }
    async fn checkpoint(&self) -> Result<CheckpointId, SandboxError> {
        if !self.checkpoints {
            return Err(SandboxError::Unsupported("no checkpoints".into()));
        }
        let mut taken = self.taken.lock().unwrap();
        taken.push(self.files.lock().unwrap().clone());
        Ok(CheckpointId::new(format!("c{}", taken.len() - 1)))
    }
    async fn restore(&self, checkpoint: &CheckpointId) -> Result<WorkspaceDiff, SandboxError> {
        if !self.checkpoints {
            return Err(SandboxError::Unsupported("no checkpoints".into()));
        }
        let mut state = self.state(checkpoint)?;
        let undone = mem_workspace_diff(&state, &self.files.lock().unwrap());
        let mut files = self.files.lock().unwrap();
        if self.protect_bug != Some(ProtectBug::RestoreOverwrites) {
            let protected = self.protected.lock().unwrap().clone();
            let kept = |path: &str| protected.iter().any(|rule| glob_matches(rule, path));
            state.retain(|path, _| !kept(path));
            state.extend(files.iter().filter(|(path, _)| kept(path)).map(|(p, c)| (p.clone(), c.clone())));
        }
        if self.checkpoint_bug == Some(CheckpointBug::KeepsCreated) {
            files.extend(state);
        } else {
            *files = state;
        }
        if self.checkpoint_bug == Some(CheckpointBug::ForgetsNewer) {
            let n: usize = checkpoint.as_str()[1..].parse().unwrap_or(0);
            self.taken.lock().unwrap().truncate(n + 1);
        }
        Ok(undone)
    }
    async fn diff(&self, from: &CheckpointId, to: Option<&CheckpointId>) -> Result<WorkspaceDiff, SandboxError> {
        if !self.checkpoints {
            return Err(SandboxError::Unsupported("no checkpoints".into()));
        }
        let before = self.state(from)?;
        let after = match to {
            Some(to) => self.state(to)?,
            None => self.files.lock().unwrap().clone(),
        };
        Ok(mem_workspace_diff(&before, &after))
    }
    async fn reset(&self) -> Result<(), SandboxError> {
        if self.wipe_on_reset {
            self.files.lock().unwrap().clear();
        }
        if self.terminal_bug != Some(TermBug::ResetKeepsTerminal) {
            let mut term = self.term.lock().unwrap();
            let generation = term.generation;
            term.end(generation, "the sandbox was reset");
        }
        if self.job_bug != Some(JobBug::ResetKeepsJobs) {
            let mut state = self.job_state.lock().unwrap();
            let running: Vec<JobId> =
                state.jobs.iter().filter(|(_, (_, end))| end.is_none()).map(|(job, _)| job.clone()).collect();
            for job in running {
                state.finish(&job, None, self.job_bug);
            }
        }
        Ok(())
    }
}

impl MemSandbox {
    /// The files a checkpoint recorded.
    fn state(&self, checkpoint: &CheckpointId) -> Result<HashMap<String, String>, SandboxError> {
        let taken = self.taken.lock().unwrap();
        let n = checkpoint.as_str().strip_prefix('c').and_then(|n| n.parse::<usize>().ok());
        match n.and_then(|n| taken.get(n)) {
            Some(state) => Ok(state.clone()),
            None if self.checkpoint_bug == Some(CheckpointBug::UnknownIsEmpty) => Ok(HashMap::new()),
            None => Err(SandboxError::NoCheckpoint(checkpoint.clone())),
        }
    }

    /// Handles `Exec` and the job actions with the tiny shell; `None` for the file actions.
    fn run_job_action(&self, id: &ActionId, action: &SandboxAction) -> Option<Observation> {
        let bug = self.job_bug;
        let mut state = self.job_state.lock().unwrap();
        let known =
            |state: &MemJobs, job: &JobId| state.jobs.contains_key(job) || bug == Some(JobBug::UnknownJobSucceeds);
        let outcome = match action {
            SandboxAction::Exec { command, background: true, .. } if self.jobs => {
                state.next += 1;
                let job =
                    JobId::new(if bug == Some(JobBug::ReusedIds) { "j1".into() } else { format!("j{}", state.next) });
                let (stdout, code, sleeps) = mini_shell(command);
                state.jobs.insert(job.clone(), (stdout, None));
                if !sleeps {
                    let code = if bug == Some(JobBug::WrongExitCode) { 0 } else { code };
                    state.finish(&job, Some(code), bug);
                }
                Outcome::Started { job }
            }
            SandboxAction::Exec { command, .. } => {
                let (stdout, code, _) = mini_shell(command);
                Outcome::Exec { exit_code: Some(code), stdout, stderr: String::new() }
            }
            SandboxAction::JobStatus { job } if self.jobs && known(&state, job) => {
                Outcome::Text { content: "known".into() }
            }
            SandboxAction::JobOutput { job, .. } if self.jobs && known(&state, job) => {
                Outcome::Text { content: state.jobs.get(job).map(|(out, _)| out.clone()).unwrap_or_default() }
            }
            SandboxAction::JobStop { job } if self.jobs && known(&state, job) => {
                state.finish(job, None, bug);
                Outcome::Text { content: "stopped".into() }
            }
            SandboxAction::JobStatus { .. } | SandboxAction::JobOutput { .. } | SandboxAction::JobStop { .. }
                if self.jobs =>
            {
                Outcome::Failed { message: "no such job".into() }
            }
            _ => return None,
        };
        Some(Observation::new(id.clone(), outcome))
    }
}

fn mem_workspace_diff(before: &HashMap<String, String>, after: &HashMap<String, String>) -> WorkspaceDiff {
    let state = |c: &String| EntryState::File { size: c.len() as u64, mode: 0o644, hash: sha256(c.as_bytes()) };
    let mut paths: Vec<&String> = before.keys().chain(after.keys()).collect();
    paths.sort();
    paths.dedup();
    let changes = paths
        .into_iter()
        .filter(|path| before.get(*path) != after.get(*path))
        .map(|path| FileChange {
            path: path.clone(),
            before: before.get(path).map(state),
            after: after.get(path).map(state),
            diff: match (before.get(path), after.get(path)) {
                (Some(old), Some(new)) => Some(mem_diff(old, new)),
                _ => None,
            },
        })
        .collect();
    WorkspaceDiff { changes, not_covered: Vec::new() }
}

#[test]
fn sandbox_reference_passes() {
    block_on(conformance::sandbox::run(MemSandbox::default));
}

#[test]
fn sandbox_reference_with_jobs_passes() {
    block_on(conformance::sandbox::run(|| MemSandbox { jobs: true, ..Default::default() }));
}

fn with_job_bug(bug: JobBug) -> MemSandbox {
    MemSandbox { jobs: bug != JobBug::BackgroundWithoutJobs, job_bug: Some(bug), ..Default::default() }
}

#[test]
#[should_panic(expected = "without jobs, a background Exec must be Unsupported")]
fn sandbox_suite_catches_a_background_exec_without_jobs() {
    block_on(conformance::sandbox::run(|| with_job_bug(JobBug::BackgroundWithoutJobs)));
}

#[test]
#[should_panic(expected = "must report each job's end")]
fn sandbox_suite_catches_a_job_that_never_reports_its_end() {
    block_on(conformance::sandbox::run(|| with_job_bug(JobBug::NoEvents)));
}

#[test]
#[should_panic(expected = "the shell's exit code")]
fn sandbox_suite_catches_a_wrong_job_exit_code() {
    block_on(conformance::sandbox::run(|| with_job_bug(JobBug::WrongExitCode)));
}

#[test]
#[should_panic(expected = "job ids must be unique")]
fn sandbox_suite_catches_reused_job_ids() {
    block_on(conformance::sandbox::run(|| with_job_bug(JobBug::ReusedIds)));
}

#[test]
#[should_panic(expected = "never started must not succeed")]
fn sandbox_suite_catches_an_unknown_job_succeeding() {
    block_on(conformance::sandbox::run(|| with_job_bug(JobBug::UnknownJobSucceeds)));
}

#[test]
#[should_panic(expected = "say how the job ended")]
fn sandbox_suite_catches_a_job_end_without_words() {
    block_on(conformance::sandbox::run(|| with_job_bug(JobBug::NoStatus)));
}

#[test]
fn a_job_end_recorded_before_its_words_still_loads() {
    let old =
        serde_json::json!({"event": "job_finished", "job": "j1", "exit_code": 0, "duration_ms": 5, "truncated": false});
    let event: SandboxEvent = serde_json::from_value(old).unwrap();
    assert!(matches!(event, SandboxEvent::JobFinished { status, .. } if status.is_empty()));
}

#[test]
fn the_controllers_job_and_wake_up_events_round_trip() {
    for event in [
        Event::JobFinished {
            job: JobId::new("j1"),
            command: "cargo build".into(),
            exit_code: None,
            duration_ms: 3100,
            status: "job j1 was lost: the sandbox stopped".into(),
        },
        Event::WakeUp { turn: TurnId::new("turn-3"), jobs: vec![JobId::new("j1"), JobId::new("j2")] },
    ] {
        let json = serde_json::to_string(&event).unwrap();
        assert_eq!(serde_json::from_str::<Event>(&json).unwrap(), event, "{json}");
    }
}

#[test]
#[should_panic(expected = "reset must stop every job")]
fn sandbox_suite_catches_reset_leaving_jobs_running() {
    block_on(conformance::sandbox::run(|| with_job_bug(JobBug::ResetKeepsJobs)));
}

#[test]
#[should_panic(expected = "the network must start off")]
fn sandbox_suite_catches_a_network_that_starts_open() {
    block_on(conformance::sandbox::run(|| MemSandbox { net_bug: Some(NetBug::StartsOpen), ..Default::default() }));
}

#[test]
#[should_panic(expected = "must report the policy just set")]
fn sandbox_suite_catches_a_policy_that_is_not_reported() {
    block_on(conformance::sandbox::run(|| MemSandbox { net_bug: Some(NetBug::NotReported), ..Default::default() }));
}

#[test]
#[should_panic(expected = "can't restrict the network must refuse a policy")]
fn sandbox_suite_catches_an_unrestricted_sandbox_taking_a_policy() {
    block_on(conformance::sandbox::run(|| MemSandbox {
        net_bug: Some(NetBug::UnrestrictedAcceptsPolicy),
        ..Default::default()
    }));
}

#[test]
fn sandbox_reference_with_a_terminal_passes() {
    block_on(conformance::sandbox::run(|| MemSandbox { jobs: true, terminal: true, ..Default::default() }));
}

fn with_terminal_bug(bug: TermBug) -> MemSandbox {
    MemSandbox {
        jobs: true,
        terminal: bug != TermBug::UndeclaredTerminal,
        terminal_bug: Some(bug),
        ..Default::default()
    }
}

#[test]
#[should_panic(expected = "without a terminal, open_terminal must be Unsupported")]
fn sandbox_suite_catches_an_undeclared_terminal() {
    block_on(conformance::sandbox::run(|| with_terminal_bug(TermBug::UndeclaredTerminal)));
}

#[test]
#[should_panic(expected = "must report the size after a resize")]
fn sandbox_suite_catches_a_terminal_that_ignores_resizes() {
    block_on(conformance::sandbox::run(|| with_terminal_bug(TermBug::NoResize)));
}

#[test]
#[should_panic(expected = "closing a terminal must end it")]
fn sandbox_suite_catches_a_close_that_keeps_the_terminal() {
    block_on(conformance::sandbox::run(|| with_terminal_bug(TermBug::CloseKeepsOpen)));
}

#[test]
#[should_panic(expected = "a closed terminal's write must fail")]
fn sandbox_suite_catches_a_closed_terminal_taking_input() {
    block_on(conformance::sandbox::run(|| with_terminal_bug(TermBug::WritesAfterClose)));
}

#[test]
#[should_panic(expected = "dropping a terminal's handle must end it")]
fn sandbox_suite_catches_a_dropped_terminal_left_open() {
    block_on(conformance::sandbox::run(|| with_terminal_bug(TermBug::DropWedges)));
}

#[test]
#[should_panic(expected = "reset must end the terminal")]
fn sandbox_suite_catches_reset_leaving_the_terminal() {
    block_on(conformance::sandbox::run(|| with_terminal_bug(TermBug::ResetKeepsTerminal)));
}

#[test]
#[should_panic(expected = "must outlive the agent's commands")]
fn sandbox_suite_catches_a_terminal_killed_with_a_command() {
    block_on(conformance::sandbox::run(|| with_terminal_bug(TermBug::DiesWithCommands)));
}

#[test]
fn an_observation_without_requests_serialises_as_before() {
    let obs = Observation::new(ActionId::new("a"), Outcome::Written { bytes: 1 });
    let json = serde_json::to_value(&obs).unwrap();
    assert!(json.get("network").is_none(), "{json}");
    assert_eq!(serde_json::from_value::<Observation>(json).unwrap(), obs);
}

#[test]
fn an_exec_recorded_before_background_jobs_still_loads() {
    let old = serde_json::json!({"kind": "exec", "command": "ls", "cwd": null, "timeout_ms": null});
    let action: SandboxAction = serde_json::from_value(old).unwrap();
    assert_eq!(action, SandboxAction::Exec { command: "ls".into(), cwd: None, timeout_ms: None, background: false });
}

#[test]
fn a_blocked_request_serialises() {
    let result = NetResult::Blocked { reason: "not allowed".into() };
    let json = serde_json::to_value(&result).unwrap();
    assert_eq!(json, serde_json::json!({"result": "blocked", "reason": "not allowed"}));
    assert_eq!(serde_json::from_value::<NetResult>(json).unwrap(), result);
}

#[test]
#[should_panic(expected = "escaped the workspace")]
fn sandbox_suite_catches_escape() {
    block_on(conformance::sandbox::run(|| MemSandbox { no_confinement: true, ..Default::default() }));
}

#[test]
#[should_panic(expected = "reset must keep the workspace")]
fn sandbox_suite_catches_reset_wiping_the_workspace() {
    block_on(conformance::sandbox::run(|| MemSandbox { wipe_on_reset: true, ..Default::default() }));
}

#[test]
#[should_panic(expected = "must really run `echo nixxa`")]
fn sandbox_suite_catches_an_isolated_exec_that_does_not_run() {
    block_on(conformance::sandbox::run(|| MemSandbox { echo_exec: true, ..Default::default() }));
}

#[test]
#[should_panic(expected = "wedged the sandbox")]
fn sandbox_suite_catches_a_dropped_exec_wedging() {
    block_on(conformance::sandbox::run(|| MemSandbox { wedge_on_drop: true, ..Default::default() }));
}

// ---------- Knowledge ----------

struct MemKnowledge;

#[async_trait]
impl Knowledge for MemKnowledge {
    async fn sources(&self) -> Vec<SourceInfo> {
        vec![
            SourceInfo { id: "notes".into(), name: "Notes".into(), status: SourceStatus::Ready },
            SourceInfo { id: "absent".into(), name: "Absent".into(), status: SourceStatus::Missing },
        ]
    }
    async fn tools(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            source: "notes".into(),
            name: "search".into(),
            description: "Search notes".into(),
            input_schema: serde_json::json!({"type": "object"}),
            mutating: false,
        }]
    }
    async fn primer(&self, _: &WorkspaceInfo) -> Result<Primer, KnowledgeError> {
        Ok(Primer {
            sections: vec![PrimerSection { source: "notes".into(), title: "Project".into(), body: "…".into() }],
        })
    }
    async fn call(
        &self,
        id: ActionId,
        source: &SourceId,
        tool: &str,
        _: serde_json::Value,
    ) -> Result<Observation, KnowledgeError> {
        if source.as_str() != "notes" {
            return Err(KnowledgeError::SourceUnavailable(source.clone()));
        }
        if tool != "search" {
            return Err(KnowledgeError::UnknownTool { source_id: source.clone(), tool: tool.into() });
        }
        Ok(Observation::new(id, Outcome::Text { content: "no results".into() }))
    }
    async fn record(&self, id: ActionId, source: &SourceId, _: KnowledgeEntry) -> Result<Observation, KnowledgeError> {
        if source.as_str() != "notes" {
            return Err(KnowledgeError::SourceUnavailable(source.clone()));
        }
        Ok(Observation::new(id, Outcome::Recorded { location: None }))
    }
}

#[test]
fn knowledge_reference_passes() {
    block_on(conformance::knowledge::run(&MemKnowledge));
}

// ---------- Credentials ----------

#[derive(Default)]
struct MemCredentials {
    accounts: Mutex<Vec<(AccountInfo, Credential)>>,
    bug: Option<CredBug>,
}

#[async_trait]
impl Credentials for MemCredentials {
    async fn list(&self, provider: Option<&ProviderId>) -> Result<Vec<AccountInfo>, CredentialsError> {
        let all = self.accounts.lock().unwrap();
        Ok(all.iter().map(|(i, _)| i.clone()).filter(|i| provider.is_none_or(|p| &i.account.provider == p)).collect())
    }
    async fn get(&self, account: &AccountRef) -> Result<Credential, CredentialsError> {
        let all = self.accounts.lock().unwrap();
        all.iter()
            .find(|(i, _)| &i.account == account)
            .map(|(_, c)| c.clone())
            .ok_or_else(|| CredentialsError::NotFound(account.clone()))
    }
    async fn default_account(&self, provider: &ProviderId) -> Result<Option<AccountRef>, CredentialsError> {
        let all = self.accounts.lock().unwrap();
        Ok(all.iter().find(|(i, _)| &i.account.provider == provider && i.is_default).map(|(i, _)| i.account.clone()))
    }
    async fn add(&self, account: AccountRef, credential: Credential) -> Result<AccountInfo, CredentialsError> {
        let mut all = self.accounts.lock().unwrap();
        let existing = all.iter().position(|(i, _)| i.account == account);
        let first = !all.iter().any(|(i, _)| i.account.provider == account.provider);
        let is_default = match existing {
            Some(n) => all.remove(n).0.is_default,
            None => first,
        };
        let info = AccountInfo { account, method: credential.method(), is_default, hint: credential.display_hint() };
        all.push((info.clone(), credential));
        Ok(info)
    }
    async fn remove(&self, account: &AccountRef) -> Result<(), CredentialsError> {
        let mut all = self.accounts.lock().unwrap();
        let n = all
            .iter()
            .position(|(i, _)| &i.account == account)
            .ok_or_else(|| CredentialsError::NotFound(account.clone()))?;
        all.remove(n);
        Ok(())
    }
    async fn set_default(&self, account: &AccountRef) -> Result<(), CredentialsError> {
        let mut all = self.accounts.lock().unwrap();
        if !all.iter().any(|(i, _)| &i.account == account) {
            return Err(CredentialsError::NotFound(account.clone()));
        }
        for (i, _) in all.iter_mut().filter(|(i, _)| i.account.provider == account.provider) {
            i.is_default = &i.account == account;
        }
        Ok(())
    }
    async fn validate(&self, account: &AccountRef) -> Result<(), CredentialsError> {
        self.get(account).await.map(|_| ())
    }
    async fn fingerprints(&self) -> Result<SecretFingerprints, CredentialsError> {
        let mut prints = SecretFingerprints::new([3; 32]);
        let all = self.accounts.lock().unwrap();
        // The bug leaves out every account but the first.
        let kept = if self.bug == Some(CredBug::FirstOnly) { 1 } else { all.len() };
        for (info, credential) in all.iter().take(kept) {
            if let Credential::ApiKey(secret) | Credential::BearerToken { token: secret, .. } = credential {
                prints.add(&format!("{}-key", info.account.provider), secret);
            }
        }
        Ok(prints)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CredBug {
    FirstOnly,
}

#[test]
fn credentials_reference_passes() {
    block_on(conformance::credentials::run(MemCredentials::default));
}

#[test]
#[should_panic(expected = "every key must be fingerprinted")]
fn credentials_suite_catches_missing_fingerprints() {
    block_on(conformance::credentials::run(|| MemCredentials { bug: Some(CredBug::FirstOnly), ..Default::default() }));
}

// ---------- Brain ----------

struct EchoBrain;

#[async_trait]
impl Brain for EchoBrain {
    fn describe(&self) -> BrainInfo {
        BrainInfo { provider: "echo".into(), model: "echo-1".into() }
    }
    async fn think(&self, _: Context) -> Result<BrainStream, BrainError> {
        Ok(Box::pin(futures::stream::iter([
            Ok(BrainEvent::TextDelta { text: "hello".into() }),
            Ok(BrainEvent::Intent { intent: Intent::Say { text: "hello".into() } }),
            Ok(BrainEvent::Usage { usage: Usage::default() }),
        ])))
    }
}

#[test]
fn brain_reference_passes() {
    block_on(conformance::brain::run(&EchoBrain, AccountRef::new("echo", "default")));
}

// ---------- Wire format ----------

#[test]
fn action_wire_format_is_stable() {
    let a = Action::Sandbox { action: SandboxAction::ReadFile { path: "src/main.rs".into() } };
    let json = serde_json::to_value(&a).unwrap();
    assert_eq!(json, serde_json::json!({"target": "sandbox", "action": {"kind": "read_file", "path": "src/main.rs"}}));
    assert_eq!(serde_json::from_value::<Action>(json).unwrap(), a);
}

// ---------- Patches and checkpoints ----------

#[test]
fn sandbox_reference_with_checkpoints_passes() {
    block_on(conformance::sandbox::run(|| MemSandbox { checkpoints: true, ..Default::default() }));
}

fn with_patch_bug(bug: PatchBug) -> MemSandbox {
    MemSandbox { patch_bug: Some(bug), ..Default::default() }
}

#[test]
#[should_panic(expected = "a stale patch must fail")]
fn sandbox_suite_catches_a_patch_that_ignores_the_hash() {
    block_on(conformance::sandbox::run(|| with_patch_bug(PatchBug::IgnoresHash)));
}

#[test]
#[should_panic(expected = "a search that matches twice")]
fn sandbox_suite_catches_a_patch_that_takes_the_first_of_several_matches() {
    block_on(conformance::sandbox::run(|| with_patch_bug(PatchBug::FirstMatch)));
}

#[test]
#[should_panic(expected = "all the edits apply, or none do")]
fn sandbox_suite_catches_a_partly_applied_patch() {
    block_on(conformance::sandbox::run(|| with_patch_bug(PatchBug::Partial)));
}

fn with_checkpoint_bug(bug: CheckpointBug) -> MemSandbox {
    MemSandbox { checkpoints: true, checkpoint_bug: Some(bug), ..Default::default() }
}

#[test]
#[should_panic(expected = "restore must remove files created since")]
fn sandbox_suite_catches_a_restore_that_keeps_new_files() {
    block_on(conformance::sandbox::run(|| with_checkpoint_bug(CheckpointBug::KeepsCreated)));
}

#[test]
#[should_panic(expected = "an unknown checkpoint must be NoCheckpoint")]
fn sandbox_suite_catches_an_unknown_checkpoint_restoring() {
    block_on(conformance::sandbox::run(|| with_checkpoint_bug(CheckpointBug::UnknownIsEmpty)));
}

#[test]
#[should_panic(expected = "and the newer again")]
fn sandbox_suite_catches_a_restore_that_forgets_newer_checkpoints() {
    block_on(conformance::sandbox::run(|| with_checkpoint_bug(CheckpointBug::ForgetsNewer)));
}

#[test]
#[should_panic(expected = "without checkpoints, checkpoint must be Unsupported")]
fn sandbox_suite_catches_undeclared_checkpoints() {
    // Keeps checkpoints but says it doesn't.
    struct Undeclared(MemSandbox);
    #[async_trait]
    impl Sandbox for Undeclared {
        fn capabilities(&self) -> SandboxCapabilities {
            SandboxCapabilities { checkpoints: false, ..self.0.capabilities() }
        }
        async fn execute(&self, id: ActionId, action: SandboxAction) -> Result<Observation, SandboxError> {
            self.0.execute(id, action).await
        }
        async fn health(&self) -> Result<(), SandboxError> {
            self.0.health().await
        }
        async fn reset(&self) -> Result<(), SandboxError> {
            self.0.reset().await
        }
        async fn set_network(&self, policy: NetworkPolicy) -> Result<(), SandboxError> {
            self.0.set_network(policy).await
        }
        async fn checkpoint(&self) -> Result<CheckpointId, SandboxError> {
            self.0.checkpoint().await
        }
    }
    block_on(conformance::sandbox::run(|| Undeclared(MemSandbox { checkpoints: true, ..Default::default() })));
}

#[test]
fn records_and_reads_from_before_stage_7_still_load() {
    let old = serde_json::json!({
        "request": {"id": "a", "action": {"target": "sandbox", "action": {"kind": "read_file", "path": "x"}}, "reason": null},
        "verdict": {"verdict": "allow"},
        "observation": null,
        "at": 1
    });
    let record: ActionRecord = serde_json::from_value(old).unwrap();
    assert_eq!(record.checkpoint, None);
    let caps =
        serde_json::json!({"backend": "b", "isolated": true, "network": {"access": "unrestricted"}, "actions": []});
    let caps: SandboxCapabilities = serde_json::from_value(caps).unwrap();
    assert!(!caps.checkpoints);
}

#[test]
fn patches_and_diffs_round_trip() {
    let action = SandboxAction::Patch {
        path: "src/a.rs".into(),
        edits: vec![Edit { search: "a".into(), replace: "b".into() }],
        expected_hash: "00".into(),
    };
    let json = serde_json::to_value(&action).unwrap();
    assert_eq!(json["kind"], "patch");
    assert_eq!(serde_json::from_value::<SandboxAction>(json).unwrap(), action);
    assert!(action.changes_files() && action.is_mutating());
    assert!(!SandboxAction::ReadFile { path: "a".into() }.changes_files());
    let diff = WorkspaceDiff {
        changes: vec![FileChange {
            path: "l".into(),
            before: Some(EntryState::Symlink { target: "t".into() }),
            after: None,
            diff: None,
        }],
        not_covered: vec!["target/".into()],
    };
    let event = Event::Restored { checkpoint: CheckpointId::new("c1"), undone: diff };
    let back: Event = serde_json::from_value(serde_json::to_value(&event).unwrap()).unwrap();
    assert_eq!(back, event);
}

// ---------- Protected paths ----------

#[test]
fn mem_sandbox_with_protected_paths_conforms() {
    block_on(conformance::sandbox::run(|| MemSandbox { protects: true, checkpoints: true, ..Default::default() }));
}

fn with_protect_bug(bug: ProtectBug) -> MemSandbox {
    MemSandbox { protects: true, checkpoints: true, protect_bug: Some(bug), ..Default::default() }
}

#[test]
#[should_panic(expected = "changed a protected path")]
fn sandbox_suite_catches_a_protected_path_matched_as_written() {
    block_on(conformance::sandbox::run(|| with_protect_bug(ProtectBug::Lexical)));
}

#[test]
#[should_panic(expected = "a restore included")]
fn sandbox_suite_catches_a_restore_that_overwrites_a_protected_path() {
    block_on(conformance::sandbox::run(|| with_protect_bug(ProtectBug::RestoreOverwrites)));
}

#[test]
#[should_panic(expected = "clearing the rules frees the path")]
fn sandbox_suite_catches_rules_that_outlive_their_clearing() {
    block_on(conformance::sandbox::run(|| with_protect_bug(ProtectBug::KeepsAfterClear)));
}
