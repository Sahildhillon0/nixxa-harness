//! [`OpenAiCompatBrain`]: any server that speaks the OpenAI Chat Completions API, with streaming
//! and tool calls. Stage 3 uses it for local models (LM Studio, Ollama); later stages for hosted
//! OpenAI-compatible providers.
//!
//! References: the OpenAI API spec (`CreateChatCompletionStreamResponse`,
//! `ChatCompletionMessageToolCallChunk`, `ChatCompletionRequestToolMessage`), LM Studio's
//! OpenAI-compatible endpoints (`http://localhost:1234/v1`) and Ollama's OpenAI compatibility
//! (`http://localhost:11434/v1`: streaming, tools and `stream_options.include_usage`).

mod errors;
mod request;
mod stream;

#[cfg(test)]
mod tests;

use std::collections::VecDeque;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{Value, json};

use nixxa_contracts::{
    AccountRef, Brain, BrainError, BrainEvent, BrainInfo, BrainStream, Context, Credential, Credentials,
    CredentialsError, ProviderId,
};

use self::stream::Decoder;
use crate::providers::common::sse::SseParser;
use crate::providers::common::transport::{Http, Method, Request, Response, Transport, header};
use crate::providers::common::{history, scrub, tools, trim};

/// Local models can take minutes to read a long prompt before the first token on a CPU.
const READ_TIMEOUT: Duration = Duration::from_secs(900);
/// The most of an error response body that is read.
const MAX_ERROR_BODY: usize = 64 * 1024;

/// Settings for [`OpenAiCompatBrain`]. The harness fills them from its config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenAiCompatConfig {
    /// The provider whose accounts this brain serves, e.g. `local`.
    pub provider: ProviderId,
    /// The model id as the server names it; a context's `model` overrides it.
    pub model: String,
    /// Base URL (ending in `/v1`) for accounts whose credential doesn't carry one (API keys). A
    /// [`Credential::Local`] account's own base URL always wins.
    pub base_url: Option<String>,
    /// Output token cap per step, unless the context sets one.
    pub max_output_tokens: u32,
    /// The model's context window in tokens; trimming keeps requests well inside it.
    pub context_window: u64,
    /// Extra headers on every request (e.g. OpenRouter's app attribution). Never secrets.
    pub headers: Vec<(String, String)>,
    /// The server reports each step's cost as `usage.cost` in US dollars (OpenRouter does).
    /// Otherwise cost is `None` for hosted servers: never estimated.
    pub reports_cost: bool,
    /// A path that answers 200 only for a valid key (OpenRouter: `key`); `verify` checks it.
    pub key_check: Option<String>,
}

/// OpenRouter's API.
pub const OPENROUTER_URL: &str = "https://openrouter.ai/api/v1";
/// The app URL sent to OpenRouter (its `HTTP-Referer` attribution header).
const NIXXA_URL: &str = "https://github.com/Sahildhillon0/nixxa-harness";

impl OpenAiCompatConfig {
    /// Defaults sized for local models: a 32k window and 4k output.
    pub fn new(provider: impl Into<ProviderId>, model: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
            base_url: None,
            max_output_tokens: 4_096,
            context_window: 32_768,
            headers: Vec::new(),
            reports_cost: false,
            key_check: None,
        }
    }

    /// OpenRouter (<https://openrouter.ai/docs>): the API at `https://openrouter.ai/api/v1`, the
    /// recommended `HTTP-Referer` and `X-OpenRouter-Title` headers, cost from `usage.cost`, and
    /// key checks at `GET /key`.
    pub fn openrouter(model: impl Into<String>) -> Self {
        Self {
            base_url: Some(OPENROUTER_URL.into()),
            max_output_tokens: 8_192,
            context_window: 128_000,
            headers: vec![("http-referer".into(), NIXXA_URL.into()), ("x-openrouter-title".into(), "Nixxa".into())],
            reports_cost: true,
            key_check: Some("key".into()),
            ..Self::new("openrouter", model)
        }
    }
}

/// A model a server offers. `None` means the server didn't say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelInfo {
    /// The id to request it by.
    pub id: String,
    /// Whether it can call tools.
    pub supports_tools: Option<bool>,
    /// Whether it costs nothing to use.
    pub free: Option<bool>,
}

/// Where and how to reach the server for one request.
struct Endpoint {
    base_url: String,
    /// The bearer key, if the account has one.
    key: Option<String>,
    /// A local server: nothing is billed.
    local: bool,
}

/// Any OpenAI-compatible chat server. Fetches the account's credential on every request.
pub struct OpenAiCompatBrain {
    credentials: Arc<dyn Credentials>,
    config: OpenAiCompatConfig,
    transport: Arc<dyn Transport>,
}

impl fmt::Debug for OpenAiCompatBrain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenAiCompatBrain").field("config", &self.config).finish_non_exhaustive()
    }
}

impl OpenAiCompatBrain {
    /// A brain that talks to the network.
    pub fn new(credentials: Arc<dyn Credentials>, config: OpenAiCompatConfig) -> Result<Self, BrainError> {
        let transport = Http::with_read_timeout(READ_TIMEOUT).map_err(BrainError::Other)?;
        Ok(Self::with_transport(credentials, config, Arc::new(transport)))
    }

    fn with_transport(
        credentials: Arc<dyn Credentials>,
        config: OpenAiCompatConfig,
        transport: Arc<dyn Transport>,
    ) -> Self {
        Self { credentials, config, transport }
    }

    /// The models the account's server offers.
    pub async fn list_models(&self, account: &AccountRef) -> Result<Vec<ModelInfo>, BrainError> {
        let endpoint = self.endpoint(account).await?;
        let response = self.send(&endpoint, Method::Get, "models", None).await?;
        if response.status != 200 {
            return Err(error_response(response, &endpoint).await);
        }
        let body = read_body(response.body).await;
        let value: Value = serde_json::from_slice(&body)
            .map_err(|_| BrainError::Provider("the server's model list is not valid JSON".into()))?;
        Ok(parse_models(&value))
    }

    /// A free live check: the account's server answers, and offers the configured model.
    pub async fn verify(&self, account: &AccountRef) -> Result<(), BrainError> {
        if let Some(path) = &self.config.key_check {
            let endpoint = self.endpoint(account).await?;
            let response = self.send(&endpoint, Method::Get, path, None).await?;
            if response.status != 200 {
                return Err(error_response(response, &endpoint).await);
            }
        }
        let models = self.list_models(account).await?;
        if models.iter().any(|m| m.id == self.config.model) {
            return Ok(());
        }
        let available: Vec<&str> = models.iter().map(|m| m.id.as_str()).take(10).collect();
        Err(BrainError::Provider(format!(
            "the server doesn't offer the model `{}`; it offers: {}",
            self.config.model,
            if available.is_empty() { "none".to_string() } else { available.join(", ") }
        )))
    }

    async fn endpoint(&self, account: &AccountRef) -> Result<Endpoint, BrainError> {
        if account.provider != self.config.provider {
            return Err(BrainError::Auth(format!("`{account}` is not a {} account", self.config.provider)));
        }
        let credential = match self.credentials.get(account).await {
            Ok(credential) => credential,
            Err(
                err @ (CredentialsError::NotFound(_) | CredentialsError::Expired(_) | CredentialsError::Invalid(_)),
            ) => {
                return Err(BrainError::Auth(err.to_string()));
            }
            Err(err) => return Err(BrainError::Other(err.to_string())),
        };
        let (base_url, key, local) = match credential {
            Credential::Local { base_url } => (base_url, None, true),
            Credential::ApiKey(key) | Credential::BearerToken { token: key, .. } => {
                let base_url = self.config.base_url.clone().ok_or_else(|| {
                    BrainError::Auth(format!("`{account}` has a key but no server URL is configured"))
                })?;
                (base_url, Some(key.expose().to_string()), false)
            }
            other => {
                return Err(BrainError::Auth(format!(
                    "`{account}` uses {:?} sign-in, which this brain doesn't support yet",
                    other.method()
                )));
            }
        };
        let base_url = base_url.trim_end_matches('/').to_string();
        if key.is_some() && base_url.starts_with("http://") && !is_loopback(&base_url) {
            return Err(BrainError::Auth(format!("refusing to send a key over plain http to {base_url}; use https")));
        }
        Ok(Endpoint { base_url, key, local })
    }

    async fn send(
        &self,
        endpoint: &Endpoint,
        method: Method,
        path: &str,
        body: Option<Vec<u8>>,
    ) -> Result<Response, BrainError> {
        let mut headers = HeaderMap::new();
        for (name, value) in &self.config.headers {
            let (Ok(name), Ok(value)) = (HeaderName::from_bytes(name.as_bytes()), HeaderValue::from_str(value)) else {
                return Err(BrainError::Other(format!("the configured header `{name}` is invalid")));
            };
            headers.insert(name, value);
        }
        if let Some(key) = &endpoint.key {
            header(&mut headers, "authorization", &format!("Bearer {key}"), true).map_err(BrainError::Auth)?;
        }
        if body.is_some() {
            header(&mut headers, "content-type", "application/json", false).map_err(BrainError::Other)?;
        }
        let url = format!("{}/{path}", endpoint.base_url);
        self.transport.send(Request { method, url, headers, body }).await.map_err(|err| {
            let err = scrub(&err, endpoint.key.as_deref().unwrap_or_default());
            let hint = if endpoint.local { " Is the local server (LM Studio, Ollama…) running?" } else { "" };
            BrainError::Network(format!("couldn't reach {}: {err}.{hint}", endpoint.base_url))
        })
    }

    /// The request body for a context.
    fn body(&self, context: &Context, model: &str) -> Result<(Value, tools::ToolMap), BrainError> {
        let (definitions, map) = tools::build(context);
        let tools_value: Vec<Value> = definitions.iter().map(request::function_tool).collect();
        let system = history::system(&context.instructions, &context.primer);
        let max_tokens = context.limits.max_output_tokens.unwrap_or(self.config.max_output_tokens);
        let fixed = trim::estimate(&json!(system)) + trim::estimate(&Value::Array(tools_value.clone()));
        let budget = self
            .config
            .context_window
            .saturating_sub(u64::from(max_tokens))
            .saturating_sub(self.config.context_window / 10)
            .saturating_sub(fixed);
        let start = trim::keep_from(&context.messages, budget)?;
        let messages = request::messages(&system, &context.messages[start..], start > 0);
        let mut body = json!({
            "model": model,
            "messages": messages,
            "stream": true,
            "stream_options": { "include_usage": true },
            "max_tokens": max_tokens,
        });
        if !tools_value.is_empty() {
            body["tools"] = Value::Array(tools_value);
        }
        Ok((body, map))
    }
}

#[async_trait]
impl Brain for OpenAiCompatBrain {
    fn describe(&self) -> BrainInfo {
        BrainInfo { provider: self.config.provider.clone(), model: self.config.model.clone() }
    }

    async fn think(&self, context: Context) -> Result<BrainStream, BrainError> {
        let model = context.model.clone().unwrap_or_else(|| self.config.model.clone());
        let endpoint = self.endpoint(&context.account).await?;
        let (body, map) = self.body(&context, &model)?;
        let body = serde_json::to_vec(&body)
            .map_err(|err| BrainError::Other(format!("could not encode the request: {err}")))?;
        let response = self.send(&endpoint, Method::Post, "chat/completions", Some(body)).await?;
        if response.status != 200 {
            return Err(errors::explain_no_tools(error_response(response, &endpoint).await, &model));
        }
        let key = endpoint.key.clone().unwrap_or_default();
        Ok(decode(response, Decoder::new(map, key, endpoint.local, model, self.config.reports_cost)))
    }
}

#[cfg(feature = "replay")]
impl OpenAiCompatBrain {
    /// A brain that answers from `replay` instead of the network.
    pub fn replaying(
        credentials: Arc<dyn Credentials>,
        config: OpenAiCompatConfig,
        replay: &crate::providers::common::replay::Replay,
    ) -> Self {
        Self::with_transport(credentials, config, Arc::new(replay.clone()))
    }
}

/// Whether a URL's host is this machine.
fn is_loopback(url: &str) -> bool {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = if let Some(v6) = authority.strip_prefix('[') {
        v6.split(']').next().unwrap_or_default()
    } else {
        authority.rsplit_once(':').map_or(authority, |(host, _)| host)
    };
    host.eq_ignore_ascii_case("localhost") || host == "::1" || host.starts_with("127.")
}

/// Reads the model list: `{"data": [{"id": …}, …]}`, with tool support and price when present.
fn parse_models(value: &Value) -> Vec<ModelInfo> {
    let Some(data) = value["data"].as_array() else { return Vec::new() };
    data.iter()
        .filter_map(|model| {
            let id = model["id"].as_str()?.to_string();
            let list_has = |field: &str, names: &[&str]| {
                model[field].as_array().map(|items| items.iter().filter_map(Value::as_str).any(|s| names.contains(&s)))
            };
            let supports_tools = list_has("supported_parameters", &["tools"])
                .or_else(|| list_has("capabilities", &["tools", "tool_use"]));
            let price = |field: &str| model["pricing"][field].as_str().and_then(|p| p.parse::<f64>().ok());
            let free = if id.ends_with(":free") {
                Some(true)
            } else {
                match (price("prompt"), price("completion")) {
                    (Some(prompt), Some(completion)) => Some(prompt == 0.0 && completion == 0.0),
                    _ => None,
                }
            };
            Some(ModelInfo { id, supports_tools, free })
        })
        .collect()
}

async fn read_body(mut body: futures::stream::BoxStream<'static, Result<bytes::Bytes, String>>) -> Vec<u8> {
    let mut bytes = Vec::new();
    while let Some(Ok(chunk)) = body.next().await {
        bytes.extend_from_slice(&chunk);
        if bytes.len() >= 8 * 1024 * 1024 {
            break;
        }
    }
    bytes
}

/// Reads (a bounded amount of) an error body and maps it.
async fn error_response(response: Response, endpoint: &Endpoint) -> BrainError {
    let Response { status, retry_after, mut body } = response;
    let mut bytes = Vec::new();
    while let Some(Ok(chunk)) = body.next().await {
        bytes.extend_from_slice(&chunk);
        if bytes.len() >= MAX_ERROR_BODY {
            break;
        }
    }
    errors::from_status(status, retry_after.as_deref(), &bytes, endpoint.key.as_deref().unwrap_or_default())
}

/// The response body as a stream of brain events. The first `Err` ends it.
fn decode(response: Response, decoder: Decoder) -> BrainStream {
    struct State {
        body: futures::stream::BoxStream<'static, Result<bytes::Bytes, String>>,
        parser: SseParser,
        decoder: Decoder,
        queue: VecDeque<Result<BrainEvent, BrainError>>,
        done: bool,
    }
    let state =
        State { body: response.body, parser: SseParser::default(), decoder, queue: VecDeque::new(), done: false };
    Box::pin(futures::stream::unfold(state, |mut state| async move {
        loop {
            if let Some(item) = state.queue.pop_front() {
                if item.is_err() {
                    state.done = true;
                    state.queue.clear();
                }
                return Some((item, state));
            }
            if state.done {
                return None;
            }
            match state.body.next().await {
                Some(Ok(chunk)) => match state.parser.push(&chunk) {
                    Ok(events) => {
                        for event in events {
                            state.queue.extend(state.decoder.on_event(&event.data));
                        }
                        if state.decoder.finished() {
                            state.done = true;
                        }
                    }
                    Err(why) => {
                        state.queue.push_back(Err(BrainError::Provider(format!("malformed response stream: {why}"))))
                    }
                },
                Some(Err(err)) => state.queue.push_back(Err(BrainError::Network(err))),
                // Some servers end the stream without `[DONE]`; a finished choice is enough.
                None => {
                    state.queue.extend(state.decoder.on_end());
                    state.done = true;
                }
            }
        }
    }))
}
