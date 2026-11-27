//! The real controller driving the real `AnthropicBrain`, answering from recorded responses (no
//! network), with every other module a Stage 2 fake.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use nixxa_accounts::{KeychainConfig, KeychainCredentials};
use nixxa_brain::{AnthropicBrain, AnthropicConfig, Replay};
use nixxa_contracts::{
    AccountRef, ActionRequest, ApprovalPort, Credential, Credentials, Event, EventSink, Secret, Verdict, WorkspaceInfo,
};
use nixxa_controller::{Command, Controller, ControllerConfig};
use nixxa_db::memory::MemoryStore;
use nixxa_knowledge::fake::FakeKnowledge;
use nixxa_sandbox::fake::FakeSandbox;
use serde_json::json;
use tokio::sync::mpsc;
use tokio::time::{Duration, sleep};

struct AllowAll;

#[async_trait]
impl ApprovalPort for AllowAll {
    async fn ask(&self, _request: &ActionRequest, _reason: &str, _asking: nixxa_contracts::Asking) -> Verdict {
        Verdict::Allow
    }
}

#[derive(Default)]
struct Sink(Mutex<Vec<Event>>);

impl EventSink for Sink {
    fn emit(&self, event: Event) {
        self.0.lock().unwrap().push(event);
    }
}

/// Regression (PR #6 review, B1): a parallel tool call goes back to the API as the model's one
/// assistant message, followed by one user message holding every result.
#[tokio::test]
async fn parallel_calls_are_replayed_as_one_assistant_turn() {
    let account = AccountRef::new("anthropic", "default");
    let credentials = Arc::new(KeychainCredentials::in_memory(KeychainConfig::default()));
    credentials.add(account.clone(), Credential::ApiKey(Secret::new("sk-ant-test-key-0000"))).await.unwrap();

    let replay = Replay::new();
    replay.push_fixture("anthropic/fixtures/parallel_tool_use.sse").push_fixture("anthropic/fixtures/text.sse");
    let brain = AnthropicBrain::replaying(credentials, AnthropicConfig::default(), &replay);

    let sink = Arc::new(Sink::default());
    let workspace = WorkspaceInfo { name: "e2e".into(), root: std::env::temp_dir().join("nixxa-e2e") };
    let mut controller = Controller::new(
        Arc::new(brain),
        Arc::new(FakeSandbox::default()),
        Arc::new(MemoryStore::default()),
        Arc::new(FakeKnowledge),
        Arc::new(AllowAll),
        sink.clone(),
        ControllerConfig::new(
            workspace,
            account,
            "You are Nixxa.",
            Arc::new(nixxa_controller::scrub::Scrubber::new().unwrap()),
        ),
    );
    controller.start_session().await.unwrap();
    let (tx, rx) = mpsc::channel(4);
    let task = tokio::spawn(async move { controller.run(rx).await });
    tx.send(Command::UserMessage("go".into())).await.unwrap();
    for _ in 0..400 {
        if sink.0.lock().unwrap().iter().any(|e| matches!(e, Event::TurnDone { .. })) {
            break;
        }
        sleep(Duration::from_millis(5)).await;
    }
    tx.send(Command::Shutdown).await.unwrap();
    task.await.unwrap();

    let bodies = replay.request_bodies();
    assert_eq!(bodies.len(), 2, "one request per step");
    let messages = bodies[1]["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 3, "user, assistant, user: {messages:#?}");
    assert_eq!(messages[0], json!({ "role": "user", "content": [{ "type": "text", "text": "go" }] }));
    assert_eq!(
        messages[1],
        json!({ "role": "assistant", "content": [
            { "type": "thinking", "thinking": "", "signature": "EqQBsig-parallel" },
            { "type": "tool_use", "id": "toolu_01A", "name": "exec", "input": { "command": "cargo test" } },
            { "type": "tool_use", "id": "toolu_01B", "name": "notes__search", "input": { "query": "build notes" } }
        ]})
    );
    let results: Vec<_> = messages[2]["content"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| (b["type"].clone(), b["tool_use_id"].clone()))
        .collect();
    assert_eq!(
        results,
        vec![(json!("tool_result"), json!("toolu_01A")), (json!("tool_result"), json!("toolu_01B"))],
        "both results in one user message"
    );
}
