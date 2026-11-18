//! OpenRouter, for real, with the account signed in through `nixxa` (setup option 3). By hand
//! only; reads the OS keychain and spends a little of the account's credit (or none, with a free
//! model):
//!
//! ```text
//! cargo test -p nixxa-harness --test live_openrouter -- --ignored live_openrouter_tool_loop
//! ```

use std::sync::Arc;

use futures::StreamExt;
use nixxa_accounts::{KeychainConfig, KeychainCredentials};
use nixxa_brain::{OpenAiCompatBrain, OpenAiCompatConfig};
use nixxa_contracts::conformance::brain::run;
use nixxa_contracts::{
    AccountRef, Brain, BrainEvent, Content, Context, Intent, Limits, Message, Observation, Outcome, Primer, Role,
    SandboxActionKind, SessionId, Timestamp,
};

#[tokio::test(flavor = "multi_thread")]
#[ignore = "calls OpenRouter with the account stored in the OS keychain"]
async fn live_openrouter_tool_loop() {
    let account = AccountRef::new("openrouter", "default");
    let credentials = Arc::new(KeychainCredentials::new(KeychainConfig::default()));
    let model = credentials
        .setting("model.openrouter:default")
        .await
        .expect("the keychain")
        .expect("sign in with OpenRouter in nixxa first (setup option 3)");
    let brain = OpenAiCompatBrain::new(credentials, OpenAiCompatConfig::openrouter(model)).expect("client");
    brain.verify(&account).await.expect("the key works and the model is offered");
    run(&brain, account.clone()).await;

    let at = Timestamp(0);
    let mut context = Context {
        session: SessionId::new("live"),
        account,
        model: None,
        instructions: "You are Nixxa.".into(),
        primer: Primer::default(),
        messages: vec![Message::text(
            Role::User,
            "Use the exec tool to run `echo nixxa-live-check`, then reply with only its output.",
            at,
        )],
        sandbox_actions: vec![SandboxActionKind::Exec],
        knowledge_tools: Vec::new(),
        limits: Limits::default(),
    };
    let mut proposals = Vec::new();
    let mut stream = brain.think(context.clone()).await.expect("request 1");
    while let Some(item) = stream.next().await {
        match item.expect("request 1 stream") {
            BrainEvent::Intent { intent: Intent::Say { text } } => {
                context.messages.push(Message::text(Role::Agent, text, at))
            }
            BrainEvent::Intent { intent: Intent::Act { request } } => {
                context.messages.push(Message {
                    role: Role::Agent,
                    content: Content::Action { request: request.clone() },
                    at,
                });
                proposals.push(request);
            }
            _ => {}
        }
    }
    assert!(!proposals.is_empty(), "the model called the tool");
    for request in proposals {
        let observation = Observation::new(
            request.id,
            Outcome::Exec { exit_code: Some(0), stdout: "nixxa-live-check\n".into(), stderr: String::new() },
        );
        context.messages.push(Message { role: Role::Observation, content: Content::Observation { observation }, at });
    }
    let items: Vec<_> = brain.think(context).await.expect("request 2").collect().await;
    assert!(items.iter().all(Result::is_ok), "request 2 failed: {items:?}");
}
