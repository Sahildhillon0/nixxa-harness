//! The real controller driving the real `OpenAiCompatBrain` (a local model), answering from
//! recorded responses (no network), with every other module a Stage 2 fake.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use nixxa_accounts::{KeychainConfig, KeychainCredentials};
use nixxa_brain::{OpenAiCompatBrain, OpenAiCompatConfig, Replay};
use nixxa_contracts::{
    AccountRef, ActionRequest, ApprovalPort, Credential, Credentials, Event, EventSink, Verdict, WorkspaceInfo,
};
use nixxa_controller::{Command, Controller, ControllerConfig};
use nixxa_db::memory::MemoryStore;
use nixxa_knowledge::fake::FakeKnowledge;
use nixxa_sandbox::fake::FakeSandbox;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::time::{Duration, sleep};

struct AllowAll;

#[async_trait]
impl ApprovalPort for AllowAll {
    async fn ask(&self, _request: &ActionRequest, _reason: &str) -> Verdict {
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

/// Review (PR #8, S1): a parallel step from a local model goes back as one assistant message with
/// both calls, followed by one `tool` message per result, in the controller's real order.
#[tokio::test]
async fn parallel_calls_from_a_local_model_are_replayed_in_chat_format() {
    let account = AccountRef::new("local", "default");
    let credentials = Arc::new(KeychainCredentials::in_memory(KeychainConfig::default()));
    let credential = Credential::Local { base_url: "http://localhost:1234/v1".into() };
    credentials.add(account.clone(), credential).await.unwrap();

    let replay = Replay::new();
    replay
        .push_fixture("openai_compat/fixtures/parallel_tool_calls.sse")
        .push_fixture("openai_compat/fixtures/text.sse");
    let brain = OpenAiCompatBrain::replaying(credentials, OpenAiCompatConfig::new("local", "qwen3-8b"), &replay);

    let sink = Arc::new(Sink::default());
    let workspace = WorkspaceInfo { name: "e2e".into(), root: std::env::temp_dir().join("nixxa-local-e2e") };
    let mut controller = Controller::new(
        Arc::new(brain),
        Arc::new(FakeSandbox::default()),
        Arc::new(MemoryStore::default()),
        Arc::new(FakeKnowledge),
        Arc::new(AllowAll),
        sink.clone(),
        ControllerConfig::new(workspace, account, "You are Nixxa."),
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
    let roles: Vec<&str> = messages.iter().map(|m| m["role"].as_str().unwrap()).collect();
    assert_eq!(roles, vec!["system", "user", "assistant", "tool", "tool"], "{messages:#?}");
    assert_eq!(messages[1], json!({ "role": "user", "content": "go" }));
    assert_eq!(messages[2]["content"], Value::Null);
    let calls: Vec<(&str, &str)> = messages[2]["tool_calls"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| (c["id"].as_str().unwrap(), c["function"]["name"].as_str().unwrap()))
        .collect();
    assert_eq!(calls, vec![("call_1", "exec"), ("call_2", "notes__search")]);
    assert_eq!(messages[3]["tool_call_id"], "call_1");
    assert_eq!(messages[4]["tool_call_id"], "call_2");
    assert!(messages[3]["content"].as_str().unwrap().contains("cargo test"), "the fake sandbox echoed the command");
}
