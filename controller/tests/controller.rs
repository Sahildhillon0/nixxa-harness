//! Tests for the agent loop in `nixxa-controller`.
//!
//! The controller may depend only on `nixxa-contracts`, so the fakes it is driven with live here.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use nixxa_contracts::{
    AccountRef, Action, ActionId, ActionRecord, ActionRequest, ApprovalPort, Brain, BrainError, BrainEvent, BrainInfo,
    BrainStream, Content, Context, Event, EventSink, Intent, Knowledge, KnowledgeAction, KnowledgeEntry,
    KnowledgeError, Message, NetRequest, NetResult, NetworkAccess, NetworkPolicy, NewSession, Observation, Outcome,
    Primer, PrimerSection, ProviderId, Role, Sandbox, SandboxAction, SandboxActionKind, SandboxCapabilities,
    SandboxError, SearchHit, SessionId, SessionInfo, SessionRecord, SourceId, SourceInfo, SourceStatus, Store,
    StoreError, Timestamp, ToolSpec, TurnId, Usage, Verdict, WorkspaceInfo,
};
use nixxa_controller::{Command, Controller, ControllerConfig, DEFAULT_MAX_STEPS};
use tokio::sync::{Notify, mpsc};
use tokio::task::JoinHandle;
use tokio::time::{Duration, sleep};

// ---------------------------------------------------------------------------------------------
// Fakes
// ---------------------------------------------------------------------------------------------

/// One scripted think step: the events it yields, then optionally blocks forever.
struct Step {
    events: Vec<Result<BrainEvent, BrainError>>,
    hang: bool,
}

fn step(events: Vec<Result<BrainEvent, BrainError>>) -> Step {
    Step { events, hang: false }
}

fn intent(intent: Intent) -> Result<BrainEvent, BrainError> {
    Ok(BrainEvent::Intent { intent })
}

/// Replays scripted steps and remembers every context it was given.
#[derive(Default)]
struct ScriptedBrain {
    steps: Mutex<VecDeque<Step>>,
    contexts: Mutex<Vec<Context>>,
}

impl ScriptedBrain {
    fn new(steps: Vec<Step>) -> Arc<Self> {
        Arc::new(Self { steps: Mutex::new(steps.into()), contexts: Mutex::default() })
    }

    fn contexts(&self) -> Vec<Context> {
        self.contexts.lock().unwrap().clone()
    }
}

#[async_trait]
impl Brain for ScriptedBrain {
    fn describe(&self) -> BrainInfo {
        BrainInfo { provider: ProviderId::new("scripted"), model: "scripted-1".into() }
    }

    async fn think(&self, context: Context) -> Result<BrainStream, BrainError> {
        self.contexts.lock().unwrap().push(context);
        let step =
            self.steps.lock().unwrap().pop_front().ok_or_else(|| BrainError::Other("script exhausted".into()))?;
        let events = futures::stream::iter(step.events);
        if step.hang {
            Ok(Box::pin(futures::StreamExt::chain(events, futures::stream::pending())))
        } else {
            Ok(Box::pin(events))
        }
    }
}

/// Echoes exec commands back as stdout.
struct EchoSandbox;

#[async_trait]
impl Sandbox for EchoSandbox {
    fn capabilities(&self) -> SandboxCapabilities {
        SandboxCapabilities {
            backend: "fake".into(),
            isolated: false,
            network: NetworkAccess::default(),
            jobs: false,
            actions: SandboxActionKind::ALL.to_vec(),
        }
    }

    async fn execute(&self, id: ActionId, action: SandboxAction) -> Result<Observation, SandboxError> {
        match action {
            SandboxAction::Exec { command, .. } => {
                Ok(Observation::new(id, Outcome::Exec { exit_code: Some(0), stdout: command, stderr: String::new() }))
            }
            other => Err(SandboxError::Unsupported(format!("{:?}", other.kind()))),
        }
    }

    async fn health(&self) -> Result<(), SandboxError> {
        Ok(())
    }

    async fn reset(&self) -> Result<(), SandboxError> {
        Ok(())
    }
}

/// Allows only crates.io, and every command makes one blocked request to example.com.
struct AllowlistSandbox;

fn blocked_request() -> NetRequest {
    NetRequest {
        time: Timestamp(0),
        job: None,
        host: "example.com".into(),
        port: 443,
        method: None,
        url: None,
        bytes_up: 0,
        bytes_down: 0,
        duration_ms: 0,
        result: NetResult::Blocked {
            reason: "host example.com isn't allowed in this session; ask the user to allow it".into(),
        },
    }
}

#[async_trait]
impl Sandbox for AllowlistSandbox {
    fn capabilities(&self) -> SandboxCapabilities {
        let crates = nixxa_contracts::DomainRule::parse("crates.io").unwrap();
        SandboxCapabilities {
            network: NetworkAccess::Controlled(NetworkPolicy::Allow(vec![crates])),
            ..EchoSandbox.capabilities()
        }
    }

    async fn execute(&self, id: ActionId, action: SandboxAction) -> Result<Observation, SandboxError> {
        let mut observation = EchoSandbox.execute(id, action).await?;
        observation.network.push(blocked_request());
        Ok(observation)
    }

    async fn health(&self) -> Result<(), SandboxError> {
        Ok(())
    }

    async fn reset(&self) -> Result<(), SandboxError> {
        Ok(())
    }
}

/// Never finishes an action.
struct HangingSandbox;

#[async_trait]
impl Sandbox for HangingSandbox {
    fn capabilities(&self) -> SandboxCapabilities {
        EchoSandbox.capabilities()
    }

    async fn execute(&self, _id: ActionId, _action: SandboxAction) -> Result<Observation, SandboxError> {
        std::future::pending().await
    }

    async fn health(&self) -> Result<(), SandboxError> {
        Ok(())
    }

    async fn reset(&self) -> Result<(), SandboxError> {
        Ok(())
    }
}

/// Fails every action with an infrastructure error.
struct FailingSandbox;

#[async_trait]
impl Sandbox for FailingSandbox {
    fn capabilities(&self) -> SandboxCapabilities {
        EchoSandbox.capabilities()
    }

    async fn execute(&self, _id: ActionId, _action: SandboxAction) -> Result<Observation, SandboxError> {
        Err(SandboxError::Timeout)
    }

    async fn health(&self) -> Result<(), SandboxError> {
        Err(SandboxError::Timeout)
    }

    async fn reset(&self) -> Result<(), SandboxError> {
        Err(SandboxError::Timeout)
    }
}

/// An in-memory store.
#[derive(Default)]
struct MemoryStore {
    sessions: Mutex<Vec<SessionRecord>>,
    /// Makes `append_action` slow, like a real database.
    action_delay: Option<Duration>,
}

impl MemoryStore {
    fn slow_actions(delay: Duration) -> Arc<Self> {
        Arc::new(Self { action_delay: Some(delay), ..Self::default() })
    }

    fn record(&self, id: &str) -> SessionRecord {
        self.sessions.lock().unwrap().iter().find(|s| s.info.id.as_str() == id).cloned().expect("session")
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
        self.with(id, |s| {
            if let (None, Role::User, Content::Text { text }) = (&s.info.first_message, message.role, &message.content)
            {
                s.info.first_message = Some(text.clone());
            }
            s.messages.push(message)
        })
    }

    async fn append_action(&self, id: &SessionId, record: ActionRecord) -> Result<(), StoreError> {
        if let Some(delay) = self.action_delay {
            sleep(delay).await;
        }
        self.with(id, |s| s.actions.push(record))
    }

    async fn list_sessions(&self) -> Result<Vec<SessionInfo>, StoreError> {
        Ok(self.sessions.lock().unwrap().iter().rev().map(|s| s.info.clone()).collect())
    }

    async fn search(&self, _query: &str) -> Result<Vec<SearchHit>, StoreError> {
        Ok(Vec::new())
    }
}

/// `notes` is ready with a `search` tool, `vault` is missing.
struct FakeKnowledge;

#[async_trait]
impl Knowledge for FakeKnowledge {
    async fn sources(&self) -> Vec<SourceInfo> {
        vec![
            SourceInfo { id: SourceId::new("notes"), name: "Notes".into(), status: SourceStatus::Ready },
            SourceInfo { id: SourceId::new("vault"), name: "Vault".into(), status: SourceStatus::Missing },
        ]
    }

    async fn tools(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            source: SourceId::new("notes"),
            name: "search".into(),
            description: "Search notes.".into(),
            input_schema: serde_json::json!({ "type": "object" }),
            mutating: false,
        }]
    }

    async fn primer(&self, _workspace: &WorkspaceInfo) -> Result<Primer, KnowledgeError> {
        Ok(Primer {
            sections: vec![PrimerSection {
                source: SourceId::new("notes"),
                title: "Recent memory".into(),
                body: "primer body".into(),
            }],
        })
    }

    async fn call(
        &self,
        id: ActionId,
        source: &SourceId,
        tool: &str,
        _args: serde_json::Value,
    ) -> Result<Observation, KnowledgeError> {
        if source.as_str() != "notes" {
            return Err(KnowledgeError::SourceUnavailable(source.clone()));
        }
        Ok(Observation::new(id, Outcome::Text { content: format!("{tool} result") }))
    }

    async fn record(
        &self,
        id: ActionId,
        _source: &SourceId,
        _entry: KnowledgeEntry,
    ) -> Result<Observation, KnowledgeError> {
        Ok(Observation::new(id, Outcome::Recorded { location: None }))
    }
}

/// Answers from a queue of verdicts (allowing once it runs dry), or never answers.
#[derive(Default)]
struct ScriptedApprovals {
    decisions: Mutex<VecDeque<Verdict>>,
    hang: bool,
    asked: Mutex<Vec<String>>,
    /// When set, each answer waits for a [`ScriptedApprovals::release`].
    gate: Option<tokio::sync::Semaphore>,
}

impl ScriptedApprovals {
    fn answering(decisions: Vec<Verdict>) -> Arc<Self> {
        Arc::new(Self { decisions: Mutex::new(decisions.into()), ..Self::default() })
    }

    fn allowing() -> Arc<Self> {
        Self::answering(Vec::new())
    }

    fn hanging() -> Arc<Self> {
        Arc::new(Self { hang: true, ..Self::default() })
    }

    fn holding() -> Arc<Self> {
        Arc::new(Self { gate: Some(tokio::sync::Semaphore::new(0)), ..Self::default() })
    }

    /// Lets one held prompt be answered with `verdict`.
    fn release(&self, verdict: Verdict) {
        self.decisions.lock().unwrap().push_back(verdict);
        if let Some(gate) = &self.gate {
            gate.add_permits(1);
        }
    }

    /// Waits until `n` prompts have been asked.
    async fn wait_asked(&self, n: usize) {
        while self.asked.lock().unwrap().len() < n {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}

#[async_trait]
impl ApprovalPort for ScriptedApprovals {
    async fn ask(&self, _request: &ActionRequest, reason: &str) -> Verdict {
        self.asked.lock().unwrap().push(reason.to_string());
        if self.hang {
            Notify::new().notified().await;
        }
        if let Some(gate) = &self.gate {
            if let Ok(permit) = gate.acquire().await {
                permit.forget();
            }
        }
        self.decisions.lock().unwrap().pop_front().unwrap_or(Verdict::Allow)
    }
}

/// Collects every event.
#[derive(Default)]
struct RecordingSink {
    events: Mutex<Vec<Event>>,
}

impl EventSink for RecordingSink {
    fn emit(&self, event: Event) {
        self.events.lock().unwrap().push(event);
    }
}

impl RecordingSink {
    fn all(&self) -> Vec<Event> {
        self.events.lock().unwrap().clone()
    }

    fn count(&self, predicate: impl Fn(&Event) -> bool) -> usize {
        self.events.lock().unwrap().iter().filter(|e| predicate(e)).count()
    }

    /// Waits until `predicate` matches `n` events.
    async fn wait(&self, n: usize, predicate: impl Fn(&Event) -> bool) {
        for _ in 0..400 {
            if self.count(&predicate) >= n {
                return;
            }
            sleep(Duration::from_millis(5)).await;
        }
        panic!("timed out waiting for events; got {:#?}", self.all());
    }
}

// ---------------------------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------------------------

fn turn_done(event: &Event) -> bool {
    matches!(event, Event::TurnDone { .. })
}

struct Rig {
    brain: Arc<ScriptedBrain>,
    approvals: Arc<ScriptedApprovals>,
    store: Arc<MemoryStore>,
    sink: Arc<RecordingSink>,
    tx: mpsc::Sender<Command>,
    task: JoinHandle<()>,
}

impl Rig {
    async fn start(
        brain: Arc<ScriptedBrain>,
        approvals: Arc<ScriptedApprovals>,
        sandbox: Arc<dyn Sandbox>,
        max_steps: u32,
    ) -> Self {
        Self::start_with_store(brain, approvals, sandbox, max_steps, Arc::new(MemoryStore::default())).await
    }

    async fn start_with_store(
        brain: Arc<ScriptedBrain>,
        approvals: Arc<ScriptedApprovals>,
        sandbox: Arc<dyn Sandbox>,
        max_steps: u32,
        store: Arc<MemoryStore>,
    ) -> Self {
        Self::launch(brain, approvals, sandbox, max_steps, store, None).await
    }

    /// Resumes the stored session `id` with the scripted account.
    async fn resume(
        brain: Arc<ScriptedBrain>,
        approvals: Arc<ScriptedApprovals>,
        store: Arc<MemoryStore>,
        id: &str,
    ) -> Self {
        Self::launch(brain, approvals, Arc::new(EchoSandbox), DEFAULT_MAX_STEPS, store, Some(SessionId::new(id))).await
    }

    async fn launch(
        brain: Arc<ScriptedBrain>,
        approvals: Arc<ScriptedApprovals>,
        sandbox: Arc<dyn Sandbox>,
        max_steps: u32,
        store: Arc<MemoryStore>,
        resume: Option<SessionId>,
    ) -> Self {
        let sink = Arc::new(RecordingSink::default());
        let mut config = ControllerConfig::new(
            WorkspaceInfo { name: "demo".into(), root: std::env::temp_dir().join("nixxa-controller-test") },
            AccountRef::new("scripted", "default"),
            "Reply briefly.",
        );
        config.max_steps = max_steps;
        config.clock = || Timestamp(0);
        let mut controller = Controller::new(
            brain.clone(),
            sandbox,
            store.clone(),
            Arc::new(FakeKnowledge),
            approvals.clone(),
            sink.clone(),
            config,
        );
        let info = match &resume {
            None => controller.start_session().await.expect("start session"),
            Some(id) => controller.resume_session(id).await.expect("resume session"),
        };
        assert_eq!(info.id.as_str(), resume.as_ref().map_or("s1", SessionId::as_str));
        let (tx, rx) = mpsc::channel(8);
        let task = tokio::spawn(async move { controller.run(rx).await });
        Self { brain, approvals, store, sink, tx, task }
    }

    async fn say(&self, text: &str) {
        self.tx.send(Command::UserMessage(text.into())).await.expect("send message");
    }

    /// Sends one message and returns the events of that turn (from `TurnStarted` to `TurnDone`).
    async fn turn(&self, text: &str) -> Vec<Event> {
        let done = self.sink.count(turn_done);
        self.say(text).await;
        self.sink.wait(done + 1, turn_done).await;
        let events = self.sink.all();
        let start = events.iter().rposition(|e| matches!(e, Event::TurnStarted { .. })).expect("turn started");
        events[start..].to_vec()
    }
}

fn t0() -> TurnId {
    TurnId::new("turn-0")
}

fn exec_request(id: &str, command: &str) -> ActionRequest {
    ActionRequest {
        id: ActionId::new(id),
        action: Action::Sandbox {
            action: SandboxAction::Exec { command: command.into(), cwd: None, timeout_ms: None, background: false },
        },
        reason: Some("run it".into()),
    }
}

fn search_request(id: &str) -> ActionRequest {
    ActionRequest {
        id: ActionId::new(id),
        action: Action::Knowledge {
            action: KnowledgeAction::Call {
                source: SourceId::new("notes"),
                tool: "search".into(),
                args: serde_json::json!({ "query": "hello" }),
            },
        },
        reason: Some("look it up".into()),
    }
}

fn exec_observation(id: &str, stdout: &str) -> Observation {
    Observation::new(
        ActionId::new(id),
        Outcome::Exec { exit_code: Some(0), stdout: stdout.into(), stderr: String::new() },
    )
}

fn text(role: Role, text: &str) -> Message {
    Message::text(role, text, Timestamp(0))
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn session_start_reports_brain_sandbox_and_sources() {
    let rig = Rig::start(ScriptedBrain::new(Vec::new()), ScriptedApprovals::allowing(), Arc::new(EchoSandbox), 8).await;
    assert_eq!(
        rig.sink.all(),
        vec![
            Event::SessionStarted {
                session: SessionId::new("s1"),
                account: AccountRef::new("scripted", "default"),
                brain: BrainInfo { provider: ProviderId::new("scripted"), model: "scripted-1".into() },
            },
            Event::SandboxStatus { capabilities: EchoSandbox.capabilities() },
            Event::SourceStatus {
                source: SourceInfo { id: SourceId::new("notes"), name: "Notes".into(), status: SourceStatus::Ready }
            },
            Event::SourceStatus {
                source: SourceInfo { id: SourceId::new("vault"), name: "Vault".into(), status: SourceStatus::Missing }
            },
        ]
    );
}

#[tokio::test]
async fn happy_path_asks_runs_and_finishes() {
    let brain = ScriptedBrain::new(vec![
        step(vec![intent(Intent::Act { request: exec_request("act-1", "echo hi") })]),
        step(vec![
            Ok(BrainEvent::TextDelta { text: "The command finished.".into() }),
            intent(Intent::Say { text: "The command finished.".into() }),
            intent(Intent::Done { summary: "ran the command".into() }),
            Ok(BrainEvent::Usage { usage: Usage::default() }),
        ]),
    ]);
    let rig = Rig::start(brain, ScriptedApprovals::allowing(), Arc::new(EchoSandbox), DEFAULT_MAX_STEPS).await;
    let events = rig.turn("hello").await;

    assert_eq!(
        events,
        vec![
            Event::TurnStarted { turn: t0() },
            Event::Message { turn: t0(), message: text(Role::User, "hello") },
            Event::ActionProposed { turn: t0(), request: exec_request("act-1", "echo hi") },
            Event::ApprovalNeeded {
                turn: t0(),
                request: exec_request("act-1", "echo hi"),
                reason: "approval needed to run `echo hi` in the sandbox".into(),
            },
            Event::ActionStarted { turn: t0(), action_id: ActionId::new("act-1") },
            Event::ActionFinished { turn: t0(), observation: exec_observation("act-1", "echo hi") },
            Event::TextDelta { turn: t0(), text: "The command finished.".into() },
            Event::Message { turn: t0(), message: text(Role::Agent, "The command finished.") },
            Event::Usage { account: AccountRef::new("scripted", "default"), usage: Usage::default() },
            Event::TurnDone { turn: t0() },
        ]
    );
    assert_eq!(*rig.approvals.asked.lock().unwrap(), vec!["approval needed to run `echo hi` in the sandbox"]);

    // The second think step saw the proposal followed by its observation, plus the primer.
    let contexts = rig.brain.contexts();
    assert_eq!(contexts.len(), 2);
    assert_eq!(contexts[1].primer.sections[0].body, "primer body");
    assert_eq!(contexts[1].knowledge_tools.len(), 1);
    assert_eq!(
        contexts[1].messages,
        vec![
            text(Role::User, "hello"),
            Message {
                role: Role::Agent,
                content: Content::Action { request: exec_request("act-1", "echo hi") },
                at: Timestamp(0)
            },
            Message {
                role: Role::Observation,
                content: Content::Observation { observation: exec_observation("act-1", "echo hi") },
                at: Timestamp(0)
            },
        ]
    );

    // Everything was persisted.
    let record = rig.store.record("s1");
    assert_eq!(record.messages.len(), 5, "user, proposal, observation, say, done");
    assert_eq!(record.actions.len(), 1);
    assert_eq!(record.actions[0].verdict, Verdict::Allow);
    assert_eq!(record.actions[0].observation, Some(exec_observation("act-1", "echo hi")));
}

#[tokio::test]
async fn reasoning_is_recorded_in_order_but_never_shown() {
    let reasoning = || BrainEvent::Reasoning {
        provider: ProviderId::new("scripted"),
        data: serde_json::json!({ "type": "thinking", "signature": "sig" }),
    };
    let brain = ScriptedBrain::new(vec![
        step(vec![Ok(reasoning()), intent(Intent::Act { request: exec_request("act-1", "echo hi") })]),
        step(vec![intent(Intent::Done { summary: "ok".into() })]),
    ]);
    let rig = Rig::start(brain, ScriptedApprovals::allowing(), Arc::new(EchoSandbox), DEFAULT_MAX_STEPS).await;
    let events = rig.turn("hello").await;

    assert!(
        !events.iter().any(
            |e| matches!(e, Event::Message { message, .. } if matches!(message.content, Content::Reasoning { .. }))
        ),
        "reasoning is never announced"
    );
    let reasoning_message = Message {
        role: Role::Agent,
        content: Content::Reasoning {
            provider: ProviderId::new("scripted"),
            data: serde_json::json!({ "type": "thinking", "signature": "sig" }),
        },
        at: Timestamp(0),
    };
    let proposal = Message {
        role: Role::Agent,
        content: Content::Action { request: exec_request("act-1", "echo hi") },
        at: Timestamp(0),
    };
    let next = &rig.brain.contexts()[1].messages;
    assert_eq!(next[1..3], [reasoning_message.clone(), proposal], "reasoning precedes the tool call it came with");
    assert_eq!(rig.store.record("s1").messages[1], reasoning_message, "and is persisted");
}

#[tokio::test]
async fn knowledge_actions_route_to_knowledge() {
    let brain = ScriptedBrain::new(vec![
        step(vec![intent(Intent::Act { request: search_request("k-1") })]),
        step(vec![intent(Intent::Act { request: exec_request("x-1", "echo found") })]),
        step(vec![intent(Intent::Done { summary: "done".into() })]),
    ]);
    let rig = Rig::start(brain, ScriptedApprovals::allowing(), Arc::new(EchoSandbox), DEFAULT_MAX_STEPS).await;
    let events = rig.turn("find it").await;

    let finished: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            Event::ActionFinished { observation, .. } => Some(observation.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        finished,
        vec![
            Observation::new(ActionId::new("k-1"), Outcome::Text { content: "search result".into() }),
            exec_observation("x-1", "echo found"),
        ]
    );
    assert!(events.contains(&Event::ApprovalNeeded {
        turn: t0(),
        request: search_request("k-1"),
        reason: "approval needed to call `search` on notes".into(),
    }));
}

#[tokio::test]
async fn denied_action_is_observed_and_the_turn_continues() {
    let brain = ScriptedBrain::new(vec![
        step(vec![intent(Intent::Act { request: exec_request("act-1", "echo hi") })]),
        step(vec![intent(Intent::Done { summary: "gave up".into() })]),
    ]);
    let rig = Rig::start(
        brain,
        ScriptedApprovals::answering(vec![Verdict::Deny("no".into())]),
        Arc::new(EchoSandbox),
        DEFAULT_MAX_STEPS,
    )
    .await;
    let events = rig.turn("hello").await;

    assert!(!events.iter().any(|e| matches!(e, Event::ActionStarted { .. })));
    assert!(events.contains(&Event::ActionFinished {
        turn: t0(),
        observation: Observation::denied(ActionId::new("act-1"), "no")
    }));
    assert_eq!(rig.brain.contexts().len(), 2, "the brain saw the denial and finished");
    assert_eq!(rig.store.record("s1").actions[0].verdict, Verdict::Deny("no".into()));
}

#[tokio::test]
async fn an_ask_from_the_approval_port_counts_as_a_denial() {
    let brain = ScriptedBrain::new(vec![
        step(vec![intent(Intent::Act { request: exec_request("act-1", "echo hi") })]),
        step(vec![intent(Intent::Done { summary: "ok".into() })]),
    ]);
    let rig = Rig::start(
        brain,
        ScriptedApprovals::answering(vec![Verdict::Ask("unsure".into())]),
        Arc::new(EchoSandbox),
        DEFAULT_MAX_STEPS,
    )
    .await;
    let events = rig.turn("hello").await;

    assert!(!events.iter().any(|e| matches!(e, Event::ActionStarted { .. })));
    assert_eq!(rig.store.record("s1").actions[0].verdict, Verdict::Deny("unsure".into()));
}

#[tokio::test]
async fn brain_error_ends_the_turn() {
    let brain = ScriptedBrain::new(vec![step(vec![Err(BrainError::Provider("boom".into()))])]);
    let rig = Rig::start(brain, ScriptedApprovals::allowing(), Arc::new(EchoSandbox), DEFAULT_MAX_STEPS).await;
    let events = rig.turn("hello").await;

    assert_eq!(
        events[2..],
        [Event::Error { turn: Some(t0()), message: "provider error: boom".into() }, Event::TurnDone { turn: t0() }]
    );
}

#[tokio::test]
async fn a_step_without_an_intent_ends_the_turn() {
    let brain = ScriptedBrain::new(vec![step(vec![Ok(BrainEvent::TextDelta { text: "hm".into() })])]);
    let rig = Rig::start(brain, ScriptedApprovals::allowing(), Arc::new(EchoSandbox), DEFAULT_MAX_STEPS).await;
    let events = rig.turn("hello").await;

    assert_eq!(
        events[events.len() - 2],
        Event::Error { turn: Some(t0()), message: "the brain ended its step without an intent".into() }
    );
}

#[tokio::test]
async fn sandbox_failure_ends_the_turn() {
    let brain = ScriptedBrain::new(vec![
        step(vec![intent(Intent::Act { request: exec_request("act-1", "echo hi") })]),
        step(vec![intent(Intent::Done { summary: "never".into() })]),
    ]);
    let rig = Rig::start(brain, ScriptedApprovals::allowing(), Arc::new(FailingSandbox), DEFAULT_MAX_STEPS).await;
    let events = rig.turn("hello").await;

    assert_eq!(
        events[4..],
        [
            Event::ActionStarted { turn: t0(), action_id: ActionId::new("act-1") },
            Event::Error { turn: Some(t0()), message: "timed out".into() },
            Event::ActionFinished { turn: t0(), observation: Observation::failed(ActionId::new("act-1"), "timed out") },
            Event::TurnDone { turn: t0() },
        ]
    );
    assert_eq!(rig.brain.contexts().len(), 1, "the brain is not asked again");
}

#[tokio::test]
async fn the_step_budget_bounds_the_turn_and_says_so() {
    let brain = ScriptedBrain::new(vec![
        step(vec![intent(Intent::Act { request: exec_request("act-1", "echo hi") })]),
        step(vec![intent(Intent::Act { request: exec_request("act-2", "echo bye") })]),
        step(vec![intent(Intent::Done { summary: "never reached".into() })]),
    ]);
    let rig = Rig::start(brain, ScriptedApprovals::allowing(), Arc::new(EchoSandbox), 2).await;
    let events = rig.turn("hello").await;

    assert_eq!(rig.brain.contexts().len(), 2);
    assert_eq!(
        events[events.len() - 2..],
        [
            Event::Message {
                turn: t0(),
                message: text(Role::System, "stopped: the step budget of 2 for this turn ran out")
            },
            Event::TurnDone { turn: t0() },
        ]
    );
}

#[tokio::test]
async fn the_conversation_carries_across_turns() {
    let brain = ScriptedBrain::new(vec![
        step(vec![intent(Intent::Say { text: "hi there".into() }), intent(Intent::Done { summary: "greeted".into() })]),
        step(vec![intent(Intent::Done { summary: "still here".into() })]),
    ]);
    let rig = Rig::start(brain, ScriptedApprovals::allowing(), Arc::new(EchoSandbox), DEFAULT_MAX_STEPS).await;
    rig.turn("hello").await;
    let second = rig.turn("again").await;

    assert_eq!(second[0], Event::TurnStarted { turn: TurnId::new("turn-1") });
    assert_eq!(
        rig.brain.contexts()[1].messages,
        vec![text(Role::User, "hello"), text(Role::Agent, "hi there"), text(Role::User, "again")]
    );
}

/// Regression (PR #6 review, B2): the `Done` summary is not part of the model's message, so the
/// brain must never see it; it is still stored.
#[tokio::test]
async fn the_done_summary_is_stored_but_never_sent_back_to_the_brain() {
    let brain = ScriptedBrain::new(vec![
        step(vec![intent(Intent::Say { text: "Hello!".into() }), intent(Intent::Done { summary: "done".into() })]),
        step(vec![intent(Intent::Done { summary: "done".into() })]),
    ]);
    let rig = Rig::start(brain, ScriptedApprovals::allowing(), Arc::new(EchoSandbox), DEFAULT_MAX_STEPS).await;
    rig.turn("hi").await;
    rig.turn("again").await;

    let seen = &rig.brain.contexts()[1].messages;
    assert_eq!(seen[1], text(Role::Agent, "Hello!"), "the model's own last block ends its turn");
    assert!(!seen.iter().any(|m| m.content == summary("done").content), "{seen:?}");
    let stored = rig.store.record("s1").messages;
    assert!(stored.contains(&summary("done")), "but it is stored, marked as a summary (D-029): {stored:?}");
    assert!(!stored.contains(&text(Role::Agent, "done")), "and never as agent text");
}

fn summary(text: &str) -> Message {
    Message { role: Role::Agent, content: Content::Summary { text: text.into() }, at: Timestamp(0) }
}

fn reasoning_message(signature: &str) -> Message {
    Message {
        role: Role::Agent,
        content: Content::Reasoning {
            provider: ProviderId::new("scripted"),
            data: serde_json::json!({ "type": "thinking", "signature": signature }),
        },
        at: Timestamp(0),
    }
}

fn proposal(request: ActionRequest) -> Message {
    Message { role: Role::Agent, content: Content::Action { request }, at: Timestamp(0) }
}

fn observed(observation: Observation) -> Message {
    Message { role: Role::Observation, content: Content::Observation { observation }, at: Timestamp(0) }
}

/// Regression (PR #6 review, B1): one step's parallel calls stay one assistant message: every
/// proposal is recorded before any action runs, and the observations follow together.
#[tokio::test]
async fn parallel_actions_are_recorded_together_before_their_results() {
    let brain = ScriptedBrain::new(vec![
        step(vec![
            Ok(BrainEvent::Reasoning {
                provider: ProviderId::new("scripted"),
                data: serde_json::json!({ "type": "thinking", "signature": "sig" }),
            }),
            intent(Intent::Say { text: "Running both.".into() }),
            intent(Intent::Act { request: exec_request("a1", "echo one") }),
            intent(Intent::Act { request: exec_request("a2", "echo two") }),
        ]),
        step(vec![intent(Intent::Done { summary: "done".into() })]),
    ]);
    let rig = Rig::start(brain, ScriptedApprovals::allowing(), Arc::new(EchoSandbox), DEFAULT_MAX_STEPS).await;
    rig.turn("go").await;

    assert_eq!(
        rig.brain.contexts()[1].messages,
        vec![
            text(Role::User, "go"),
            reasoning_message("sig"),
            text(Role::Agent, "Running both."),
            proposal(exec_request("a1", "echo one")),
            proposal(exec_request("a2", "echo two")),
            observed(exec_observation("a1", "echo one")),
            observed(exec_observation("a2", "echo two")),
        ]
    );
}

#[tokio::test]
async fn an_interrupt_during_the_first_approval_still_closes_the_second_call() {
    let brain = ScriptedBrain::new(vec![step(vec![
        intent(Intent::Act { request: exec_request("a1", "echo one") }),
        intent(Intent::Act { request: exec_request("a2", "echo two") }),
    ])]);
    let rig = Rig::start(brain, ScriptedApprovals::hanging(), Arc::new(EchoSandbox), DEFAULT_MAX_STEPS).await;
    rig.say("go").await;
    rig.sink.wait(1, |e| matches!(e, Event::ApprovalNeeded { .. })).await;
    rig.tx.send(Command::Interrupt).await.unwrap();
    rig.sink.wait(1, turn_done).await;

    let record = rig.store.record("s1");
    let closed: Vec<_> =
        record.actions.iter().map(|a| (a.request.id.as_str().to_string(), a.verdict.clone())).collect();
    assert_eq!(
        closed,
        vec![
            ("a1".to_string(), Verdict::Deny("interrupted by the user".into())),
            ("a2".to_string(), Verdict::Deny("interrupted by the user".into())),
        ]
    );
    let observations = record.messages.iter().filter(|m| matches!(m.content, Content::Observation { .. })).count();
    assert_eq!(observations, 2, "every proposal has a result");
}

#[tokio::test]
async fn a_failed_first_call_closes_the_calls_after_it() {
    let brain = ScriptedBrain::new(vec![step(vec![
        intent(Intent::Act { request: exec_request("a1", "echo one") }),
        intent(Intent::Act { request: exec_request("a2", "echo two") }),
    ])]);
    let rig = Rig::start(brain, ScriptedApprovals::allowing(), Arc::new(FailingSandbox), DEFAULT_MAX_STEPS).await;
    rig.turn("go").await;

    let record = rig.store.record("s1");
    assert_eq!(record.actions.len(), 2);
    assert_eq!(record.actions[0].observation, Some(Observation::failed(ActionId::new("a1"), "timed out")));
    assert_eq!(
        record.actions[1].observation,
        Some(Observation::denied(ActionId::new("a2"), "not run: the turn stopped before this action"))
    );
}

#[tokio::test]
async fn messages_sent_during_a_turn_are_queued() {
    let brain = ScriptedBrain::new(vec![
        step(vec![intent(Intent::Act { request: exec_request("act-1", "echo hi") })]),
        step(vec![intent(Intent::Done { summary: "first".into() })]),
        step(vec![intent(Intent::Done { summary: "second".into() })]),
    ]);
    let rig = Rig::start(brain, ScriptedApprovals::allowing(), Arc::new(EchoSandbox), DEFAULT_MAX_STEPS).await;
    rig.say("one").await;
    rig.say("two").await;
    rig.sink.wait(2, turn_done).await;

    let starts: Vec<_> = rig
        .sink
        .all()
        .into_iter()
        .filter(|e| matches!(e, Event::TurnStarted { .. } | Event::TurnDone { .. }))
        .collect();
    assert_eq!(
        starts,
        vec![
            Event::TurnStarted { turn: t0() },
            Event::TurnDone { turn: t0() },
            Event::TurnStarted { turn: TurnId::new("turn-1") },
            Event::TurnDone { turn: TurnId::new("turn-1") },
        ],
        "turns never overlap"
    );
}

#[tokio::test]
async fn interrupt_cancels_a_streaming_turn_and_the_next_turn_still_runs() {
    let brain = ScriptedBrain::new(vec![
        Step { events: vec![Ok(BrainEvent::TextDelta { text: "Hold on.".into() })], hang: true },
        step(vec![intent(Intent::Done { summary: "back".into() })]),
    ]);
    let rig = Rig::start(brain, ScriptedApprovals::allowing(), Arc::new(EchoSandbox), DEFAULT_MAX_STEPS).await;
    rig.say("hello").await;
    rig.sink.wait(1, |e| matches!(e, Event::TextDelta { .. })).await;
    rig.tx.send(Command::Interrupt).await.unwrap();
    rig.sink.wait(1, turn_done).await;

    let events = rig.sink.all();
    assert_eq!(
        events[events.len() - 2..],
        [
            Event::Message { turn: t0(), message: text(Role::System, "interrupted by the user") },
            Event::TurnDone { turn: t0() },
        ]
    );

    let next = rig.turn("again").await;
    assert_eq!(next.last(), Some(&Event::TurnDone { turn: TurnId::new("turn-1") }));
    assert_eq!(
        rig.brain.contexts()[1].messages,
        vec![text(Role::User, "hello"), text(Role::System, "interrupted by the user"), text(Role::User, "again")]
    );
}

#[tokio::test]
async fn interrupt_while_awaiting_approval_records_a_denial() {
    let brain = ScriptedBrain::new(vec![step(vec![intent(Intent::Act { request: exec_request("act-1", "echo hi") })])]);
    let rig = Rig::start(brain, ScriptedApprovals::hanging(), Arc::new(EchoSandbox), DEFAULT_MAX_STEPS).await;
    rig.say("hello").await;
    rig.sink.wait(1, |e| matches!(e, Event::ApprovalNeeded { .. })).await;
    rig.tx.send(Command::Interrupt).await.unwrap();
    rig.sink.wait(1, turn_done).await;

    let denied = Observation::denied(ActionId::new("act-1"), "interrupted by the user");
    assert!(rig.sink.all().contains(&Event::ActionFinished { turn: t0(), observation: denied.clone() }));
    let record = rig.store.record("s1");
    assert_eq!(record.actions.len(), 1);
    assert_eq!(record.actions[0].verdict, Verdict::Deny("interrupted by the user".into()));
    assert_eq!(record.actions[0].observation, Some(denied));
}

#[tokio::test]
async fn shutdown_cancels_the_turn_and_ends_the_session() {
    let brain = ScriptedBrain::new(vec![Step { events: Vec::new(), hang: true }]);
    let rig = Rig::start(brain, ScriptedApprovals::allowing(), Arc::new(EchoSandbox), DEFAULT_MAX_STEPS).await;
    rig.say("hello").await;
    rig.sink.wait(1, |e| matches!(e, Event::TurnStarted { .. })).await;
    rig.tx.send(Command::Shutdown).await.unwrap();
    rig.task.await.expect("loop ends");

    let events = rig.sink.all();
    assert_eq!(
        events[events.len() - 2..],
        [Event::TurnDone { turn: t0() }, Event::SessionEnded { session: SessionId::new("s1") }]
    );
}

#[tokio::test]
async fn dropping_the_sender_ends_the_session() {
    let rig = Rig::start(ScriptedBrain::new(Vec::new()), ScriptedApprovals::allowing(), Arc::new(EchoSandbox), 8).await;
    drop(rig.tx);
    rig.task.await.expect("loop ends");
    assert_eq!(rig.sink.all().last(), Some(&Event::SessionEnded { session: SessionId::new("s1") }));
}

#[tokio::test]
async fn done_is_recorded_but_not_announced() {
    let brain = ScriptedBrain::new(vec![step(vec![
        intent(Intent::Say { text: "All set.".into() }),
        intent(Intent::Done { summary: "internal summary".into() }),
    ])]);
    let rig = Rig::start(brain, ScriptedApprovals::allowing(), Arc::new(EchoSandbox), DEFAULT_MAX_STEPS).await;
    let events = rig.turn("hello").await;

    let shown: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            Event::Message { message, .. } => Some(message.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(shown, vec![text(Role::User, "hello"), text(Role::Agent, "All set.")]);
    assert_eq!(rig.store.record("s1").messages.last(), Some(&summary("internal summary")));
}

#[tokio::test]
async fn interrupt_while_running_an_action_records_a_failure_and_the_next_turn_runs() {
    let brain = ScriptedBrain::new(vec![
        step(vec![intent(Intent::Act { request: exec_request("act-1", "sleep 100") })]),
        step(vec![intent(Intent::Done { summary: "next".into() })]),
    ]);
    let rig = Rig::start(brain, ScriptedApprovals::allowing(), Arc::new(HangingSandbox), DEFAULT_MAX_STEPS).await;
    rig.say("hello").await;
    rig.sink.wait(1, |e| matches!(e, Event::ActionStarted { .. })).await;
    rig.tx.send(Command::Interrupt).await.unwrap();
    rig.sink.wait(1, turn_done).await;

    let failed = Observation::failed(ActionId::new("act-1"), "interrupted by the user");
    assert!(rig.sink.all().contains(&Event::ActionFinished { turn: t0(), observation: failed.clone() }));
    let record = rig.store.record("s1");
    assert_eq!(record.actions.len(), 1);
    assert_eq!(record.actions[0].verdict, Verdict::Allow);
    assert_eq!(record.actions[0].observation, Some(failed));

    let next = rig.turn("again").await;
    assert_eq!(next.last(), Some(&Event::TurnDone { turn: TurnId::new("turn-1") }));
}

/// Regression (PR #3 review, B1): an interrupt that lands while the store is writing the action
/// record must not leave a proposal without its observation.
#[tokio::test]
async fn interrupt_during_a_slow_store_write_keeps_the_action_whole() {
    let brain = ScriptedBrain::new(vec![
        step(vec![intent(Intent::Act { request: exec_request("act-1", "echo hi") })]),
        step(vec![intent(Intent::Done { summary: "next".into() })]),
    ]);
    let rig = Rig::start_with_store(
        brain,
        ScriptedApprovals::allowing(),
        Arc::new(EchoSandbox),
        DEFAULT_MAX_STEPS,
        MemoryStore::slow_actions(Duration::from_millis(300)),
    )
    .await;
    rig.say("hello").await;
    rig.sink.wait(1, |e| matches!(e, Event::ActionStarted { .. })).await;
    rig.tx.send(Command::Interrupt).await.unwrap();
    rig.sink.wait(1, turn_done).await;

    assert!(
        rig.sink
            .all()
            .contains(&Event::ActionFinished { turn: t0(), observation: exec_observation("act-1", "echo hi") })
    );
    let record = rig.store.record("s1");
    assert_eq!(record.actions.len(), 1, "the action record was written");
    assert_eq!(record.actions[0].observation, Some(exec_observation("act-1", "echo hi")));

    rig.turn("again").await;
    let messages = &rig.brain.contexts()[1].messages;
    let proposal = Message {
        role: Role::Agent,
        content: Content::Action { request: exec_request("act-1", "echo hi") },
        at: Timestamp(0),
    };
    let observation = Message {
        role: Role::Observation,
        content: Content::Observation { observation: exec_observation("act-1", "echo hi") },
        at: Timestamp(0),
    };
    let at = messages.iter().position(|m| m == &proposal).expect("proposal in history");
    assert_eq!(messages[at + 1], observation, "the observation follows its proposal");
    assert_eq!(messages.last(), Some(&text(Role::User, "again")));
}

// ---------------------------------------------------------------------------------------------
// Resume (Stage 5, D-057)
// ---------------------------------------------------------------------------------------------

/// The system message a resumed session starts with.
fn resumed_notice(events: &[Event]) -> String {
    events
        .iter()
        .find_map(|e| match e {
            Event::Message { message: Message { role: Role::System, content: Content::Text { text }, .. }, .. }
                if text.contains("resumed") =>
            {
                Some(text.clone())
            }
            _ => None,
        })
        .expect("a resumed notice")
}

/// A session with one full turn: an action, a reply and a `Done` summary.
async fn one_finished_session() -> Arc<MemoryStore> {
    let brain = ScriptedBrain::new(vec![
        step(vec![intent(Intent::Act { request: exec_request("act-1", "echo hi") })]),
        step(vec![
            intent(Intent::Say { text: "It said hi.".into() }),
            intent(Intent::Done { summary: "ran it".into() }),
        ]),
    ]);
    let store = Arc::new(MemoryStore::default());
    let rig =
        Rig::start_with_store(brain, ScriptedApprovals::allowing(), Arc::new(EchoSandbox), 8, store.clone()).await;
    rig.turn("hello").await;
    rig.tx.send(Command::Shutdown).await.unwrap();
    rig.task.await.unwrap();
    store
}

#[tokio::test]
async fn resume_replays_the_history_to_the_brain_without_summaries() {
    let store = one_finished_session().await;
    assert!(store.record("s1").messages.contains(&summary("ran it")), "the summary is stored");

    let brain = ScriptedBrain::new(vec![step(vec![intent(Intent::Say { text: "Back.".into() })]), step(vec![])]);
    let rig = Rig::resume(brain, ScriptedApprovals::allowing(), store.clone(), "s1").await;
    let next = rig.turn("again").await;
    assert_eq!(next[0], Event::TurnStarted { turn: TurnId::new("turn-1") }, "turn numbers carry on");

    let notice = resumed_notice(&rig.sink.all());
    assert!(notice.contains("workspace may have changed"), "{notice}");
    assert!(!notice.contains("account"), "same account, nothing to say: {notice}");
    assert_eq!(
        rig.brain.contexts()[0].messages,
        vec![
            text(Role::User, "hello"),
            proposal(exec_request("act-1", "echo hi")),
            observed(exec_observation("act-1", "echo hi")),
            text(Role::Agent, "It said hi."),
            text(Role::System, &notice),
            text(Role::User, "again"),
        ],
        "the stored conversation, minus the Done summary, then the notice"
    );

    // New messages go to the same session.
    let record = store.record("s1");
    assert_eq!(store.sessions.lock().unwrap().len(), 1, "no new session");
    assert_eq!(record.messages.last(), Some(&text(Role::Agent, "Back.")));
    assert!(record.messages.contains(&text(Role::User, "again")));
}

#[tokio::test]
async fn resume_redraws_the_conversation_and_past_actions() {
    let store = one_finished_session().await;
    let rig = Rig::resume(ScriptedBrain::new(vec![]), ScriptedApprovals::allowing(), store, "s1").await;
    rig.sink.wait(1, |e| matches!(e, Event::Message { message, .. } if message.role == Role::System)).await;
    let events: Vec<_> = rig
        .sink
        .all()
        .into_iter()
        .filter(|e| !matches!(e, Event::SandboxStatus { .. } | Event::SourceStatus { .. }))
        .collect();
    let notice = resumed_notice(&events);
    assert_eq!(
        events,
        vec![
            Event::SessionStarted {
                session: SessionId::new("s1"),
                account: AccountRef::new("scripted", "default"),
                brain: BrainInfo { provider: ProviderId::new("scripted"), model: "scripted-1".into() },
            },
            Event::Message { turn: t0(), message: text(Role::User, "hello") },
            Event::ActionProposed { turn: t0(), request: exec_request("act-1", "echo hi") },
            Event::ActionFinished { turn: t0(), observation: exec_observation("act-1", "echo hi") },
            Event::Message { turn: t0(), message: text(Role::Agent, "It said hi.") },
            Event::Message { turn: t0(), message: text(Role::System, &notice) },
        ],
        "no TurnStarted, no summary, every past action with its result"
    );
}

/// Nixxa quit (or crashed) while an action waited for approval: the proposal was stored without
/// a result. Resuming closes it before the brain sees the history again.
#[tokio::test]
async fn resume_closes_a_tool_call_left_without_a_result() {
    let brain = ScriptedBrain::new(vec![step(vec![
        intent(Intent::Say { text: "Running it.".into() }),
        intent(Intent::Act { request: exec_request("act-1", "rm -rf build") }),
    ])]);
    let store = Arc::new(MemoryStore::default());
    let rig = Rig::start_with_store(brain, ScriptedApprovals::hanging(), Arc::new(EchoSandbox), 8, store.clone()).await;
    rig.say("clean up").await;
    rig.sink.wait(1, |e| matches!(e, Event::ApprovalNeeded { .. })).await;
    rig.task.abort(); // the process died: nothing ran after this point
    let _ = rig.task.await;
    let stored = store.record("s1");
    assert_eq!(stored.messages.last(), Some(&proposal(exec_request("act-1", "rm -rf build"))), "dangling");

    let brain = ScriptedBrain::new(vec![step(vec![intent(Intent::Done { summary: "ok".into() })])]);
    let rig = Rig::resume(brain, ScriptedApprovals::allowing(), store.clone(), "s1").await;
    rig.turn("what happened?").await;

    let closed = Observation::failed(
        ActionId::new("act-1"),
        "not finished: Nixxa quit before this action finished, so it may not have run, or may have run only in part",
    );
    let seen = &rig.brain.contexts()[0].messages;
    let at = seen.iter().position(|m| m == &proposal(exec_request("act-1", "rm -rf build"))).expect("proposal");
    assert_eq!(seen[at + 1], observed(closed.clone()), "the proposal is answered before anything else");
    assert!(rig.sink.all().contains(&Event::ActionFinished { turn: t0(), observation: closed.clone() }));
    let record = store.record("s1");
    assert_eq!(record.actions.len(), 1, "the closing is recorded as an action record");
    assert_eq!(record.actions[0].observation, Some(closed));
    assert!(matches!(record.actions[0].verdict, Verdict::Deny(_)));
}

#[tokio::test]
async fn resume_with_another_account_says_so() {
    let store = Arc::new(MemoryStore::default());
    let workspace = WorkspaceInfo { name: "demo".into(), root: std::env::temp_dir() };
    let new = NewSession { workspace, account: AccountRef::new("anthropic", "work"), started_at: Timestamp(0) };
    store.create_session(new).await.unwrap();
    store.append_message(&SessionId::new("s1"), text(Role::User, "hi")).await.unwrap();

    let rig = Rig::resume(ScriptedBrain::new(vec![]), ScriptedApprovals::allowing(), store, "s1").await;
    rig.sink.wait(1, |e| matches!(e, Event::Message { message, .. } if message.role == Role::System)).await;
    let notice = resumed_notice(&rig.sink.all());
    assert!(notice.contains("anthropic:work") && notice.contains("scripted:default"), "{notice}");
    assert!(rig.sink.all().contains(&Event::SessionStarted {
        session: SessionId::new("s1"),
        account: AccountRef::new("scripted", "default"),
        brain: BrainInfo { provider: ProviderId::new("scripted"), model: "scripted-1".into() },
    }));
}

#[tokio::test]
async fn resuming_an_unknown_session_is_an_error() {
    let mut controller = Controller::new(
        ScriptedBrain::new(vec![]),
        Arc::new(EchoSandbox),
        Arc::new(MemoryStore::default()),
        Arc::new(FakeKnowledge),
        ScriptedApprovals::allowing(),
        Arc::new(RecordingSink::default()),
        ControllerConfig::new(
            WorkspaceInfo { name: "demo".into(), root: std::env::temp_dir() },
            AccountRef::new("scripted", "default"),
            "",
        ),
    );
    let err = controller.resume_session(&SessionId::new("nope")).await.unwrap_err();
    assert!(err.to_string().contains("not found"), "{err}");
}

#[tokio::test]
async fn the_agent_is_told_the_network_policy_and_sees_what_was_blocked() {
    let brain = ScriptedBrain::new(vec![
        step(vec![intent(Intent::Act { request: exec_request("act-1", "curl https://example.com") })]),
        step(vec![intent(Intent::Done { summary: "blocked".into() })]),
    ]);
    let rig = Rig::start(brain, ScriptedApprovals::allowing(), Arc::new(AllowlistSandbox), DEFAULT_MAX_STEPS).await;
    rig.turn("fetch it").await;
    let contexts = rig.brain.contexts();
    assert!(contexts[0].instructions.starts_with("Reply briefly.\n\nNetwork: only these hosts are reachable"));
    assert!(contexts[0].instructions.contains(": crates.io."), "{}", contexts[0].instructions);
    let observed = contexts[1].messages.iter().find_map(|m| match &m.content {
        Content::Observation { observation } => Some(observation.clone()),
        _ => None,
    });
    assert_eq!(observed.expect("the observation reaches the brain").network, vec![blocked_request()]);
}

#[tokio::test]
async fn a_note_is_recorded_between_turns_never_inside_one() {
    let brain = ScriptedBrain::new(vec![
        step(vec![intent(Intent::Act { request: exec_request("act-1", "ls") })]),
        step(vec![intent(Intent::Done { summary: "done".into() })]),
        step(vec![intent(Intent::Done { summary: "again".into() })]),
    ]);
    let approvals = ScriptedApprovals::holding();
    let rig = Rig::start(brain, approvals.clone(), Arc::new(EchoSandbox), DEFAULT_MAX_STEPS).await;
    rig.say("first").await;
    approvals.wait_asked(1).await;
    // Mid-turn, waiting for approval: the note waits for the turn to end.
    rig.tx.send(Command::Note("the user allowed crates.io".into())).await.unwrap();
    approvals.release(Verdict::Allow);
    rig.turn("second").await;
    let roles: Vec<(Role, String)> = rig
        .brain
        .contexts()
        .last()
        .unwrap()
        .messages
        .iter()
        .map(|m| {
            (
                m.role,
                match &m.content {
                    Content::Text { text } => text.clone(),
                    _ => "…".into(),
                },
            )
        })
        .collect();
    let note = roles.iter().position(|(role, text)| *role == Role::System && text == "the user allowed crates.io");
    let observation = roles.iter().position(|(role, _)| *role == Role::Observation);
    let second = roles.iter().position(|(role, text)| *role == Role::User && text == "second");
    assert!(observation < note && note < second, "the note sits after the first turn: {roles:?}");
}
