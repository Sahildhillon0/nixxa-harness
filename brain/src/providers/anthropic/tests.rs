//! Tests for [`AnthropicBrain`] against recorded SSE fixtures. No test here touches the network
//! except `live_conformance`, which is `#[ignore]`d and run by hand.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use serde_json::{Value, json};

use nixxa_contracts::conformance::brain::run;
use nixxa_contracts::{
    AccountInfo, AccountRef, Action, ActionId, ActionRequest, Brain, BrainError, BrainEvent, Content, Context,
    Credential, Credentials, CredentialsError, Intent, KnowledgeAction, Limits, Message, Primer, ProviderId, Role,
    SandboxAction, SandboxActionKind, Secret, SessionId, SourceId, Timestamp, ToolSpec,
};

use super::{AnthropicBrain, AnthropicConfig, DEFAULT_MODEL};
use crate::providers::common::history::TRIMMED_NOTE;
use crate::providers::common::transport::{Method, Request, Response, Transport};

const KEY: &str = "sk-ant-api03-test-key-never-shown-9f2c";

// ---------------------------------------------------------------------------------------------
// Fakes
// ---------------------------------------------------------------------------------------------

/// One canned response.
struct Canned {
    status: u16,
    retry_after: Option<String>,
    body: Vec<u8>,
}

/// What a request looked like, minus the key (checked separately).
#[derive(Debug, Clone)]
struct Sent {
    method: Method,
    url: String,
    key: Option<String>,
    version: Option<String>,
    beta: Option<String>,
    body: Option<Value>,
}

/// Replays canned responses, delivering bodies in small chunks to exercise the parser.
#[derive(Default)]
struct Fixtures {
    responses: Mutex<VecDeque<Result<Canned, String>>>,
    sent: Mutex<Vec<Sent>>,
}

impl Fixtures {
    fn with(responses: Vec<Result<Canned, String>>) -> Arc<Self> {
        Arc::new(Self { responses: Mutex::new(responses.into()), sent: Mutex::default() })
    }

    fn sent(&self) -> Vec<Sent> {
        self.sent.lock().unwrap().clone()
    }
}

#[async_trait]
impl Transport for Fixtures {
    async fn send(&self, request: Request) -> Result<Response, String> {
        let header = |name: &str| request.headers.get(name).and_then(|v| v.to_str().ok()).map(str::to_string);
        self.sent.lock().unwrap().push(Sent {
            method: request.method,
            url: request.url.clone(),
            key: header("x-api-key"),
            version: header("anthropic-version"),
            beta: header("anthropic-beta"),
            body: request.body.as_deref().map(|b| serde_json::from_slice(b).unwrap()),
        });
        let canned = self.responses.lock().unwrap().pop_front().expect("a canned response")?;
        let chunks: Vec<Result<Bytes, String>> =
            canned.body.chunks(7).map(|chunk| Ok(Bytes::copy_from_slice(chunk))).collect();
        Ok(Response {
            status: canned.status,
            retry_after: canned.retry_after,
            body: futures::stream::iter(chunks).boxed(),
        })
    }
}

fn sse(name: &str) -> Result<Canned, String> {
    let path = format!("{}/src/providers/anthropic/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    let body = std::fs::read(&path).unwrap_or_else(|_| panic!("fixture {path}"));
    Ok(Canned { status: 200, retry_after: None, body })
}

fn http_error(status: u16, kind: &str, message: &str) -> Result<Canned, String> {
    let body = json!({ "type": "error", "error": { "type": kind, "message": message } });
    Ok(Canned { status, retry_after: None, body: serde_json::to_vec(&body).unwrap() })
}

/// A credentials store holding one Anthropic key.
struct OneKey(Credential);

#[async_trait]
impl Credentials for OneKey {
    async fn list(&self, _provider: Option<&ProviderId>) -> Result<Vec<AccountInfo>, CredentialsError> {
        Ok(Vec::new())
    }

    async fn get(&self, account: &AccountRef) -> Result<Credential, CredentialsError> {
        if account == &anthropic() { Ok(self.0.clone()) } else { Err(CredentialsError::NotFound(account.clone())) }
    }

    async fn default_account(&self, _provider: &ProviderId) -> Result<Option<AccountRef>, CredentialsError> {
        Ok(Some(anthropic()))
    }

    async fn add(&self, _account: AccountRef, _credential: Credential) -> Result<AccountInfo, CredentialsError> {
        Err(CredentialsError::Unsupported("test".into()))
    }

    async fn remove(&self, _account: &AccountRef) -> Result<(), CredentialsError> {
        Ok(())
    }

    async fn set_default(&self, _account: &AccountRef) -> Result<(), CredentialsError> {
        Ok(())
    }

    async fn validate(&self, _account: &AccountRef) -> Result<(), CredentialsError> {
        Ok(())
    }
    async fn fingerprints(&self) -> Result<nixxa_contracts::SecretFingerprints, CredentialsError> {
        Err(CredentialsError::Unsupported("not needed by the brain".into()))
    }
}

fn anthropic() -> AccountRef {
    AccountRef::new("anthropic", "default")
}

fn brain_with(fixtures: &Arc<Fixtures>, config: AnthropicConfig) -> AnthropicBrain {
    AnthropicBrain::with_transport(Arc::new(OneKey(Credential::ApiKey(Secret::new(KEY)))), config, fixtures.clone())
}

fn brain(fixtures: &Arc<Fixtures>) -> AnthropicBrain {
    brain_with(fixtures, AnthropicConfig::default())
}

fn context(messages: Vec<Message>) -> Context {
    Context {
        session: SessionId::new("s1"),
        account: anthropic(),
        model: None,
        instructions: "You are Nixxa.".into(),
        primer: Primer::default(),
        messages,
        sandbox_actions: SandboxActionKind::ALL.to_vec(),
        knowledge_tools: vec![ToolSpec {
            source: SourceId::new("notes"),
            name: "search".into(),
            description: "Search notes.".into(),
            input_schema: json!({ "type": "object", "properties": { "query": { "type": "string" } }, "required": ["query"] }),
            mutating: false,
        }],
        limits: Limits::default(),
    }
}

fn hello() -> Context {
    context(vec![Message::text(Role::User, "hello", Timestamp(0))])
}

async fn collect(brain: &AnthropicBrain, context: Context) -> Vec<Result<BrainEvent, BrainError>> {
    brain.think(context).await.expect("think").collect().await
}

fn ok(items: Vec<Result<BrainEvent, BrainError>>) -> Vec<BrainEvent> {
    items.into_iter().map(|item| item.expect("no error")).collect()
}

fn say(text: &str) -> BrainEvent {
    BrainEvent::Intent { intent: Intent::Say { text: text.into() } }
}

fn done(summary: &str) -> BrainEvent {
    BrainEvent::Intent { intent: Intent::Done { summary: summary.into() } }
}

fn reasoning(signature: &str) -> BrainEvent {
    BrainEvent::Reasoning {
        provider: ProviderId::new("anthropic"),
        data: json!({ "type": "thinking", "thinking": "", "signature": signature }),
    }
}

fn act(id: &str, action: Action) -> BrainEvent {
    BrainEvent::Intent {
        intent: Intent::Act { request: ActionRequest { id: ActionId::new(id), action, reason: None } },
    }
}

fn exec(command: &str) -> Action {
    Action::Sandbox {
        action: SandboxAction::Exec { command: command.into(), cwd: None, timeout_ms: None, background: false },
    }
}

fn usage_of(events: &[BrainEvent]) -> nixxa_contracts::Usage {
    events
        .iter()
        .find_map(|e| match e {
            BrainEvent::Usage { usage } => Some(*usage),
            _ => None,
        })
        .expect("a usage event")
}

// ---------------------------------------------------------------------------------------------
// Streams
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn plain_text_streams_deltas_then_the_final_intents() {
    let fixtures = Fixtures::with(vec![sse("text.sse")]);
    let events = ok(collect(&brain(&fixtures), hello()).await);
    assert_eq!(
        events,
        vec![
            BrainEvent::TextDelta { text: "Hello".into() },
            BrainEvent::TextDelta { text: " there.".into() },
            reasoning("EqQBsig-text"),
            say("Hello there."),
            done("done"),
            BrainEvent::Usage {
                usage: nixxa_contracts::Usage { input_tokens: 20, output_tokens: 12, cost_micro_usd: Some(160) }
            },
        ]
    );
}

#[tokio::test]
async fn a_tool_call_becomes_an_act_after_its_reasoning_and_text() {
    let fixtures = Fixtures::with(vec![sse("tool_use.sse")]);
    let events = ok(collect(&brain(&fixtures), hello()).await);
    let finals: Vec<_> =
        events.into_iter().filter(|e| !matches!(e, BrainEvent::TextDelta { .. } | BrainEvent::Usage { .. })).collect();
    assert_eq!(finals, vec![reasoning("EqQBsig-tool"), say("Let me look."), act("toolu_01A", exec("ls -la"))]);
}

#[tokio::test]
async fn parallel_tool_calls_route_to_sandbox_and_knowledge() {
    let fixtures = Fixtures::with(vec![sse("parallel_tool_use.sse")]);
    let events = ok(collect(&brain(&fixtures), hello()).await);
    let acts: Vec<_> = events.into_iter().filter(|e| matches!(e, BrainEvent::Intent { .. })).collect();
    assert_eq!(
        acts,
        vec![
            act("toolu_01A", exec("cargo test")),
            act(
                "toolu_01B",
                Action::Knowledge {
                    action: KnowledgeAction::Call {
                        source: SourceId::new("notes"),
                        tool: "search".into(),
                        args: json!({ "query": "build notes" })
                    }
                }
            ),
        ]
    );
}

#[tokio::test]
async fn max_tokens_says_what_arrived_and_never_runs_a_cut_off_call() {
    let fixtures = Fixtures::with(vec![sse("max_tokens.sse")]);
    let events = ok(collect(&brain(&fixtures), hello()).await);
    let intents: Vec<_> = events.into_iter().filter(|e| matches!(e, BrainEvent::Intent { .. })).collect();
    assert_eq!(intents, vec![say("Partial an"), done("stopped: output limit")]);
}

#[tokio::test]
async fn a_refusal_is_a_provider_error_after_usage() {
    let fixtures = Fixtures::with(vec![sse("refusal.sse")]);
    let items = collect(&brain(&fixtures), hello()).await;
    let last = items.last().unwrap().clone().unwrap_err();
    assert!(matches!(&last, BrainError::Provider(m) if m.contains("declined") && m.contains("cyber")), "{last}");
    assert!(items.iter().any(|i| matches!(i, Ok(BrainEvent::Usage { .. }))));
    assert!(
        !items.iter().any(|i| matches!(i, Ok(BrainEvent::Intent { .. }))),
        "nothing from the declined answer is kept"
    );
}

#[tokio::test]
async fn a_mid_stream_fallback_keeps_text_but_drops_the_declined_calls_and_thinking() {
    let fixtures = Fixtures::with(vec![sse("fallback.sse")]);
    let events = ok(collect(&brain(&fixtures), hello()).await);
    let finals: Vec<_> = events
        .iter()
        .filter(|e| !matches!(e, BrainEvent::TextDelta { .. } | BrainEvent::Usage { .. }))
        .cloned()
        .collect();
    assert_eq!(finals, vec![say("Sure, "), reasoning("fallback-sig"), say("Here is the answer."), done("done")]);
    assert_eq!(usage_of(&events).cost_micro_usd, None, "two models ran: the price is not guessed");
}

#[tokio::test]
async fn redacted_thinking_is_kept_verbatim() {
    let fixtures = Fixtures::with(vec![sse("redacted_thinking.sse")]);
    let events = ok(collect(&brain(&fixtures), hello()).await);
    assert!(events.contains(&BrainEvent::Reasoning {
        provider: ProviderId::new("anthropic"),
        data: json!({ "type": "redacted_thinking", "data": "EmwKAhgBEgy3va3pzix" })
    }));
}

#[tokio::test]
async fn broken_streams_end_in_one_clear_error() {
    for (fixture, expected) in [
        ("stream_error.sse", "overloaded_error"),
        ("malformed.sse", "malformed"),
        ("truncated.sse", "ended before"),
        ("unknown_tool.sse", "not offered"),
        ("invalid_input.sse", "must be a string"),
    ] {
        let fixtures = Fixtures::with(vec![sse(fixture)]);
        let items = collect(&brain(&fixtures), hello()).await;
        let errors: Vec<_> = items.iter().filter_map(|i| i.as_ref().err()).collect();
        assert_eq!(errors.len(), 1, "{fixture}: {items:?}");
        assert!(errors[0].to_string().contains(expected), "{fixture}: {}", errors[0]);
        assert!(items.last().unwrap().is_err(), "{fixture}: the error ends the stream");
        assert!(!items.iter().any(|i| matches!(i, Ok(BrainEvent::Intent { intent: Intent::Act { .. } }))), "{fixture}");
    }
}

// ---------------------------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_request_carries_key_version_tools_caching_and_fallback() {
    let fixtures = Fixtures::with(vec![sse("text.sse")]);
    collect(&brain(&fixtures), hello()).await;
    let sent = fixtures.sent().remove(0);
    assert_eq!(sent.method, Method::Post);
    assert_eq!(sent.url, "https://api.anthropic.com/v1/messages");
    assert_eq!(sent.key.as_deref(), Some(KEY));
    assert_eq!(sent.version.as_deref(), Some("2026-10-21"));
    assert_eq!(sent.beta.as_deref(), Some("server-side-fallback-2026-07-01"));
    let body = sent.body.unwrap();
    assert_eq!(body["model"], DEFAULT_MODEL);
    assert_eq!(body["max_tokens"], 64_000);
    assert_eq!(body["stream"], true);
    assert_eq!(body["fallbacks"], "default");
    assert_eq!(body["cache_control"], json!({ "type": "ephemeral" }));
    assert_eq!(body["system"][0]["text"], "You are Nixxa.");
    let tools: Vec<_> =
        body["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_string()).collect();
    assert_eq!(
        tools,
        vec![
            "exec",
            "read_file",
            "write_file",
            "edit_file",
            "list_dir",
            "job_status",
            "job_output",
            "job_stop",
            "notes__search"
        ]
    );
    assert!(body.get("thinking").is_none(), "Sonnet 5.5 thinks adaptively by default");
    assert!(body.get("temperature").is_none());
}

#[tokio::test]
async fn the_context_can_change_model_and_output_cap() {
    let fixtures = Fixtures::with(vec![sse("text.sse")]);
    let mut context = hello();
    context.model = Some("claude-haiku-5-5".into());
    context.limits.max_output_tokens = Some(64);
    context.sandbox_actions.clear();
    context.knowledge_tools.clear();
    collect(&brain(&fixtures), context).await;
    let sent = fixtures.sent().remove(0);
    let body = sent.body.unwrap();
    assert_eq!(body["model"], "claude-haiku-5-5");
    assert_eq!(body["max_tokens"], 64);
    assert!(body.get("tools").is_none(), "no tools when the context offers none");
    assert!(body.get("fallbacks").is_none() && sent.beta.is_none(), "Haiku 5.5 has no server-side fallback");
}

#[tokio::test]
async fn fallback_and_caching_can_be_turned_off() {
    let fixtures = Fixtures::with(vec![sse("text.sse")]);
    let config = AnthropicConfig { refusal_fallback: false, prompt_caching: false, ..AnthropicConfig::default() };
    collect(&brain_with(&fixtures, config), hello()).await;
    let sent = fixtures.sent().remove(0);
    let body = sent.body.unwrap();
    assert!(sent.beta.is_none() && body.get("fallbacks").is_none() && body.get("cache_control").is_none());
}

#[tokio::test]
async fn effort_is_sent_only_when_configured() {
    let fixtures = Fixtures::with(vec![sse("text.sse"), sse("text.sse")]);
    collect(&brain(&fixtures), hello()).await;
    let config = AnthropicConfig { effort: Some(super::Effort::Medium), ..AnthropicConfig::default() };
    collect(&brain_with(&fixtures, config), hello()).await;
    let sent = fixtures.sent();
    assert!(sent[0].body.as_ref().unwrap().get("output_config").is_none());
    assert_eq!(sent[1].body.as_ref().unwrap()["output_config"], json!({ "effort": "medium" }));
}

#[tokio::test]
async fn history_with_reasoning_is_replayed_in_order() {
    let fixtures = Fixtures::with(vec![sse("text.sse")]);
    let request = ActionRequest { id: ActionId::new("toolu_01A"), action: exec("ls -la"), reason: None };
    let observation = nixxa_contracts::Observation::new(
        ActionId::new("toolu_01A"),
        nixxa_contracts::Outcome::Exec { exit_code: Some(0), stdout: "a.rs".into(), stderr: String::new() },
    );
    let at = Timestamp(0);
    let messages = vec![
        Message::text(Role::User, "list", at),
        Message {
            role: Role::Agent,
            content: Content::Reasoning {
                provider: ProviderId::new("anthropic"),
                data: json!({ "type": "thinking", "thinking": "", "signature": "sig" }),
            },
            at,
        },
        Message { role: Role::Agent, content: Content::Action { request }, at },
        Message { role: Role::Observation, content: Content::Observation { observation }, at },
    ];
    collect(&brain(&fixtures), context(messages)).await;
    let body = fixtures.sent().remove(0).body.unwrap();
    assert_eq!(body["messages"][1]["content"][0], json!({ "type": "thinking", "thinking": "", "signature": "sig" }));
    assert_eq!(body["messages"][1]["content"][1]["type"], "tool_use");
    assert_eq!(body["messages"][2]["content"][0]["tool_use_id"], "toolu_01A");
}

#[tokio::test]
async fn a_long_history_is_trimmed_and_drops_its_thinking() {
    let fixtures = Fixtures::with(vec![sse("text.sse")]);
    let at = Timestamp(0);
    let mut messages = Vec::new();
    for i in 0..20 {
        messages.push(Message::text(Role::User, format!("question {i} {}", "x".repeat(2000)), at));
        messages.push(Message {
            role: Role::Agent,
            content: Content::Reasoning {
                provider: ProviderId::new("anthropic"),
                data: json!({ "type": "thinking", "thinking": "", "signature": format!("sig-{i}") }),
            },
            at,
        });
        messages.push(Message::text(Role::Agent, format!("answer {i}"), at));
    }
    let config = AnthropicConfig { context_window: 10_000, max_output_tokens: 1_000, ..AnthropicConfig::default() };
    collect(&brain_with(&fixtures, config), context(messages)).await;
    let body = fixtures.sent().remove(0).body.unwrap();
    let sent = body["messages"].as_array().unwrap();
    assert_eq!(sent[0]["content"][0]["text"], TRIMMED_NOTE);
    assert!(sent.len() < 40, "older exchanges were dropped");
    let all = body["messages"].to_string();
    assert!(all.contains("question 19") && !all.contains("question 0 "), "the newest is kept, the oldest dropped");
    assert!(
        !body["messages"].to_string().contains("\"thinking\""),
        "a changed prefix invalidates every thinking block"
    );
}

#[tokio::test]
async fn a_context_that_cannot_fit_is_context_too_large() {
    let fixtures = Fixtures::with(vec![]);
    let messages = vec![Message::text(Role::User, "x".repeat(100_000), Timestamp(0))];
    let config = AnthropicConfig { context_window: 10_000, max_output_tokens: 1_000, ..AnthropicConfig::default() };
    let err = brain_with(&fixtures, config).think(context(messages)).await.err().unwrap();
    assert_eq!(err, BrainError::ContextTooLarge);
}

// ---------------------------------------------------------------------------------------------
// Errors and credentials
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn http_errors_map_to_brain_errors_without_the_key() {
    type Check = fn(&BrainError) -> bool;
    let cases: Vec<(Result<Canned, String>, Check)> = vec![
        (http_error(401, "authentication_error", &format!("invalid x-api-key {KEY}")), |e| {
            matches!(e, BrainError::Auth(_))
        }),
        (http_error(403, "permission_error", "not allowed"), |e| matches!(e, BrainError::Auth(_))),
        (
            Ok(Canned {
                status: 429,
                retry_after: Some("12".into()),
                body: serde_json::to_vec(
                    &json!({ "type": "error", "error": { "type": "rate_limit_error", "message": "slow" } }),
                )
                .unwrap(),
            }),
            |e| *e == BrainError::RateLimited { retry_after_ms: Some(12_000) },
        ),
        (http_error(413, "request_too_large", "too big"), |e| *e == BrainError::ContextTooLarge),
        (http_error(400, "invalid_request_error", "prompt is too long: 1048577 tokens > 1000000 maximum"), |e| {
            *e == BrainError::ContextTooLarge
        }),
        (http_error(400, "invalid_request_error", "bad field"), |e| matches!(e, BrainError::Provider(_))),
        (http_error(404, "not_found_error", "model: claude-nope"), |e| matches!(e, BrainError::Provider(_))),
        (http_error(500, "api_error", "internal"), |e| matches!(e, BrainError::Provider(_))),
        (http_error(529, "overloaded_error", "Overloaded"), |e| matches!(e, BrainError::Provider(_))),
        (Err(format!("connection refused (key {KEY})")), |e| matches!(e, BrainError::Network(_))),
    ];
    for (response, expected) in cases {
        let fixtures = Fixtures::with(vec![response]);
        let err = brain(&fixtures).think(hello()).await.err().expect("an error");
        assert!(expected(&err), "{err:?}");
        assert!(!err.to_string().contains(KEY) && !format!("{err:?}").contains(KEY), "the key leaked: {err}");
    }
}

#[tokio::test]
async fn credentials_problems_are_auth_errors() {
    let fixtures = Fixtures::with(vec![]);
    let mut wrong_account = hello();
    wrong_account.account = AccountRef::new("anthropic", "missing");
    assert!(matches!(brain(&fixtures).think(wrong_account).await.err(), Some(BrainError::Auth(_))));

    let mut other_provider = hello();
    other_provider.account = AccountRef::new("openai", "default");
    assert!(matches!(brain(&fixtures).think(other_provider).await.err(), Some(BrainError::Auth(_))));

    let profile = AnthropicBrain::with_transport(
        Arc::new(OneKey(Credential::CloudProfile { profile: "x".into() })),
        AnthropicConfig::default(),
        fixtures.clone(),
    );
    assert!(matches!(profile.think(hello()).await.err(), Some(BrainError::Auth(_))));
    assert!(fixtures.sent().is_empty(), "nothing was sent");
}

#[tokio::test]
async fn verify_reads_the_model_entry() {
    let fixtures = Fixtures::with(vec![
        Ok(Canned { status: 200, retry_after: None, body: br#"{"id":"claude-sonnet-5-5"}"#.to_vec() }),
        http_error(401, "authentication_error", "invalid x-api-key"),
    ]);
    let brain = brain(&fixtures);
    brain.verify(&anthropic()).await.expect("valid");
    assert!(matches!(brain.verify_key(&Secret::new("sk-ant-wrong-key-0000")).await, Err(BrainError::Auth(_))));
    let sent = fixtures.sent();
    assert_eq!(sent[0].method, Method::Get);
    assert_eq!(sent[0].url, "https://api.anthropic.com/v1/models/claude-sonnet-5-5");
    assert_eq!(sent[1].key.as_deref(), Some("sk-ant-wrong-key-0000"));
}

#[tokio::test]
async fn debug_output_never_shows_the_key() {
    let fixtures = Fixtures::with(vec![]);
    let shown = format!("{:?}", brain(&fixtures));
    assert!(!shown.contains(KEY), "{shown}");
}

#[tokio::test]
async fn conforms_with_recorded_streams() {
    let fixtures = Fixtures::with(vec![sse("text.sse")]);
    run(&brain(&fixtures), anthropic()).await;
}

/// Runs the conformance suite against the real API. By hand only; costs a few tokens:
/// `ANTHROPIC_API_KEY=… cargo test -p nixxa-brain -- --ignored live_conformance`.
#[tokio::test]
#[ignore = "calls the real Anthropic API"]
async fn live_conformance() {
    let key = std::env::var("ANTHROPIC_API_KEY").expect("ANTHROPIC_API_KEY");
    let brain =
        AnthropicBrain::new(Arc::new(OneKey(Credential::ApiKey(Secret::new(key.trim())))), AnthropicConfig::default())
            .expect("client");
    brain.verify(&anthropic()).await.expect("the key works");
    run(&brain, anthropic()).await;
}

/// A two-request tool loop against the real API: request 1 makes the model call `exec`;
/// request 2 replays that turn (thinking included) exactly as the controller records it, with
/// the result. A history edit would show up here as a 400. By hand only; costs a few cents:
/// `ANTHROPIC_API_KEY=… cargo test -p nixxa-brain -- --ignored live_tool_loop`.
#[tokio::test]
#[ignore = "calls the real Anthropic API"]
async fn live_tool_loop() {
    let key = std::env::var("ANTHROPIC_API_KEY").expect("ANTHROPIC_API_KEY");
    let brain =
        AnthropicBrain::new(Arc::new(OneKey(Credential::ApiKey(Secret::new(key.trim())))), AnthropicConfig::default())
            .expect("client");
    let at = Timestamp(0);
    let mut context = context(vec![Message::text(
        Role::User,
        "Use the exec tool to run `echo nixxa-live-check`, then reply with only its output.",
        at,
    )]);
    context.knowledge_tools.clear();
    context.limits.max_output_tokens = Some(4_000);

    // Request 1, recorded the way the controller records it: the model's blocks in order, then
    // every observation together. The Done summary is never sent back.
    let mut proposals = Vec::new();
    for item in collect(&brain, context.clone()).await {
        match item.expect("request 1") {
            BrainEvent::Reasoning { provider, data } => {
                context.messages.push(Message {
                    role: Role::Agent,
                    content: Content::Reasoning { provider, data },
                    at,
                });
            }
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
        let observation = nixxa_contracts::Observation::new(
            request.id,
            nixxa_contracts::Outcome::Exec {
                exit_code: Some(0),
                stdout: "nixxa-live-check\n".into(),
                stderr: String::new(),
            },
        );
        context.messages.push(Message { role: Role::Observation, content: Content::Observation { observation }, at });
    }

    // Request 2: the replayed turn must be accepted.
    let items = collect(&brain, context).await;
    assert!(items.iter().all(Result::is_ok), "request 2 failed: {items:?}");
    assert!(items.iter().any(|i| matches!(i, Ok(BrainEvent::Intent { .. }))));
}
