//! [`AnthropicBrain`]: Claude through the Messages API, over HTTP with streaming and tool use.

mod errors;
mod pricing;
mod request;
mod sse;
mod stream;
mod tools;
mod transport;
mod trim;

#[cfg(test)]
mod tests;

use std::collections::VecDeque;
use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use reqwest::header::HeaderMap;
use serde_json::{Value, json};

use nixxa_contracts::{
    AccountRef, Brain, BrainError, BrainEvent, BrainInfo, BrainStream, Context, Credential, Credentials,
    CredentialsError, ProviderId, Secret,
};

use self::sse::SseParser;
use self::stream::Decoder;
use self::transport::{Http, Method, Request, Response, Transport, header};

/// The model used when neither the config nor the context names one: the current Sonnet.
pub const DEFAULT_MODEL: &str = "claude-sonnet-5-5";
/// The Claude API.
pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
/// The API version sent with every request.
const API_VERSION: &str = "2026-10-21";
/// Beta that enables `fallbacks: "default"`.
const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
/// Models that accept `fallbacks: "default"` on the Claude API.
const FALLBACK_MODELS: &[&str] = &["claude-sonnet-5-5", "claude-opus-5-5", "claude-opus-5", "claude-fable-5-1"];
/// The most of an error response body that is read.
const MAX_ERROR_BODY: usize = 64 * 1024;

/// Settings for [`AnthropicBrain`]. The harness fills them from its config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnthropicConfig {
    /// The default model; a context's `model` overrides it.
    pub model: String,
    /// Output token cap per step, unless the context sets one.
    pub max_output_tokens: u32,
    /// API base URL, without a trailing slash.
    pub base_url: String,
    /// The model's context window in tokens; trimming keeps requests well inside it.
    pub context_window: u64,
    /// Ask the API to cache the request prefix (system, tools, history).
    pub prompt_caching: bool,
    /// Opt into server-side refusal fallbacks (`fallbacks: "default"`) on models that support
    /// them. Claude API only: turn it off for other endpoints.
    pub refusal_fallback: bool,
}

impl Default for AnthropicConfig {
    fn default() -> Self {
        Self {
            model: DEFAULT_MODEL.to_string(),
            max_output_tokens: 64_000,
            base_url: DEFAULT_BASE_URL.to_string(),
            context_window: 1_000_000,
            prompt_caching: true,
            refusal_fallback: true,
        }
    }
}

/// Claude via the Messages API. Fetches the account's key from [`Credentials`] for every
/// request, so a rotated key is picked up at once and the key never lives longer than a call.
pub struct AnthropicBrain {
    credentials: Arc<dyn Credentials>,
    config: AnthropicConfig,
    transport: Arc<dyn Transport>,
}

impl fmt::Debug for AnthropicBrain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AnthropicBrain").field("config", &self.config).finish_non_exhaustive()
    }
}

impl AnthropicBrain {
    /// A brain that talks to the network.
    pub fn new(credentials: Arc<dyn Credentials>, config: AnthropicConfig) -> Result<Self, BrainError> {
        let transport = Http::new().map_err(BrainError::Other)?;
        Ok(Self::with_transport(credentials, config, Arc::new(transport)))
    }

    fn with_transport(
        credentials: Arc<dyn Credentials>,
        config: AnthropicConfig,
        transport: Arc<dyn Transport>,
    ) -> Self {
        let mut config = config;
        config.base_url = config.base_url.trim_end_matches('/').to_string();
        Self { credentials, config, transport }
    }

    /// A cheap live check that `account`'s key works and can use the configured model: reads the
    /// model's entry from the Models API (no tokens are spent).
    pub async fn verify(&self, account: &AccountRef) -> Result<(), BrainError> {
        let key = self.key(account).await?;
        self.verify_key(&key).await
    }

    /// Like [`AnthropicBrain::verify`], for a key that isn't stored yet (first-run setup checks a
    /// pasted key before saving it).
    pub async fn verify_key(&self, key: &Secret) -> Result<(), BrainError> {
        let url = format!("{}/v1/models/{}", self.config.base_url, self.config.model);
        let headers = self.headers(key, false)?;
        let response = self
            .transport
            .send(Request { method: Method::Get, url, headers, body: None })
            .await
            .map_err(|err| BrainError::Network(errors::scrub(&err, key.expose())))?;
        if response.status == 200 {
            return Ok(());
        }
        Err(error_response(response, key.expose()).await)
    }

    async fn key(&self, account: &AccountRef) -> Result<Secret, BrainError> {
        if account.provider != ProviderId::new(request::PROVIDER) {
            return Err(BrainError::Auth(format!("`{account}` is not an Anthropic account")));
        }
        match self.credentials.get(account).await {
            Ok(Credential::ApiKey(key)) => Ok(key),
            Ok(other) => Err(BrainError::Auth(format!(
                "`{account}` uses {:?} sign-in, which the Anthropic brain doesn't support yet",
                other.method()
            ))),
            Err(
                err @ (CredentialsError::NotFound(_) | CredentialsError::Expired(_) | CredentialsError::Invalid(_)),
            ) => Err(BrainError::Auth(err.to_string())),
            Err(err) => Err(BrainError::Other(err.to_string())),
        }
    }

    fn headers(&self, key: &Secret, fallback: bool) -> Result<HeaderMap, BrainError> {
        let mut headers = HeaderMap::new();
        let set = |headers: &mut HeaderMap, name, value: &str, sensitive| {
            header(headers, name, value, sensitive).map_err(BrainError::Auth)
        };
        set(&mut headers, "x-api-key", key.expose(), true)?;
        set(&mut headers, "anthropic-version", API_VERSION, false)?;
        set(&mut headers, "content-type", "application/json", false)?;
        if fallback {
            set(&mut headers, "anthropic-beta", FALLBACK_BETA, false)?;
        }
        Ok(headers)
    }

    /// The request body for a context.
    fn body(&self, context: &Context, model: &str, fallback: bool) -> Result<(Value, tools::ToolMap), BrainError> {
        let (tool_definitions, map) = tools::build(context);
        let system = request::system(&context.instructions, &context.primer);
        let max_tokens = context.limits.max_output_tokens.unwrap_or(self.config.max_output_tokens);
        let tools_value = Value::Array(tool_definitions);
        let fixed = trim::estimate(&json!(system)) + trim::estimate(&tools_value);
        // Room for the answer, plus a tenth of the window as margin for the estimate.
        let budget = self
            .config
            .context_window
            .saturating_sub(u64::from(max_tokens))
            .saturating_sub(self.config.context_window / 10)
            .saturating_sub(fixed);
        let start = trim::keep_from(&context.messages, budget)?;
        let trimmed = start > 0;
        let messages = request::messages(&context.messages[start..], !trimmed, trimmed);

        let mut body = json!({
            "model": model,
            "max_tokens": max_tokens,
            "stream": true,
            "system": [{ "type": "text", "text": system }],
            "messages": messages,
        });
        if tools_value.as_array().is_some_and(|t| !t.is_empty()) {
            body["tools"] = tools_value;
        }
        if self.config.prompt_caching {
            body["cache_control"] = json!({ "type": "ephemeral" });
        }
        if fallback {
            body["fallbacks"] = json!("default");
        }
        Ok((body, map))
    }

    fn uses_fallback(&self, model: &str) -> bool {
        self.config.refusal_fallback && FALLBACK_MODELS.contains(&model)
    }
}

#[async_trait]
impl Brain for AnthropicBrain {
    fn describe(&self) -> BrainInfo {
        BrainInfo { provider: ProviderId::new(request::PROVIDER), model: self.config.model.clone() }
    }

    async fn think(&self, context: Context) -> Result<BrainStream, BrainError> {
        let model = context.model.clone().unwrap_or_else(|| self.config.model.clone());
        let fallback = self.uses_fallback(&model);
        let key = self.key(&context.account).await?;
        let (body, map) = self.body(&context, &model, fallback)?;
        let body = serde_json::to_vec(&body)
            .map_err(|err| BrainError::Other(format!("could not encode the request: {err}")))?;
        let request = Request {
            method: Method::Post,
            url: format!("{}/v1/messages", self.config.base_url),
            headers: self.headers(&key, fallback)?,
            body: Some(body),
        };
        let response =
            self.transport.send(request).await.map_err(|err| BrainError::Network(errors::scrub(&err, key.expose())))?;
        if response.status != 200 {
            return Err(error_response(response, key.expose()).await);
        }
        Ok(decode(response, Decoder::new(map, model, key.expose().to_string())))
    }
}

/// Reads (a bounded amount of) an error body and maps it.
async fn error_response(response: Response, key: &str) -> BrainError {
    let Response { status, retry_after, mut body } = response;
    let mut bytes = Vec::new();
    while let Some(Ok(chunk)) = body.next().await {
        bytes.extend_from_slice(&chunk);
        if bytes.len() >= MAX_ERROR_BODY {
            break;
        }
    }
    errors::from_status(status, retry_after.as_deref(), &bytes, key)
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
                            state.queue.extend(state.decoder.on_event(event));
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
                None => {
                    state.queue.push_back(Err(BrainError::Network(
                        "the response stream ended before the message finished".into(),
                    )));
                }
            }
        }
    }))
}
