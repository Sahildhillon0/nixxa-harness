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
    newest_last: bool,  // bug switch
    drop_summary: bool, // bug switch: stores a `Done` summary as plain agent text
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
        self.with(id, |s| s.messages.push(message))
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

// ---------- Sandbox ----------

#[derive(Default)]
struct MemSandbox {
    files: Mutex<HashMap<String, String>>,
    no_confinement: bool, // bug switch
    wipe_on_reset: bool,  // bug switch
    echo_exec: bool,      // bug switch: claims isolation but "runs" Exec by echoing the command
    wedge_on_drop: bool,  // bug switch: a dropped Exec leaves the sandbox stuck
    wedged: std::sync::atomic::AtomicBool,
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
            network: false,
            actions: if self.echo_exec || self.wedge_on_drop {
                SandboxActionKind::ALL.to_vec()
            } else {
                vec![SandboxActionKind::ReadFile, SandboxActionKind::WriteFile, SandboxActionKind::ListDir]
            },
        }
    }
    async fn execute(&self, id: ActionId, action: SandboxAction) -> Result<Observation, SandboxError> {
        if self.wedged.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(SandboxError::Unavailable("wedged".into()));
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
            SandboxAction::ReadFile { path }
            | SandboxAction::WriteFile { path, .. }
            | SandboxAction::ListDir { path } => path,
        };
        let Some(path) = self.confine(path) else {
            return Ok(Observation::failed(id, "path is outside the workspace"));
        };
        let mut files = self.files.lock().unwrap();
        let outcome = match action {
            SandboxAction::ReadFile { .. } => match files.get(&path) {
                Some(c) => Outcome::Text { content: c.clone() },
                None if self.no_confinement => Outcome::Text { content: "host file".into() },
                None => Outcome::Failed { message: "no such file".into() },
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
    async fn reset(&self) -> Result<(), SandboxError> {
        if self.wipe_on_reset {
            self.files.lock().unwrap().clear();
        }
        Ok(())
    }
}

#[test]
fn sandbox_reference_passes() {
    block_on(conformance::sandbox::run(MemSandbox::default));
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
}

#[test]
fn credentials_reference_passes() {
    block_on(conformance::credentials::run(MemCredentials::default));
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
