//! Conformance suite for [`Brain`].

use std::collections::HashSet;

use futures::StreamExt;

use super::T0;
use crate::{AccountRef, Brain, BrainEvent, Context, Intent, Limits, Message, Primer, Role, SessionId};

/// Runs one think step on `brain` with `account` and checks the stream contract.
/// Real providers cost money: run this against them only in opt-in tests.
pub async fn run(brain: &dyn Brain, account: AccountRef) {
    let info = brain.describe();
    assert!(!info.provider.as_str().is_empty() && !info.model.is_empty(), "describe() must name provider and model");

    let context = Context {
        session: SessionId::new("conformance"),
        account,
        model: None,
        instructions: "Reply with one short sentence.".into(),
        primer: Primer::default(),
        messages: vec![Message::text(Role::User, "Say hello.", T0)],
        sandbox_actions: vec![],
        knowledge_tools: vec![],
        limits: Limits { max_output_tokens: Some(64) },
    };
    let mut stream = brain.think(context).await.expect("think");
    let mut intents = 0;
    let mut action_ids = HashSet::new();
    while let Some(event) = stream.next().await {
        if let BrainEvent::Intent { intent } = event.expect("stream item") {
            intents += 1;
            if let Intent::Act { request } = intent {
                assert!(!request.id.as_str().is_empty(), "Act intents need an action id");
                assert!(action_ids.insert(request.id.clone()), "action ids must be unique per stream");
            }
        }
    }
    assert!(intents > 0, "a think stream must yield at least one intent");
}
