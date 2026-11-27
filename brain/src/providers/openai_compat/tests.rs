//! Tests for [`OpenAiCompatBrain`] against recorded SSE fixtures. No test here touches the
//! network except `live_local_tool_loop`, which is `#[ignore]`d and run by hand.

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
    SandboxAction, SandboxActionKind, Secret, SessionId, SourceId, Timestamp, ToolSpec, Usage,
};

use super::{ModelInfo, OpenAiCompatBrain, OpenAiCompatConfig, is_loopback};
use crate::providers::common::transport::{Method, Request, Response, Transport};

const KEY: &str = "sk-test-openai-compat-key-5555";
const LOCAL_URL: &str = "http://localhost:1234/v1";

struct Canned {
    status: u16,
    body: Vec<u8>,
}

#[derive(Debug, Clone)]
struct Sent {
    method: Method,
    url: String,
    authorization: Option<String>,
    headers: Vec<(String, String)>,
    body: Option<Value>,
}

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
        self.sent.lock().unwrap().push(Sent {
            method: request.method,
            url: request.url.clone(),
            authorization: request.headers.get("authorization").and_then(|v| v.to_str().ok()).map(str::to_string),
            headers: request
                .headers
                .iter()
                .filter(|(name, _)| name.as_str() != "authorization")
                .map(|(name, value)| (name.to_string(), value.to_str().unwrap_or_default().to_string()))
                .collect(),
            body: request.body.as_deref().map(|b| serde_json::from_slice(b).unwrap()),
        });
        let canned = self.responses.lock().unwrap().pop_front().expect("a canned response")?;
        let chunks: Vec<Result<Bytes, String>> = canned.body.chunks(5).map(|c| Ok(Bytes::copy_from_slice(c))).collect();
        Ok(Response { status: canned.status, retry_after: None, body: futures::stream::iter(chunks).boxed() })
    }
}

fn fixture(name: &str) -> Result<Canned, String> {
    let path = format!("{}/src/providers/openai_compat/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    Ok(Canned { status: 200, body: std::fs::read(&path).unwrap_or_else(|_| panic!("fixture {path}")) })
}

/// One account, with the credential given.
struct OneAccount(AccountRef, Credential);

#[async_trait]
impl Credentials for OneAccount {
    async fn list(&self, _provider: Option<&ProviderId>) -> Result<Vec<AccountInfo>, CredentialsError> {
        Ok(Vec::new())
    }
    async fn get(&self, account: &AccountRef) -> Result<Credential, CredentialsError> {
        if account == &self.0 { Ok(self.1.clone()) } else { Err(CredentialsError::NotFound(account.clone())) }
    }
    async fn default_account(&self, _provider: &ProviderId) -> Result<Option<AccountRef>, CredentialsError> {
        Ok(Some(self.0.clone()))
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

fn local() -> AccountRef {
    AccountRef::new("local", "default")
}

fn local_brain(fixtures: &Arc<Fixtures>) -> OpenAiCompatBrain {
    OpenAiCompatBrain::with_transport(
        Arc::new(OneAccount(local(), Credential::Local { base_url: format!("{LOCAL_URL}/") })),
        OpenAiCompatConfig::new("local", "qwen3-8b"),
        fixtures.clone(),
    )
}

fn context(messages: Vec<Message>) -> Context {
    Context {
        session: SessionId::new("s1"),
        account: local(),
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

async fn collect(brain: &OpenAiCompatBrain, context: Context) -> Vec<Result<BrainEvent, BrainError>> {
    brain.think(context).await.expect("think").collect().await
}

fn ok(items: Vec<Result<BrainEvent, BrainError>>) -> Vec<BrainEvent> {
    items.into_iter().map(|i| i.expect("no error")).collect()
}

fn intents(events: &[BrainEvent]) -> Vec<Intent> {
    events
        .iter()
        .filter_map(|e| match e {
            BrainEvent::Intent { intent } => Some(intent.clone()),
            _ => None,
        })
        .collect()
}

fn act(id: &str, action: Action) -> Intent {
    Intent::Act { request: ActionRequest { id: ActionId::new(id), action, reason: None } }
}

fn exec(command: &str) -> Action {
    Action::Sandbox {
        action: SandboxAction::Exec { command: command.into(), cwd: None, timeout_ms: None, background: false },
    }
}

const LOCAL_USAGE: BrainEvent =
    BrainEvent::Usage { usage: Usage { input_tokens: 120, output_tokens: 30, cost_micro_usd: Some(0) } };

// ---------------------------------------------------------------------------------------------
// Streams
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn plain_text_streams_then_says_and_finishes() {
    let fixtures = Fixtures::with(vec![fixture("text.sse")]);
    let events = ok(collect(&local_brain(&fixtures), hello()).await);
    assert_eq!(
        events,
        vec![
            BrainEvent::TextDelta { text: "Hello".into() },
            BrainEvent::TextDelta { text: " there.".into() },
            BrainEvent::Intent { intent: Intent::Say { text: "Hello there.".into() } },
            BrainEvent::Intent { intent: Intent::Done { summary: "done".into() } },
            LOCAL_USAGE,
        ]
    );
}

#[tokio::test]
async fn a_streamed_tool_call_becomes_an_act() {
    let fixtures = Fixtures::with(vec![fixture("tool_call.sse")]);
    let events = ok(collect(&local_brain(&fixtures), hello()).await);
    assert_eq!(intents(&events), vec![Intent::Say { text: "Let me look.".into() }, act("call_1", exec("ls -la"))]);
}

#[tokio::test]
async fn parallel_tool_calls_route_to_sandbox_and_knowledge() {
    let fixtures = Fixtures::with(vec![fixture("parallel_tool_calls.sse")]);
    let events = ok(collect(&local_brain(&fixtures), hello()).await);
    let search = Action::Knowledge {
        action: KnowledgeAction::Call {
            source: SourceId::new("notes"),
            tool: "search".into(),
            args: json!({ "query": "build notes" }),
        },
    };
    assert_eq!(intents(&events), vec![act("call_1", exec("cargo test")), act("call_2", search)]);
}

#[tokio::test]
async fn a_whole_call_finishing_with_stop_and_no_done_marker_still_acts() {
    let fixtures = Fixtures::with(vec![fixture("whole_call_stop.sse")]);
    let events = ok(collect(&local_brain(&fixtures), hello()).await);
    assert_eq!(intents(&events), vec![act("call_9", exec("pwd"))]);
    assert_eq!(events.last(), Some(&LOCAL_USAGE));
}

#[tokio::test]
async fn the_output_limit_says_what_arrived_and_never_runs_a_cut_off_call() {
    let fixtures = Fixtures::with(vec![fixture("length.sse")]);
    let events = ok(collect(&local_brain(&fixtures), hello()).await);
    assert_eq!(
        intents(&events),
        vec![Intent::Say { text: "Partial an".into() }, Intent::Done { summary: "stopped: output limit".into() }]
    );
}

#[tokio::test]
async fn missing_call_ids_get_unique_ones() {
    let fixtures = Fixtures::with(vec![fixture("no_ids.sse")]);
    let events = ok(collect(&local_brain(&fixtures), hello()).await);
    assert_eq!(intents(&events), vec![act("call_0", exec("ls")), act("call_1", exec("pwd"))]);
}

#[tokio::test]
async fn broken_streams_end_in_one_clear_error() {
    for (name, expected) in [
        ("content_filter.sse", "filtered"),
        ("stream_error.sse", "model unloaded"),
        ("malformed.sse", "malformed"),
        ("truncated.sse", "ended before"),
        ("unknown_tool.sse", "not offered"),
        ("invalid_arguments.sse", "must be a string"),
    ] {
        let fixtures = Fixtures::with(vec![fixture(name)]);
        let items = collect(&local_brain(&fixtures), hello()).await;
        let errors: Vec<_> = items.iter().filter_map(|i| i.as_ref().err()).collect();
        assert_eq!(errors.len(), 1, "{name}: {items:?}");
        assert!(errors[0].to_string().contains(expected), "{name}: {}", errors[0]);
        assert!(items.last().unwrap().is_err(), "{name}: the error ends the stream");
        assert!(!items.iter().any(|i| matches!(i, Ok(BrainEvent::Intent { intent: Intent::Act { .. } }))), "{name}");
    }
}

// ---------------------------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_local_request_goes_to_the_account_url_without_auth() {
    let fixtures = Fixtures::with(vec![fixture("text.sse")]);
    collect(&local_brain(&fixtures), hello()).await;
    let sent = fixtures.sent().remove(0);
    assert_eq!(sent.method, Method::Post);
    assert_eq!(sent.url, "http://localhost:1234/v1/chat/completions");
    assert!(sent.authorization.is_none());
    let body = sent.body.unwrap();
    assert_eq!(body["model"], "qwen3-8b");
    assert_eq!(body["stream"], true);
    assert_eq!(body["stream_options"], json!({ "include_usage": true }));
    assert_eq!(body["max_tokens"], 4096);
    assert_eq!(body["messages"][0], json!({ "role": "system", "content": "You are Nixxa." }));
    assert_eq!(body["messages"][1], json!({ "role": "user", "content": "hello" }));
    let names: Vec<_> =
        body["tools"].as_array().unwrap().iter().map(|t| t["function"]["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
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
    assert_eq!(body["tools"][0]["type"], "function");
}

#[tokio::test]
async fn a_key_account_uses_the_configured_url_with_a_bearer_header() {
    let fixtures = Fixtures::with(vec![fixture("text.sse")]);
    let account = AccountRef::new("compat", "work");
    let mut config = OpenAiCompatConfig::new("compat", "some-model");
    config.base_url = Some("https://llm.example.com/v1".into());
    let brain = OpenAiCompatBrain::with_transport(
        Arc::new(OneAccount(account.clone(), Credential::ApiKey(Secret::new(KEY)))),
        config,
        fixtures.clone(),
    );
    let mut ctx = hello();
    ctx.account = account;
    let items = collect(&brain, ctx).await;
    assert!(items.iter().all(Result::is_ok));
    let sent = fixtures.sent().remove(0);
    assert_eq!(sent.url, "https://llm.example.com/v1/chat/completions");
    assert_eq!(sent.authorization.as_deref(), Some(format!("Bearer {KEY}").as_str()));
    let usage = items.into_iter().filter_map(Result::ok).find_map(|e| match e {
        BrainEvent::Usage { usage } => Some(usage),
        _ => None,
    });
    assert_eq!(usage.unwrap().cost_micro_usd, None, "a hosted server's price is never guessed");
}

#[tokio::test]
async fn a_key_is_never_sent_over_plain_http_to_another_machine() {
    let fixtures = Fixtures::with(vec![]);
    let account = AccountRef::new("compat", "work");
    let mut config = OpenAiCompatConfig::new("compat", "m");
    config.base_url = Some("http://192.168.1.20:8080/v1".into());
    let brain = OpenAiCompatBrain::with_transport(
        Arc::new(OneAccount(account.clone(), Credential::ApiKey(Secret::new(KEY)))),
        config,
        fixtures.clone(),
    );
    let mut ctx = hello();
    ctx.account = account;
    let err = brain.think(ctx).await.err().unwrap();
    assert!(matches!(&err, BrainError::Auth(m) if m.contains("plain http")), "{err}");
    assert!(fixtures.sent().is_empty());
}

#[test]
fn loopback_hosts_are_recognised() {
    for url in ["http://localhost:1234/v1", "http://127.0.0.1:11434/v1", "http://[::1]:8080/v1", "http://LOCALHOST/v1"]
    {
        assert!(is_loopback(url), "{url}");
    }
    for url in ["http://192.168.1.2/v1", "http://localhost.example.com/v1", "http://example.com/v1"] {
        assert!(!is_loopback(url), "{url}");
    }
}

#[tokio::test]
async fn a_tool_loop_history_is_sent_in_chat_format() {
    let fixtures = Fixtures::with(vec![fixture("text.sse")]);
    let at = Timestamp(0);
    let request = ActionRequest { id: ActionId::new("call_1"), action: exec("ls"), reason: None };
    let observation = nixxa_contracts::Observation::new(
        ActionId::new("call_1"),
        nixxa_contracts::Outcome::Exec { exit_code: Some(0), stdout: "a.rs".into(), stderr: String::new() },
    );
    let messages = vec![
        Message::text(Role::User, "list", at),
        Message { role: Role::Agent, content: Content::Action { request }, at },
        Message { role: Role::Observation, content: Content::Observation { observation }, at },
    ];
    collect(&local_brain(&fixtures), context(messages)).await;
    let body = fixtures.sent().remove(0).body.unwrap();
    assert_eq!(
        body["messages"][2]["tool_calls"][0]["function"],
        json!({ "name": "exec", "arguments": "{\"command\":\"ls\"}" })
    );
    assert_eq!(body["messages"][3]["role"], "tool");
    assert_eq!(body["messages"][3]["tool_call_id"], "call_1");
}

#[tokio::test]
async fn a_long_history_is_trimmed() {
    let fixtures = Fixtures::with(vec![fixture("text.sse")]);
    let at = Timestamp(0);
    let mut messages = Vec::new();
    for i in 0..20 {
        messages.push(Message::text(Role::User, format!("question {i} {}", "x".repeat(2000)), at));
        messages.push(Message::text(Role::Agent, format!("answer {i}"), at));
    }
    messages.push(Message::text(Role::User, "last question", at));
    let mut config = OpenAiCompatConfig::new("local", "qwen3-8b");
    config.context_window = 8_000;
    config.max_output_tokens = 1_000;
    let brain = OpenAiCompatBrain::with_transport(
        Arc::new(OneAccount(local(), Credential::Local { base_url: LOCAL_URL.into() })),
        config,
        fixtures.clone(),
    );
    collect(&brain, context(messages)).await;
    let all = fixtures.sent().remove(0).body.unwrap()["messages"].to_string();
    assert!(all.contains("trimmed to fit") && all.contains("last question") && !all.contains("question 0 "));
}

// ---------------------------------------------------------------------------------------------
// Errors, models, credentials
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn an_unreachable_local_server_says_what_to_check() {
    let fixtures = Fixtures::with(vec![Err("error sending request: connection refused".into())]);
    let err = local_brain(&fixtures).think(hello()).await.err().unwrap();
    let BrainError::Network(message) = &err else { panic!("{err:?}") };
    assert!(message.contains("http://localhost:1234/v1") && message.contains("running"), "{message}");
}

#[tokio::test]
async fn http_errors_map_to_brain_errors() {
    let body = |m: &str| serde_json::to_vec(&json!({ "error": { "message": m } })).unwrap();
    for (status, message, expected) in [
        (401_u16, "bad key", "authentication failed"),
        (404, "model not found", "not found"),
        (400, "maximum context length is 8192 tokens", "context too large"),
        (500, "boom", "HTTP 500"),
    ] {
        let fixtures = Fixtures::with(vec![Ok(Canned { status, body: body(message) })]);
        let err = local_brain(&fixtures).think(hello()).await.err().unwrap();
        assert!(err.to_string().contains(expected), "{status}: {err}");
    }
}

/// Review S2: a model that can't use tools gets a hint, not the server's raw error.
#[tokio::test]
async fn a_model_without_tools_is_explained() {
    let fixtures = Fixtures::with(vec![Ok(Canned {
        status: 400,
        body: br#"{"error":"registry.ollama.ai/library/qwen-uncensored does not support tools"}"#.to_vec(),
    })]);
    let err = local_brain(&fixtures).think(hello()).await.err().unwrap();
    assert_eq!(
        err.to_string(),
        "provider error: `qwen3-8b` can't use tools. Pick a tool-capable model (e.g. gpt-oss, qwen2.5/3, llama3.1+)."
    );
}

#[tokio::test]
async fn list_models_reads_ids_and_what_the_server_says_about_them() {
    let fixtures = Fixtures::with(vec![fixture("models_local.json"), fixture("models_rich.json")]);
    let brain = local_brain(&fixtures);
    let local_models = brain.list_models(&local()).await.unwrap();
    assert_eq!(
        local_models,
        vec![
            ModelInfo { id: "qwen3-8b".into(), supports_tools: None, free: None },
            ModelInfo { id: "llama-3.2-3b-instruct".into(), supports_tools: None, free: None },
        ]
    );
    let rich = brain.list_models(&local()).await.unwrap();
    assert_eq!(
        rich,
        vec![
            ModelInfo { id: "vendor/tool-model".into(), supports_tools: Some(true), free: Some(false) },
            ModelInfo { id: "vendor/free-model:free".into(), supports_tools: Some(false), free: Some(true) },
            ModelInfo { id: "vendor/zero-priced".into(), supports_tools: Some(true), free: Some(true) },
        ]
    );
    let sent = fixtures.sent();
    assert_eq!(sent[0].method, Method::Get);
    assert_eq!(sent[0].url, "http://localhost:1234/v1/models");
}

#[tokio::test]
async fn verify_checks_the_model_is_offered() {
    let fixtures = Fixtures::with(vec![fixture("models_local.json"), fixture("models_local.json")]);
    local_brain(&fixtures).verify(&local()).await.expect("qwen3-8b is offered");
    let missing = OpenAiCompatBrain::with_transport(
        Arc::new(OneAccount(local(), Credential::Local { base_url: LOCAL_URL.into() })),
        OpenAiCompatConfig::new("local", "gpt-oss-120b"),
        fixtures.clone(),
    );
    let err = missing.verify(&local()).await.unwrap_err();
    assert!(
        err.to_string().contains("doesn't offer the model `gpt-oss-120b`") && err.to_string().contains("qwen3-8b"),
        "{err}"
    );
}

#[tokio::test]
async fn account_problems_are_auth_errors_and_nothing_is_sent() {
    let fixtures = Fixtures::with(vec![]);
    let brain = local_brain(&fixtures);
    let mut wrong = hello();
    wrong.account = AccountRef::new("local", "missing");
    assert!(matches!(brain.think(wrong).await.err(), Some(BrainError::Auth(_))));
    let mut other = hello();
    other.account = AccountRef::new("anthropic", "default");
    assert!(matches!(brain.think(other).await.err(), Some(BrainError::Auth(_))));
    let no_url = OpenAiCompatBrain::with_transport(
        Arc::new(OneAccount(local(), Credential::ApiKey(Secret::new(KEY)))),
        OpenAiCompatConfig::new("local", "m"),
        fixtures.clone(),
    );
    assert!(matches!(no_url.think(hello()).await.err(), Some(BrainError::Auth(m)) if m.contains("no server URL")));
    assert!(fixtures.sent().is_empty());
}

#[tokio::test]
async fn debug_output_never_shows_the_key() {
    let fixtures = Fixtures::with(vec![]);
    let brain = OpenAiCompatBrain::with_transport(
        Arc::new(OneAccount(local(), Credential::ApiKey(Secret::new(KEY)))),
        OpenAiCompatConfig::new("local", "m"),
        fixtures,
    );
    assert!(!format!("{brain:?}").contains(KEY));
}

const OPENROUTER_TOKEN: &str = "sk-or-v1-signed-in-test-key-81aa";

fn openrouter_brain(fixtures: &Arc<Fixtures>) -> OpenAiCompatBrain {
    let account = AccountRef::new("openrouter", "default");
    let token = Credential::BearerToken { token: Secret::new(OPENROUTER_TOKEN), expires_at: None };
    OpenAiCompatBrain::with_transport(
        Arc::new(OneAccount(account, token)),
        OpenAiCompatConfig::openrouter("vendor/tool-model"),
        fixtures.clone(),
    )
}

fn openrouter_context() -> Context {
    let mut ctx = hello();
    ctx.account = AccountRef::new("openrouter", "default");
    ctx
}

#[tokio::test]
async fn openrouter_gets_the_signed_in_key_its_url_and_attribution_headers() {
    let fixtures = Fixtures::with(vec![fixture("openrouter_text.sse")]);
    let items = collect(&openrouter_brain(&fixtures), openrouter_context()).await;
    let sent = fixtures.sent().remove(0);
    assert_eq!(sent.url, "https://openrouter.ai/api/v1/chat/completions");
    assert_eq!(sent.authorization.as_deref(), Some(format!("Bearer {OPENROUTER_TOKEN}").as_str()));
    let header = |name: &str| sent.headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.clone());
    assert_eq!(header("http-referer").as_deref(), Some("https://github.com/Sahildhillon0/nixxa-harness"));
    assert_eq!(header("x-openrouter-title").as_deref(), Some("Nixxa"));
    assert!(sent.headers.iter().all(|(_, v)| !v.contains(OPENROUTER_TOKEN)), "the key is only in authorization");
    let usage = items.into_iter().filter_map(Result::ok).find_map(|e| match e {
        BrainEvent::Usage { usage } => Some(usage),
        _ => None,
    });
    assert_eq!(
        usage,
        Some(Usage { input_tokens: 50, output_tokens: 5, cost_micro_usd: Some(421) }),
        "the reported cost"
    );
}

#[tokio::test]
async fn a_cost_from_a_server_not_known_to_report_one_is_ignored() {
    let fixtures = Fixtures::with(vec![fixture("openrouter_text.sse")]);
    let account = AccountRef::new("compat", "work");
    let mut config = OpenAiCompatConfig::new("compat", "m");
    config.base_url = Some("https://llm.example.com/v1".into());
    let brain = OpenAiCompatBrain::with_transport(
        Arc::new(OneAccount(account.clone(), Credential::ApiKey(Secret::new(KEY)))),
        config,
        fixtures.clone(),
    );
    let mut ctx = hello();
    ctx.account = account;
    let usage = collect(&brain, ctx).await.into_iter().filter_map(Result::ok).find_map(|e| match e {
        BrainEvent::Usage { usage } => Some(usage),
        _ => None,
    });
    assert_eq!(usage.unwrap().cost_micro_usd, None);
}

#[tokio::test]
async fn openrouter_verify_checks_the_key_then_the_model() {
    let fixtures = Fixtures::with(vec![
        Ok(Canned { status: 200, body: br#"{"data":{"label":"nixxa","limit":null,"usage":0}}"#.to_vec() }),
        fixture("models_rich.json"),
        Ok(Canned { status: 401, body: br#"{"error":{"message":"No auth credentials found","code":401}}"#.to_vec() }),
    ]);
    let brain = openrouter_brain(&fixtures);
    let account = AccountRef::new("openrouter", "default");
    brain.verify(&account).await.expect("valid key, offered model");
    let err = brain.verify(&account).await.unwrap_err();
    assert!(matches!(&err, BrainError::Auth(m) if m.contains("No auth credentials")), "{err:?}");
    let sent = fixtures.sent();
    let urls: Vec<_> = sent.iter().map(|s| s.url.as_str()).collect();
    assert_eq!(
        urls,
        vec![
            "https://openrouter.ai/api/v1/key",
            "https://openrouter.ai/api/v1/models",
            "https://openrouter.ai/api/v1/key"
        ],
        "a rejected key stops before the model list"
    );
}

#[tokio::test]
async fn conforms_with_recorded_streams() {
    let fixtures = Fixtures::with(vec![fixture("text.sse")]);
    run(&local_brain(&fixtures), local()).await;
}

/// A local model, for real: the conformance suite, then a two-request tool loop. By hand only,
/// with a server running (LM Studio, Ollama…) and a tool-capable model:
/// `NIXXA_LOCAL_URL=http://localhost:1234/v1 NIXXA_LOCAL_MODEL=qwen3-8b cargo test -p nixxa-brain -- --ignored live_local_tool_loop`
#[tokio::test]
#[ignore = "needs a local OpenAI-compatible server"]
async fn live_local_tool_loop() {
    let base_url = std::env::var("NIXXA_LOCAL_URL").unwrap_or_else(|_| LOCAL_URL.to_string());
    let model = std::env::var("NIXXA_LOCAL_MODEL").expect("NIXXA_LOCAL_MODEL");
    let brain = OpenAiCompatBrain::new(
        Arc::new(OneAccount(local(), Credential::Local { base_url })),
        OpenAiCompatConfig::new("local", model),
    )
    .expect("client");
    brain.verify(&local()).await.expect("the server offers the model");
    run(&brain, local()).await;

    let at = Timestamp(0);
    let mut ctx = context(vec![Message::text(
        Role::User,
        "Use the exec tool to run `echo nixxa-live-check`, then reply with only its output.",
        at,
    )]);
    ctx.knowledge_tools.clear();
    let mut proposals = Vec::new();
    for item in collect(&brain, ctx.clone()).await {
        match item.expect("request 1") {
            BrainEvent::Intent { intent: Intent::Say { text } } => {
                ctx.messages.push(Message::text(Role::Agent, text, at))
            }
            BrainEvent::Intent { intent: Intent::Act { request } } => {
                ctx.messages.push(Message {
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
        ctx.messages.push(Message { role: Role::Observation, content: Content::Observation { observation }, at });
    }
    let items = collect(&brain, ctx).await;
    assert!(items.iter().all(Result::is_ok), "request 2 failed: {items:?}");
}
