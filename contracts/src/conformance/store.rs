//! Conformance suite for [`Store`].

use super::{T0, account, id, workspace};
use crate::{
    Action, ActionRecord, ActionRequest, Content, Message, NewSession, Observation, Outcome, Role, SandboxAction,
    SessionId, Store, StoreError, Timestamp, Verdict,
};

fn new_session(at: Timestamp) -> NewSession {
    NewSession { workspace: workspace(), account: account(), started_at: at }
}

/// Runs every scenario, each on a fresh store from `fresh`.
pub async fn run<S: Store, F: Fn() -> S>(fresh: F) {
    create_then_load(&fresh()).await;
    appends_keep_order(&fresh()).await;
    unknown_session_is_not_found(&fresh()).await;
    list_is_newest_first(&fresh()).await;
    search_finds_text(&fresh()).await;
    summaries_keep_their_marker(&fresh()).await;
    first_message_is_the_first_user_text(&fresh()).await;
}

async fn create_then_load(store: &dyn Store) {
    let info = store.create_session(new_session(T0)).await.expect("create_session");
    assert_eq!(info.workspace, workspace());
    assert_eq!(info.account, account());
    assert_eq!(info.started_at, T0);
    assert_eq!(info.first_message, None, "no user message yet");
    let record = store.load_session(&info.id).await.expect("load_session");
    assert_eq!(record.info, info);
    assert!(record.messages.is_empty() && record.actions.is_empty());
    let listed = store.list_sessions().await.expect("list_sessions");
    assert!(listed.iter().any(|s| s.id == info.id), "new session missing from list");
}

async fn appends_keep_order(store: &dyn Store) {
    let s = store.create_session(new_session(T0)).await.unwrap().id;
    let first = Message::text(Role::User, "first", Timestamp(1));
    let request = ActionRequest {
        id: id(1),
        action: Action::Sandbox { action: SandboxAction::ListDir { path: ".".into() } },
        reason: None,
    };
    let proposal =
        Message { role: Role::Agent, content: Content::Action { request: request.clone() }, at: Timestamp(2) };
    let observation = Observation::new(id(1), Outcome::Listing { entries: vec![] });
    store.append_message(&s, first.clone()).await.unwrap();
    store.append_message(&s, proposal.clone()).await.unwrap();
    let record = ActionRecord {
        request,
        verdict: Verdict::Allow,
        observation: Some(observation),
        checkpoint: Some(crate::CheckpointId::new("c1")),
        at: Timestamp(3),
    };
    store.append_action(&s, record.clone()).await.unwrap();
    let loaded = store.load_session(&s).await.unwrap();
    assert_eq!(loaded.messages, vec![first, proposal], "messages must round-trip in order");
    assert_eq!(loaded.actions, vec![record], "actions must round-trip");
}

async fn unknown_session_is_not_found(store: &dyn Store) {
    let ghost = SessionId::new("conformance-does-not-exist");
    assert!(matches!(store.load_session(&ghost).await, Err(StoreError::NotFound(_))));
    let msg = Message::text(Role::User, "x", T0);
    assert!(matches!(store.append_message(&ghost, msg).await, Err(StoreError::NotFound(_))));
}

async fn list_is_newest_first(store: &dyn Store) {
    let older = store.create_session(new_session(Timestamp(10))).await.unwrap().id;
    let newer = store.create_session(new_session(Timestamp(20))).await.unwrap().id;
    let ids: Vec<_> = store.list_sessions().await.unwrap().into_iter().map(|s| s.id).collect();
    let pos = |x: &SessionId| ids.iter().position(|i| i == x).expect("session listed");
    assert!(pos(&newer) < pos(&older), "list_sessions must be newest first");
}

async fn search_finds_text(store: &dyn Store) {
    let s = store.create_session(new_session(T0)).await.unwrap().id;
    store.append_message(&s, Message::text(Role::User, "find the Needle here", T0)).await.unwrap();
    let hits = store.search("needle").await.unwrap();
    assert!(hits.iter().any(|h| h.session == s), "case-insensitive search must find the message");
    assert!(store.search("zz-absent-term-zz").await.unwrap().is_empty());
}

/// D-029: a `Done` summary must come back as a summary, never as agent text, so a resumed
/// session can keep it away from the brain.
async fn summaries_keep_their_marker(store: &dyn Store) {
    let s = store.create_session(new_session(T0)).await.unwrap().id;
    let said = Message::text(Role::Agent, "all done", Timestamp(1));
    let summary =
        Message { role: Role::Agent, content: Content::Summary { text: "ran the tests".into() }, at: Timestamp(2) };
    store.append_message(&s, said.clone()).await.unwrap();
    store.append_message(&s, summary.clone()).await.unwrap();
    let loaded = store.load_session(&s).await.unwrap();
    assert_eq!(loaded.messages, vec![said, summary], "a summary must keep its marker (D-029)");
}

/// `first_message` is the first user text, whatever comes before or after it.
async fn first_message_is_the_first_user_text(store: &dyn Store) {
    let s = store.create_session(new_session(T0)).await.unwrap().id;
    store.append_message(&s, Message::text(Role::System, "a note", Timestamp(1))).await.unwrap();
    store.append_message(&s, Message::text(Role::Agent, "hello", Timestamp(2))).await.unwrap();
    store.append_message(&s, Message::text(Role::User, "fix the build", Timestamp(3))).await.unwrap();
    store.append_message(&s, Message::text(Role::User, "and the tests", Timestamp(4))).await.unwrap();
    let first = Some("fix the build".to_string());
    assert_eq!(store.load_session(&s).await.unwrap().info.first_message, first, "first user text");
    let listed = store.list_sessions().await.unwrap().into_iter().find(|i| i.id == s).expect("listed");
    assert_eq!(listed.first_message, first, "list_sessions carries the first user text");
}
