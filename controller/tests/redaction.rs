//! Secret redaction (#3) at every way into the agent's context: a planted AWS key pair never
//! reaches the brain (a recording brain keeps every context it was given) or the store, and
//! every redaction is told to the UI with where it came from.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::stream::BoxStream;
use nixxa_contracts::{
    AccountRef, Action, ActionId, ActionRecord, ActionRequest, ApprovalPort, Asking, Brain, BrainError, BrainEvent,
    BrainInfo, BrainStream, Content, Context, DirEntry, Edit, Event, EventSink, Intent, JobId, Knowledge,
    KnowledgeAction, KnowledgeEntry, KnowledgeError, Message, NetRequest, NetResult, NetworkAccess, NewSession,
    Observation, Outcome, Primer, PrimerSection, ProviderId, RedactionSource, Role, Sandbox, SandboxAction,
    SandboxActionKind, SandboxCapabilities, SandboxError, SandboxEvent, SearchHit, Secret, SecretFingerprints,
    SessionId, SessionInfo, SessionRecord, SourceId, SourceInfo, SourceStatus, Store, StoreError, Timestamp, ToolSpec,
    Verdict, WorkspaceInfo,
};
use nixxa_controller::scrub::Scrubber;
use nixxa_controller::{Command, Controller, ControllerConfig, UserMessage};
use tokio::sync::mpsc;
use tokio::time::{Duration, sleep};

/// The planted key pair, as `aws configure` would write it.
const KEY_ID: &str = "AKIAIOSFODNN7EXAMPLE";
const SECRET: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";

fn planted() -> String {
    format!("aws_access_key_id = {KEY_ID}\naws_secret_access_key = {SECRET}\n")
}

/// Whether `value`, serialised, holds any piece of the planted pair.
fn leaks(value: &impl serde::Serialize) -> bool {
    let json = serde_json::to_string(value).unwrap();
    json.contains("IOSFODNN7") || json.contains("K7MDENG")
}

// ---------------------------------------------------------------------------------------------
// Fakes
// ---------------------------------------------------------------------------------------------

/// Replays one intent per step (then `Done`), and keeps every context.
#[derive(Default)]
struct RecordingBrain {
    steps: Mutex<VecDeque<Vec<Intent>>>,
    contexts: Mutex<Vec<Context>>,
}

impl RecordingBrain {
    fn new(steps: Vec<Vec<Intent>>) -> Arc<Self> {
        Arc::new(Self { steps: Mutex::new(steps.into()), contexts: Mutex::default() })
    }

    fn contexts(&self) -> Vec<Context> {
        self.contexts.lock().unwrap().clone()
    }
}

#[async_trait]
impl Brain for RecordingBrain {
    fn describe(&self) -> BrainInfo {
        BrainInfo { provider: ProviderId::new("recording"), model: "recording-1".into() }
    }

    async fn think(&self, context: Context) -> Result<BrainStream, BrainError> {
        self.contexts.lock().unwrap().push(context);
        let intents =
            self.steps.lock().unwrap().pop_front().unwrap_or_else(|| vec![Intent::Done { summary: "done".into() }]);
        let events: Vec<Result<BrainEvent, BrainError>> =
            intents.into_iter().map(|intent| Ok(BrainEvent::Intent { intent })).collect();
        Ok(Box::pin(futures::stream::iter(events)))
    }
}

/// Every action's result holds the planted pair; a background job's end does too.
struct PlantedSandbox {
    ended: Mutex<Option<mpsc::UnboundedSender<SandboxEvent>>>,
    events: Mutex<Option<mpsc::UnboundedReceiver<SandboxEvent>>>,
}

impl PlantedSandbox {
    fn new() -> Arc<Self> {
        let (tx, rx) = mpsc::unbounded_channel();
        Arc::new(Self { ended: Mutex::new(Some(tx)), events: Mutex::new(Some(rx)) })
    }
}

#[async_trait]
impl Sandbox for PlantedSandbox {
    fn capabilities(&self) -> SandboxCapabilities {
        SandboxCapabilities {
            backend: "planted".into(),
            isolated: false,
            network: NetworkAccess::default(),
            jobs: true,
            terminal: false,
            checkpoints: false,
            actions: SandboxActionKind::ALL.to_vec(),
        }
    }

    async fn execute(&self, id: ActionId, action: SandboxAction) -> Result<Observation, SandboxError> {
        let outcome = match action {
            SandboxAction::Exec { background: true, .. } => {
                let job = JobId::new("j1");
                if let Some(tx) = self.ended.lock().unwrap().as_ref() {
                    let _ = tx.send(SandboxEvent::JobFinished {
                        job: job.clone(),
                        exit_code: Some(1),
                        duration_ms: 5,
                        truncated: false,
                        status: format!("job j1 exited with code 1: bad key {KEY_ID}"),
                    });
                }
                Outcome::Started { job }
            }
            SandboxAction::Exec { .. } => {
                let mut observation = Observation::new(
                    id,
                    Outcome::Exec { exit_code: Some(0), stdout: planted(), stderr: format!("using {KEY_ID}") },
                );
                observation.network.push(NetRequest {
                    time: Timestamp(0),
                    job: None,
                    host: "example.com".into(),
                    port: 80,
                    method: Some("GET".into()),
                    url: Some(format!("http://example.com/upload?AWSAccessKeyId={KEY_ID}&Signature=x")),
                    bytes_up: 0,
                    bytes_down: 0,
                    duration_ms: 1,
                    result: NetResult::Allowed,
                });
                return Ok(observation);
            }
            SandboxAction::ReadFile { .. } => Outcome::File { content: planted(), hash: "h".into() },
            SandboxAction::Patch { .. } => Outcome::Patched { diff: format!("+{}", planted()), hash: "h2".into() },
            SandboxAction::WriteFile { content, .. } => Outcome::Written { bytes: content.len() as u64 },
            SandboxAction::JobOutput { .. } => Outcome::Text { content: format!("--- stdout ---\n{}", planted()) },
            SandboxAction::JobStatus { .. } => Outcome::Text { content: format!("job j1: {KEY_ID}") },
            SandboxAction::ListDir { .. } => Outcome::Listing {
                entries: vec![DirEntry { name: format!("{KEY_ID}.txt"), is_dir: false, size: None }],
            },
            SandboxAction::JobStop { .. } => {
                return Err(SandboxError::Other(format!("could not stop: the key {KEY_ID} is wrong")));
            }
        };
        Ok(Observation::new(id, outcome))
    }

    async fn health(&self) -> Result<(), SandboxError> {
        Ok(())
    }

    async fn reset(&self) -> Result<(), SandboxError> {
        Ok(())
    }

    fn subscribe(&self) -> BoxStream<'static, SandboxEvent> {
        match self.events.lock().unwrap().take() {
            Some(rx) => Box::pin(futures::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|e| (e, rx)) })),
            None => Box::pin(futures::stream::empty()),
        }
    }
}

#[derive(Default)]
struct MemoryStore {
    sessions: Mutex<Vec<SessionRecord>>,
}

impl MemoryStore {
    fn all(&self) -> Vec<SessionRecord> {
        self.sessions.lock().unwrap().clone()
    }

    fn with<T>(&self, id: &SessionId, f: impl FnOnce(&mut SessionRecord) -> T) -> Result<T, StoreError> {
        let mut sessions = self.sessions.lock().unwrap();
        let session = sessions.iter_mut().find(|s| &s.info.id == id).ok_or_else(|| StoreError::NotFound(id.clone()))?;
        Ok(f(session))
    }
}

#[async_trait]
impl Store for MemoryStore {
    async fn create_session(&self, new: NewSession) -> Result<SessionInfo, StoreError> {
        let mut sessions = self.sessions.lock().unwrap();
        let info = SessionInfo {
            id: SessionId::new(format!("s{}", sessions.len() + 1)),
            workspace: new.workspace,
            account: new.account,
            started_at: new.started_at,
            first_message: None,
        };
        sessions.push(SessionRecord { info: info.clone(), messages: Vec::new(), actions: Vec::new() });
        Ok(info)
    }

    async fn load_session(&self, id: &SessionId) -> Result<SessionRecord, StoreError> {
        self.with(id, |s| s.clone())
    }

    async fn append_message(&self, id: &SessionId, message: Message) -> Result<(), StoreError> {
        self.with(id, |s| s.messages.push(message))
    }

    async fn append_action(&self, id: &SessionId, record: ActionRecord) -> Result<(), StoreError> {
        self.with(id, |s| s.actions.push(record))
    }

    async fn list_sessions(&self) -> Result<Vec<SessionInfo>, StoreError> {
        Ok(self.sessions.lock().unwrap().iter().map(|s| s.info.clone()).collect())
    }

    async fn search(&self, _query: &str) -> Result<Vec<SearchHit>, StoreError> {
        Ok(Vec::new())
    }
}

/// Its search results hold the planted pair, and so may its primer.
struct PlantedKnowledge {
    primer: bool,
}

#[async_trait]
impl Knowledge for PlantedKnowledge {
    async fn sources(&self) -> Vec<SourceInfo> {
        vec![SourceInfo { id: SourceId::new("notes"), name: "Notes".into(), status: SourceStatus::Ready }]
    }

    async fn tools(&self) -> Vec<ToolSpec> {
        Vec::new()
    }

    async fn primer(&self, _workspace: &WorkspaceInfo) -> Result<Primer, KnowledgeError> {
        if !self.primer {
            return Ok(Primer::default());
        }
        Ok(Primer {
            sections: vec![PrimerSection {
                source: SourceId::new("notes"),
                title: "Deploy notes".into(),
                body: format!("The CI key: {KEY_ID}"),
            }],
        })
    }

    async fn call(
        &self,
        id: ActionId,
        _: &SourceId,
        _: &str,
        _: serde_json::Value,
    ) -> Result<Observation, KnowledgeError> {
        Ok(Observation::new(id, Outcome::Text { content: planted() }))
    }

    async fn record(&self, id: ActionId, _: &SourceId, _: KnowledgeEntry) -> Result<Observation, KnowledgeError> {
        Ok(Observation::new(id, Outcome::Recorded { location: None }))
    }
}

/// Allows everything, and keeps how each prompt was asked.
#[derive(Default)]
struct Approvals {
    asked: Mutex<Vec<(String, Asking)>>,
}

#[async_trait]
impl ApprovalPort for Approvals {
    async fn ask(&self, _request: &ActionRequest, reason: &str, asking: Asking) -> Verdict {
        self.asked.lock().unwrap().push((reason.to_string(), asking));
        Verdict::Allow
    }
}

#[derive(Default)]
struct Sink {
    events: Mutex<Vec<Event>>,
}

impl EventSink for Sink {
    fn emit(&self, event: Event) {
        self.events.lock().unwrap().push(event);
    }
}

impl Sink {
    fn all(&self) -> Vec<Event> {
        self.events.lock().unwrap().clone()
    }

    /// Where each redaction came from, and whether it was sent anyway.
    fn redactions(&self) -> Vec<(String, RedactionSource, bool)> {
        self.all()
            .into_iter()
            .filter_map(|e| match e {
                Event::SecretRedacted { kind, source, sent, .. } => Some((kind, source, sent)),
                _ => None,
            })
            .collect()
    }

    async fn wait_turns(&self, n: usize) {
        for _ in 0..400 {
            if self.all().iter().filter(|e| matches!(e, Event::TurnDone { .. })).count() >= n {
                return;
            }
            sleep(Duration::from_millis(5)).await;
        }
        panic!("timed out: {:#?}", self.all());
    }
}

// ---------------------------------------------------------------------------------------------
// Rig
// ---------------------------------------------------------------------------------------------

struct Rig {
    brain: Arc<RecordingBrain>,
    store: Arc<MemoryStore>,
    sink: Arc<Sink>,
    approvals: Arc<Approvals>,
    scrubber: Arc<Scrubber>,
    tx: mpsc::Sender<Command>,
}

impl Rig {
    async fn start(steps: Vec<Vec<Intent>>) -> Self {
        Self::launch(steps, Arc::new(MemoryStore::default()), None, false).await
    }

    async fn launch(steps: Vec<Vec<Intent>>, store: Arc<MemoryStore>, resume: Option<&str>, primer: bool) -> Self {
        let brain = RecordingBrain::new(steps);
        let sink = Arc::new(Sink::default());
        let approvals = Arc::new(Approvals::default());
        let scrubber = Arc::new(Scrubber::with_key([9; 32]));
        let mut config = ControllerConfig::new(
            WorkspaceInfo { name: "demo".into(), root: std::env::temp_dir() },
            AccountRef::new("recording", "default"),
            "Be brief.",
            scrubber.clone(),
        );
        config.clock = || Timestamp(0);
        config.wake_delay = Duration::from_millis(10);
        let mut controller = Controller::new(
            brain.clone(),
            PlantedSandbox::new(),
            store.clone(),
            Arc::new(PlantedKnowledge { primer }),
            approvals.clone(),
            sink.clone(),
            config,
        );
        match resume {
            None => controller.start_session().await.unwrap(),
            Some(id) => controller.resume_session(&SessionId::new(id)).await.unwrap(),
        };
        let (tx, rx) = mpsc::channel(8);
        tokio::spawn(async move { controller.run(rx).await });
        Self { brain, store, sink, approvals, scrubber, tx }
    }

    async fn turn(&self, message: impl Into<UserMessage>) {
        let done = self.sink.all().iter().filter(|e| matches!(e, Event::TurnDone { .. })).count();
        self.tx.send(Command::UserMessage(message.into())).await.unwrap();
        self.sink.wait_turns(done + 1).await;
    }

    /// Neither the brain nor the store ever got the planted pair, nor did the UI's events.
    fn assert_never_leaked(&self) {
        for context in self.brain.contexts() {
            assert!(!leaks(&context), "the brain got the key: {context:#?}");
        }
        assert!(!leaks(&self.store.all()), "the store holds the key: {:#?}", self.store.all());
        assert!(!leaks(&self.sink.all()), "an event holds the key: {:#?}", self.sink.all());
    }

    /// The last context the brain got, as JSON.
    fn last_context(&self) -> String {
        serde_json::to_string(self.brain.contexts().last().unwrap()).unwrap()
    }
}

fn act(action: SandboxAction) -> Intent {
    Intent::Act { request: ActionRequest { id: ActionId::new("a1"), action: Action::Sandbox { action }, reason: None } }
}

fn exec(command: &str, background: bool) -> SandboxAction {
    SandboxAction::Exec { command: command.into(), cwd: None, timeout_ms: None, background }
}

// ---------------------------------------------------------------------------------------------
// Every way in
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_users_message() {
    let rig = Rig::start(Vec::new()).await;
    rig.turn(format!("deploy with {KEY_ID} and {SECRET}").as_str()).await;
    rig.assert_never_leaked();
    assert!(rig.last_context().contains("‹secret:aws-access-key#"), "the agent sees a placeholder");
    let sources: Vec<_> = rig.sink.redactions().into_iter().map(|(_, source, _)| source).collect();
    assert_eq!(sources, [RedactionSource::UserMessage, RedactionSource::UserMessage]);
}

#[tokio::test]
async fn lines_shared_from_the_terminal() {
    let rig = Rig::start(Vec::new()).await;
    rig.turn(UserMessage { text: planted(), send_anyway: Vec::new(), from_terminal: true }).await;
    rig.assert_never_leaked();
    assert!(rig.sink.redactions().iter().all(|(_, source, _)| *source == RedactionSource::TerminalShare));
}

#[tokio::test]
async fn one_item_sent_anyway_reaches_the_agent_but_never_the_record() {
    let rig = Rig::start(Vec::new()).await;
    let text = format!("use {KEY_ID} with {SECRET}");
    let found = rig.scrubber.find(&text);
    let id = found.iter().find(|f| f.kind == "aws-access-key").unwrap().placeholder.clone();
    rig.turn(UserMessage { text, send_anyway: vec![id], from_terminal: false }).await;
    let context = rig.last_context();
    assert!(context.contains(KEY_ID), "the chosen one went as it is");
    assert!(!context.contains("K7MDENG"), "the other did not");
    assert!(!leaks(&rig.store.all()), "the record keeps both placeholders");
    let sent: Vec<_> = rig.sink.redactions().into_iter().map(|(kind, _, sent)| (kind, sent)).collect();
    assert_eq!(sent, [("aws-access-key".to_string(), true), ("aws-secret-key".to_string(), false)]);
}

#[tokio::test]
async fn a_file_read_asks_even_in_auto_and_is_scrubbed() {
    let rig = Rig::start(vec![vec![act(SandboxAction::ReadFile { path: ".env".into() })]]).await;
    rig.turn("read it").await;
    rig.assert_never_leaked();
    let asked = rig.approvals.asked.lock().unwrap().clone();
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].1, Asking::Always, "{asked:?}");
    assert!(rig.sink.redactions().iter().any(|(_, s, _)| *s == RedactionSource::FileRead { path: ".env".into() }));
    // An ordinary file asks by mode.
    let rig = Rig::start(vec![vec![act(SandboxAction::ReadFile { path: "notes.txt".into() })]]).await;
    rig.turn("read it").await;
    assert_eq!(rig.approvals.asked.lock().unwrap()[0].1, Asking::ByMode);
    rig.assert_never_leaked();
}

#[tokio::test]
async fn command_output_and_the_urls_it_requested() {
    let rig = Rig::start(vec![vec![act(exec("cat .env", false))]]).await;
    rig.turn("show it").await;
    rig.assert_never_leaked();
    let sources: Vec<_> = rig.sink.redactions().into_iter().map(|(_, s, _)| s).collect();
    assert!(sources.contains(&RedactionSource::CommandOutput { action: ActionId::new("a1") }), "{sources:?}");
    assert!(sources.contains(&RedactionSource::NetworkRequest { action: ActionId::new("a1") }), "{sources:?}");
}

#[tokio::test]
async fn a_jobs_output_its_status_and_the_note_when_it_ends() {
    let rig = Rig::start(vec![
        vec![act(exec("./deploy.sh", true))],
        vec![act(SandboxAction::JobOutput { job: JobId::new("j1"), tail_bytes: 1000 })],
        vec![act(SandboxAction::JobStatus { job: JobId::new("j1") })],
    ])
    .await;
    rig.turn("deploy in the background").await;
    // The note that the job ended comes at a later step, or between turns.
    for _ in 0..400 {
        if rig.sink.redactions().iter().any(|(_, s, _)| *s == RedactionSource::JobNote) {
            break;
        }
        sleep(Duration::from_millis(5)).await;
    }
    rig.assert_never_leaked();
    let sources: Vec<_> = rig.sink.redactions().into_iter().map(|(_, s, _)| s).collect();
    assert!(sources.contains(&RedactionSource::JobOutput { job: JobId::new("j1") }), "{sources:?}");
    assert!(sources.contains(&RedactionSource::JobNote), "the note that the job ended: {sources:?}");
}

#[tokio::test]
async fn a_patch_diff_a_listing_and_an_error() {
    let patch = SandboxAction::Patch {
        path: "config.ini".into(),
        edits: vec![Edit { search: "a".into(), replace: "b".into() }],
        expected_hash: "h".into(),
    };
    let rig = Rig::start(vec![
        vec![act(patch)],
        vec![act(SandboxAction::ListDir { path: ".".into() })],
        vec![act(SandboxAction::JobStop { job: JobId::new("j1") })],
    ])
    .await;
    rig.turn("fix the config").await;
    rig.assert_never_leaked();
    let sources: Vec<_> = rig.sink.redactions().into_iter().map(|(_, s, _)| s).collect();
    assert!(sources.contains(&RedactionSource::FileEdit { path: "config.ini".into() }), "{sources:?}");
    assert!(sources.contains(&RedactionSource::ActionResult { action: ActionId::new("a1") }), "{sources:?}");
    assert!(sources.contains(&RedactionSource::Notice), "the error event: {sources:?}");
}

#[tokio::test]
async fn knowledge_results_and_the_primer() {
    let call = Intent::Act {
        request: ActionRequest {
            id: ActionId::new("k1"),
            action: Action::Knowledge {
                action: KnowledgeAction::Call {
                    source: SourceId::new("notes"),
                    tool: "search".into(),
                    args: serde_json::json!({}),
                },
            },
            reason: None,
        },
    };
    let rig = Rig::launch(vec![vec![call]], Arc::new(MemoryStore::default()), None, true).await;
    rig.turn("look it up").await;
    rig.assert_never_leaked();
    let context = rig.brain.contexts().last().unwrap().clone();
    assert!(context.primer.sections[0].body.contains("‹secret:aws-access-key#"), "{:?}", context.primer);
    let knowledge = RedactionSource::Knowledge { source: SourceId::new("notes") };
    assert!(rig.sink.redactions().iter().filter(|(_, s, _)| *s == knowledge).count() >= 2);
}

#[tokio::test]
async fn notes_from_nixxa() {
    let rig = Rig::start(Vec::new()).await;
    rig.tx.send(Command::Note(format!("The user's terminal printed {KEY_ID}"))).await.unwrap();
    rig.turn("hi").await;
    rig.assert_never_leaked();
    assert!(rig.sink.redactions().iter().any(|(_, s, _)| *s == RedactionSource::Notice));
}

#[tokio::test]
async fn a_resumed_session_from_before_scrubbing() {
    // Stored by a Nixxa without scrubbing: the user's text, a command, its output, a summary.
    let store = Arc::new(MemoryStore::default());
    let info = store
        .create_session(NewSession {
            workspace: WorkspaceInfo { name: "demo".into(), root: std::env::temp_dir() },
            account: AccountRef::new("recording", "default"),
            started_at: Timestamp(0),
        })
        .await
        .unwrap();
    let request = ActionRequest {
        id: ActionId::new("old"),
        action: Action::Sandbox { action: exec(&format!("aws --key {KEY_ID} s3 ls"), false) },
        reason: None,
    };
    let at = Timestamp(0);
    for message in [
        Message::text(Role::User, format!("my key is {KEY_ID}"), at),
        Message { role: Role::Agent, content: Content::Action { request: request.clone() }, at },
        Message {
            role: Role::Observation,
            content: Content::Observation {
                observation: Observation::new(
                    ActionId::new("old"),
                    Outcome::Exec { exit_code: Some(0), stdout: planted(), stderr: String::new() },
                ),
            },
            at,
        },
        Message { role: Role::Agent, content: Content::Summary { text: format!("used {KEY_ID}") }, at },
    ] {
        store.append_message(&info.id, message).await.unwrap();
    }
    let rig = Rig::launch(Vec::new(), store, Some("s1"), false).await;
    rig.turn("carry on").await;
    for context in rig.brain.contexts() {
        assert!(!leaks(&context), "the brain got the key from the old session: {context:#?}");
    }
    assert!(!leaks(&rig.sink.all()), "the UI was shown the old key again");
    assert!(rig.sink.redactions().iter().all(|(_, s, _)| *s == RedactionSource::ResumedHistory));
    assert!(!rig.sink.redactions().is_empty());
    // What this session adds to the record is scrubbed; the old rows are as they were.
    let record = rig.store.all().remove(0);
    assert!(!leaks(&record.messages[4..].to_vec()), "{:#?}", &record.messages[4..]);
}

#[tokio::test]
async fn a_secret_nixxa_holds_is_found_exactly() {
    let rig = Rig::start(Vec::new()).await;
    let mut known = SecretFingerprints::new([4; 32]);
    known.add("local-key", &Secret::new("no-format-knows-this-one-42"));
    rig.scrubber.set_known(known);
    rig.turn("the proxy password is no-format-knows-this-one-42").await;
    assert!(!rig.last_context().contains("no-format-knows-this-one-42"));
    assert!(!serde_json::to_string(&rig.store.all()).unwrap().contains("no-format-knows-this-one-42"));
    assert_eq!(rig.sink.redactions()[0].0, "local-key");
}

#[tokio::test]
async fn a_placeholder_written_into_a_file_is_flagged_to_the_agent() {
    let rig = Rig::start(Vec::new()).await;
    let placeholder = rig.scrubber.find(KEY_ID)[0].placeholder.clone();
    let write = SandboxAction::WriteFile { path: "deploy.env".into(), content: format!("KEY={placeholder}\n") };
    // A fresh rig so the brain is scripted with the placeholder it will see.
    let rig = Rig::start(vec![vec![act(write)]]).await;
    rig.turn("write the env file").await;
    let finished = rig
        .sink
        .all()
        .into_iter()
        .find_map(|e| match e {
            Event::ActionFinished { observation, .. } => Some(observation),
            _ => None,
        })
        .unwrap();
    assert_eq!(finished.warnings.len(), 1, "{finished:?}");
    assert!(finished.warnings[0].contains("`deploy.env` now contains 1 secret placeholder"), "{finished:?}");
    assert!(rig.last_context().contains("not the secret"), "the agent is told at its next step");
}
