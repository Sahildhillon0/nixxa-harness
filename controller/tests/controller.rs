//! Tests for the agent loop in `nixxa-controller`.
//!
//! The controller may depend only on `nixxa-contracts`, so the fakes it is driven with live here.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use nixxa_contracts::{
    AccountRef, Action, ActionId, ActionRecord, ActionRequest, ApprovalPort, Brain, BrainError, BrainEvent, BrainInfo,
    BrainStream, Content, Context, Event, EventSink, Intent, Knowledge, KnowledgeAction, KnowledgeEntry,
    KnowledgeError, Message, NewSession, Observation, Outcome, Primer, PrimerSection, ProviderId, Role, Sandbox,
    SandboxAction, SandboxActionKind, SandboxCapabilities, SandboxError, SearchHit, SessionId, SessionInfo,
    SessionRecord, SourceId, SourceInfo, SourceStatus, Store, StoreError, Timestamp, ToolSpec, TurnId, Usage, Verdict,
    WorkspaceInfo,
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
            network: false,
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
}

impl MemoryStore {
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
}

#[async_trait]
impl ApprovalPort for ScriptedApprovals {
    async fn ask(&self, _request: &ActionRequest, reason: &str) -> Verdict {
        self.asked.lock().unwrap().push(reason.to_string());
        if self.hang {
            Notify::new().notified().await;
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
        let store = Arc::new(MemoryStore::default());
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
        let info = controller.start_session().await.expect("start session");
        assert_eq!(info.id.as_str(), "s1");
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
            action: SandboxAction::Exec { command: command.into(), cwd: None, timeout_ms: None },
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
            Event::Message { turn: t0(), message: text(Role::Agent, "ran the command") },
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
    assert_eq!(events[events.len() - 2], Event::Message { turn: t0(), message: text(Role::Agent, "gave up") });
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
        step(vec![intent(Intent::Done { summary: "hi there".into() })]),
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
