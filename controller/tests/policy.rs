//! The project policy (#5) at the controller's gate: refused before any approval, in Auto and
//! in wake-up turns alike, with the rule named; asked even in Auto where it says so; its prose
//! in the agent's instructions, labelled and scrubbed; a change in force only once the user
//! accepts it.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::stream::BoxStream;
use nixxa_contracts::{
    AccountRef, Action, ActionId, ActionRecord, ActionRequest, ApprovalPort, Asking, Brain, BrainError, BrainEvent,
    BrainInfo, BrainStream, Budgets, Context, Edit, Event, EventSink, Intent, JobId, Knowledge, KnowledgeEntry,
    KnowledgeError, Limit, Message, NetworkAccess, NewSession, Observation, Outcome, PolicyProblem, Primer,
    ProjectPolicy, ProjectProse, ProviderId, RedactionSource, Rule, RuleOrigin, RuleSource, Sandbox, SandboxAction,
    SandboxActionKind, SandboxCapabilities, SandboxError, SandboxEvent, SearchHit, SessionId, SessionInfo,
    SessionRecord, SourceId, SourceInfo, Store, StoreError, Timestamp, ToolSpec, Verdict, WorkspaceInfo,
};
use nixxa_controller::scrub::Scrubber;
use nixxa_controller::{Command, Controller, ControllerConfig};
use tokio::sync::mpsc;
use tokio::time::{Duration, sleep};

// ---------------------------------------------------------------------------------------------
// Fakes
// ---------------------------------------------------------------------------------------------

/// Replays one list of intents per step (then `Done`), and keeps every context.
#[derive(Default)]
struct ScriptedBrain {
    steps: Mutex<VecDeque<Vec<Intent>>>,
    contexts: Mutex<Vec<Context>>,
}

#[async_trait]
impl Brain for ScriptedBrain {
    fn describe(&self) -> BrainInfo {
        BrainInfo { provider: ProviderId::new("scripted"), model: "scripted-1".into() }
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

/// Runs nothing; keeps every action it was given. A background exec ends at once.
struct RecordingSandbox {
    ran: Mutex<Vec<SandboxAction>>,
    tx: mpsc::UnboundedSender<SandboxEvent>,
    rx: Mutex<Option<mpsc::UnboundedReceiver<SandboxEvent>>>,
}

impl RecordingSandbox {
    fn new() -> Arc<Self> {
        let (tx, rx) = mpsc::unbounded_channel();
        Arc::new(Self { ran: Mutex::default(), tx, rx: Mutex::new(Some(rx)) })
    }
}

#[async_trait]
impl Sandbox for RecordingSandbox {
    fn capabilities(&self) -> SandboxCapabilities {
        SandboxCapabilities {
            backend: "recording".into(),
            isolated: true,
            network: NetworkAccess::default(),
            jobs: true,
            terminal: false,
            checkpoints: false,
            actions: SandboxActionKind::ALL.to_vec(),
        }
    }

    async fn execute(&self, id: ActionId, action: SandboxAction) -> Result<Observation, SandboxError> {
        self.ran.lock().unwrap().push(action.clone());
        let outcome = match action {
            SandboxAction::Exec { background: true, .. } => {
                let job = JobId::new("j1");
                let _ = self.tx.send(SandboxEvent::JobFinished {
                    job: job.clone(),
                    exit_code: Some(0),
                    duration_ms: 1,
                    truncated: false,
                    status: "job j1 exited with code 0".into(),
                });
                Outcome::Started { job }
            }
            SandboxAction::WriteFile { content, .. } => Outcome::Written { bytes: content.len() as u64 },
            _ => Outcome::Exec { exit_code: Some(0), stdout: String::new(), stderr: String::new() },
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
        match self.rx.lock().unwrap().take() {
            Some(rx) => Box::pin(futures::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|e| (e, rx)) })),
            None => Box::pin(futures::stream::empty()),
        }
    }
}

#[derive(Default)]
struct MemoryStore {
    sessions: Mutex<Vec<SessionRecord>>,
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
        let sessions = self.sessions.lock().unwrap();
        sessions.iter().find(|s| &s.info.id == id).cloned().ok_or_else(|| StoreError::NotFound(id.clone()))
    }

    async fn append_message(&self, id: &SessionId, message: Message) -> Result<(), StoreError> {
        let mut sessions = self.sessions.lock().unwrap();
        if let Some(session) = sessions.iter_mut().find(|s| &s.info.id == id) {
            session.messages.push(message);
        }
        Ok(())
    }

    async fn append_action(&self, id: &SessionId, record: ActionRecord) -> Result<(), StoreError> {
        let mut sessions = self.sessions.lock().unwrap();
        if let Some(session) = sessions.iter_mut().find(|s| &s.info.id == id) {
            session.actions.push(record);
        }
        Ok(())
    }

    async fn list_sessions(&self) -> Result<Vec<SessionInfo>, StoreError> {
        Ok(self.sessions.lock().unwrap().iter().map(|s| s.info.clone()).collect())
    }

    async fn search(&self, _query: &str) -> Result<Vec<SearchHit>, StoreError> {
        Ok(Vec::new())
    }
}

struct NoKnowledge;

#[async_trait]
impl Knowledge for NoKnowledge {
    async fn sources(&self) -> Vec<SourceInfo> {
        Vec::new()
    }

    async fn tools(&self) -> Vec<ToolSpec> {
        Vec::new()
    }

    async fn primer(&self, _workspace: &WorkspaceInfo) -> Result<Primer, KnowledgeError> {
        Ok(Primer::default())
    }

    async fn call(
        &self,
        id: ActionId,
        _source: &SourceId,
        _tool: &str,
        _args: serde_json::Value,
    ) -> Result<Observation, KnowledgeError> {
        Ok(Observation::new(id, Outcome::Text { content: String::new() }))
    }

    async fn record(&self, id: ActionId, _: &SourceId, _: KnowledgeEntry) -> Result<Observation, KnowledgeError> {
        Ok(Observation::new(id, Outcome::Recorded { location: None }))
    }
}

/// Auto mode: approves at once unless told to ask, and keeps how each prompt was asked.
#[derive(Default)]
struct AutoApprovals {
    asked: Mutex<Vec<(String, Asking)>>,
}

#[async_trait]
impl ApprovalPort for AutoApprovals {
    async fn ask(&self, _request: &ActionRequest, reason: &str, asking: Asking) -> Verdict {
        self.asked.lock().unwrap().push((reason.to_string(), asking));
        match asking {
            Asking::ByMode => Verdict::Allow,
            Asking::Always => Verdict::Deny("the user said no".into()),
        }
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

    async fn wait_turns(&self, n: usize) {
        for _ in 0..400 {
            if self.all().iter().filter(|e| matches!(e, Event::TurnDone { .. })).count() >= n {
                return;
            }
            sleep(Duration::from_millis(5)).await;
        }
        panic!("timed out: {:#?}", self.all());
    }

    /// Every action's observation, in order.
    fn finished(&self) -> Vec<Observation> {
        self.all()
            .into_iter()
            .filter_map(|e| match e {
                Event::ActionFinished { observation, .. } => Some(observation),
                _ => None,
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------------------------
// Rig
// ---------------------------------------------------------------------------------------------

struct Rig {
    brain: Arc<ScriptedBrain>,
    sandbox: Arc<RecordingSandbox>,
    approvals: Arc<AutoApprovals>,
    sink: Arc<Sink>,
    tx: mpsc::Sender<Command>,
}

impl Rig {
    async fn start(policy: ProjectPolicy, steps: Vec<Vec<Intent>>) -> Self {
        let brain = Arc::new(ScriptedBrain { steps: Mutex::new(steps.into()), contexts: Mutex::default() });
        let sandbox = RecordingSandbox::new();
        let approvals = Arc::new(AutoApprovals::default());
        let sink = Arc::new(Sink::default());
        let mut config = ControllerConfig::new(
            WorkspaceInfo { name: "demo".into(), root: "/home/u/proj".into() },
            AccountRef::new("scripted", "default"),
            "Be brief.",
            Arc::new(Scrubber::with_key([3; 32])),
        );
        config.clock = || Timestamp(0);
        config.wake_delay = Duration::from_millis(10);
        config.policy = policy;
        let mut controller = Controller::new(
            brain.clone(),
            sandbox.clone(),
            Arc::new(MemoryStore::default()),
            Arc::new(NoKnowledge),
            approvals.clone(),
            sink.clone(),
            config,
        );
        controller.start_session().await.unwrap();
        let (tx, rx) = mpsc::channel(8);
        tokio::spawn(async move { controller.run(rx).await });
        Self { brain, sandbox, approvals, sink, tx }
    }

    async fn turn(&self, text: &str) {
        let done = self.sink.all().iter().filter(|e| matches!(e, Event::TurnDone { .. })).count();
        self.tx.send(Command::UserMessage(text.into())).await.unwrap();
        self.sink.wait_turns(done + 1).await;
    }

    fn ran(&self) -> Vec<SandboxAction> {
        self.sandbox.ran.lock().unwrap().clone()
    }

    fn last_instructions(&self) -> String {
        self.brain.contexts.lock().unwrap().last().unwrap().instructions.clone()
    }
}

fn rule(pattern: &str, line: u32) -> Rule {
    Rule { pattern: pattern.into(), source: RuleSource { file: "AGENTS.md".into(), line, origin: RuleOrigin::Project } }
}

fn protecting_migrations() -> ProjectPolicy {
    ProjectPolicy { protect: vec![rule("migrations/**", 12)], ..ProjectPolicy::default() }
}

fn act(n: u32, action: SandboxAction) -> Intent {
    Intent::Act {
        request: ActionRequest { id: ActionId::new(format!("a{n}")), action: Action::Sandbox { action }, reason: None },
    }
}

fn patch(path: &str) -> SandboxAction {
    SandboxAction::Patch {
        path: path.into(),
        edits: vec![Edit { search: "int".into(), replace: "bigint".into() }],
        expected_hash: "0".repeat(64),
    }
}

fn write(path: &str) -> SandboxAction {
    SandboxAction::WriteFile { path: path.into(), content: "x".into() }
}

fn exec(command: &str, background: bool) -> SandboxAction {
    SandboxAction::Exec { command: command.into(), cwd: None, timeout_ms: None, background }
}

/// The observation was a refusal naming the rule and its source.
fn refused_by_rule(observation: &Observation) {
    assert!(
        matches!(&observation.outcome, Outcome::Denied { reason } if reason.contains("`migrations/**` (AGENTS.md:12)")),
        "{observation:?}"
    );
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_protected_path_is_refused_before_any_approval_even_in_auto() {
    let rig = Rig::start(
        protecting_migrations(),
        vec![vec![
            act(1, patch("migrations/001.sql")),
            act(2, write("/workspace/migrations/002.sql")),
            act(3, write("./src/../migrations//003.sql")),
            act(4, write("src/main.rs")),
        ]],
    )
    .await;
    rig.turn("change the schema").await;
    let finished = rig.sink.finished();
    finished[..3].iter().for_each(refused_by_rule);
    assert!(matches!(finished[3].outcome, Outcome::Written { .. }), "{:?}", finished[3]);
    assert_eq!(rig.ran(), [write("src/main.rs")], "nothing protected reached the sandbox");
    assert_eq!(rig.approvals.asked.lock().unwrap().len(), 1, "only the allowed write was put to the (Auto) approver");
    // The agent was told why.
    let context = serde_json::to_string(rig.brain.contexts.lock().unwrap().last().unwrap()).unwrap();
    assert!(context.contains("refused by the project policy"), "{context}");
}

#[tokio::test]
async fn a_wake_up_turn_goes_through_the_same_gate() {
    let rig = Rig::start(
        protecting_migrations(),
        vec![
            vec![act(1, exec("make", true))],
            vec![Intent::Done { summary: "waiting".into() }],
            // The wake-up turn.
            vec![act(2, write("migrations/9.sql"))],
        ],
    )
    .await;
    rig.turn("build").await;
    rig.sink.wait_turns(2).await;
    assert!(rig.sink.all().iter().any(|e| matches!(e, Event::WakeUp { .. })));
    refused_by_rule(rig.sink.finished().last().unwrap());
    assert!(!rig.ran().contains(&write("migrations/9.sql")));
}

#[tokio::test]
async fn commands_are_refused_or_asked_even_in_auto() {
    let policy = ProjectPolicy {
        deny_commands: vec![rule("git push", 20)],
        ask_commands: vec![rule("npm publish", 21)],
        ask_read: vec![rule("data/**", 22)],
        ..ProjectPolicy::default()
    };
    let rig = Rig::start(
        policy,
        vec![vec![
            act(1, exec("cargo test && git push --force", false)),
            act(2, exec("npm publish", false)),
            act(3, SandboxAction::ReadFile { path: "data/users.csv".into() }),
            act(4, exec("cargo test", false)),
        ]],
    )
    .await;
    rig.turn("ship it").await;
    let finished = rig.sink.finished();
    assert!(matches!(&finished[0].outcome, Outcome::Denied { reason } if reason.contains("`git push` (AGENTS.md:20)")));
    let asked = rig.approvals.asked.lock().unwrap().clone();
    assert_eq!(asked.iter().map(|(_, a)| *a).collect::<Vec<_>>(), [Asking::Always, Asking::Always, Asking::ByMode]);
    assert!(asked[0].0.contains("`npm publish` (AGENTS.md:21)"), "{asked:?}");
    assert_eq!(rig.ran(), [exec("cargo test", false)]);
}

#[tokio::test]
async fn the_prose_reaches_the_agent_labelled_and_scrubbed() {
    let key = "AKIAIOSFODNN7EXAMPLE";
    let policy = ProjectPolicy {
        protect: vec![rule("migrations/**", 12)],
        prose: vec![ProjectProse {
            file: "AGENTS.md".into(),
            dir: String::new(),
            text: format!("Run `cargo test` before you finish. The staging key is {key}."),
        }],
        ..ProjectPolicy::default()
    };
    let rig = Rig::start(policy, Vec::new()).await;
    rig.turn("hello").await;
    let instructions = rig.last_instructions();
    assert!(instructions.contains("Run `cargo test` before you finish."), "{instructions}");
    assert!(instructions.contains("not the user's instructions"), "{instructions}");
    assert!(instructions.contains("Protected: never changed"), "{instructions}");
    assert!(!instructions.contains(key), "the prose is scrubbed: {instructions}");
    assert!(rig.sink.all().iter().any(|e| matches!(
        e,
        Event::SecretRedacted { source: RedactionSource::ProjectRules { file }, .. } if file == "AGENTS.md"
    )));
}

#[tokio::test]
async fn an_accepted_change_is_in_force_from_then_on_and_the_agent_is_told() {
    let rig = Rig::start(
        ProjectPolicy::default(),
        vec![
            vec![act(1, write("migrations/1.sql"))],
            vec![Intent::Done { summary: "ok".into() }],
            vec![act(2, write("migrations/2.sql"))],
        ],
    )
    .await;
    rig.turn("first").await;
    assert!(matches!(rig.sink.finished()[0].outcome, Outcome::Written { .. }), "no policy yet");
    rig.tx.send(Command::Policy(Box::new(protecting_migrations()))).await.unwrap();
    rig.turn("second").await;
    refused_by_rule(rig.sink.finished().last().unwrap());
    let context = serde_json::to_string(rig.brain.contexts.lock().unwrap().last().unwrap()).unwrap();
    assert!(context.contains("protected, added: `migrations/**`"), "{context}");
}

#[tokio::test]
async fn the_policys_step_budget_caps_a_turn_when_it_is_lower() {
    let policy = ProjectPolicy {
        budgets: Budgets {
            steps_per_turn: Some(Limit { value: 2, line: 5, origin: RuleOrigin::Project }),
            ..Budgets::default()
        },
        ..ProjectPolicy::default()
    };
    let steps = (0..5).map(|n| vec![act(n, exec("true", false))]).collect();
    let rig = Rig::start(policy, steps).await;
    rig.turn("loop").await;
    assert_eq!(rig.ran().len(), 2);
    assert!(rig.sink.all().iter().any(|e| matches!(
        e,
        Event::Message { message, .. } if serde_json::to_string(message).unwrap().contains("step budget of 2")
    )));
}

#[tokio::test]
async fn a_policy_that_cant_be_read_makes_every_action_ask_even_in_auto() {
    let policy = ProjectPolicy {
        problems: vec![PolicyProblem {
            source: RuleSource { file: "AGENTS.md".into(), line: 4, origin: RuleOrigin::Project },
            message: "unknown key `mode`".into(),
        }],
        ..ProjectPolicy::default()
    };
    let rig = Rig::start(policy, vec![vec![act(1, exec("ls", false))]]).await;
    rig.turn("look").await;
    let asked = rig.approvals.asked.lock().unwrap().clone();
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].1, Asking::Always, "{asked:?}");
    assert!(rig.ran().is_empty(), "the (fake) user said no");
}
