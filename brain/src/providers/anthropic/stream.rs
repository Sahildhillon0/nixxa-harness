//! SSE events → [`BrainEvent`]s.
//!
//! Text deltas are passed on as they arrive, for the live view. Everything else is held until
//! `message_stop`, because only then is it final: a server-side refusal fallback can still
//! replace a partial answer, and a `max_tokens` stop can cut a tool call short. So no tool call
//! from a declined or truncated partial ever reaches the controller.

use std::collections::BTreeMap;

use nixxa_contracts::{BrainError, BrainEvent, Intent, ProviderId, Usage};
use serde_json::{Value, json};

use super::errors;
use super::pricing::{self, Tokens};
use super::request::PROVIDER;
use crate::providers::common::sse::SseEvent;
use crate::providers::common::tools::{self, ToolMap};

/// One content block as it streams in.
#[derive(Debug)]
enum Block {
    Text(String),
    Thinking {
        thinking: String,
        signature: String,
    },
    /// Arrives whole (e.g. `redacted_thinking`); replayed verbatim.
    Opaque(Value),
    ToolUse {
        id: String,
        name: String,
        json: String,
    },
    /// The boundary a server-side fallback inserts where one model declined and the next took
    /// over.
    Fallback,
    /// Anything else; not replayed.
    Unknown,
}

/// Decodes one response stream.
pub(crate) struct Decoder {
    tools: ToolMap,
    /// The model asked for; `message_start` names the one that answered.
    model: String,
    key: String,
    blocks: BTreeMap<u64, Block>,
    stop_reason: Option<String>,
    refusal_category: Option<String>,
    tokens: Tokens,
    /// A fallback or other multi-attempt response: the price can't be read off one model.
    several_attempts: bool,
    started: bool,
    finished: bool,
}

/// What one SSE event produced.
pub(crate) type Items = Vec<Result<BrainEvent, BrainError>>;

impl Decoder {
    pub(crate) fn new(tools: ToolMap, model: String, key: String) -> Self {
        Self {
            tools,
            model,
            key,
            blocks: BTreeMap::new(),
            stop_reason: None,
            refusal_category: None,
            tokens: Tokens::default(),
            several_attempts: false,
            started: false,
            finished: false,
        }
    }

    /// Whether `message_stop` arrived.
    pub(crate) fn finished(&self) -> bool {
        self.finished
    }

    /// Handles one event. An `Err` item ends the stream.
    pub(crate) fn on_event(&mut self, event: SseEvent) -> Items {
        let data: Value = match serde_json::from_str(&event.data) {
            Ok(data) => data,
            Err(_) => return vec![Err(malformed("an event is not valid JSON"))],
        };
        let kind = event.event.as_deref().or_else(|| data["type"].as_str()).unwrap_or_default().to_string();
        match self.handle(&kind, &data) {
            Ok(items) => items,
            Err(err) => vec![Err(err)],
        }
    }

    fn handle(&mut self, kind: &str, data: &Value) -> Result<Items, BrainError> {
        match kind {
            "message_start" => {
                self.started = true;
                let message = &data["message"];
                if let Some(model) = message["model"].as_str() {
                    self.model = model.to_string();
                }
                self.read_usage(&message["usage"]);
            }
            "content_block_start" => {
                let index = index(data)?;
                let block = &data["content_block"];
                let parsed = match block["type"].as_str() {
                    Some("text") => Block::Text(block["text"].as_str().unwrap_or_default().to_string()),
                    Some("thinking") => Block::Thinking {
                        thinking: block["thinking"].as_str().unwrap_or_default().to_string(),
                        signature: block["signature"].as_str().unwrap_or_default().to_string(),
                    },
                    Some("redacted_thinking") => Block::Opaque(block.clone()),
                    Some("tool_use") => {
                        Block::ToolUse { id: string(block, "id")?, name: string(block, "name")?, json: String::new() }
                    }
                    Some("fallback") => Block::Fallback,
                    _ => Block::Unknown,
                };
                self.blocks.insert(index, parsed);
            }
            "content_block_delta" => {
                let index = index(data)?;
                let delta = &data["delta"];
                let block =
                    self.blocks.get_mut(&index).ok_or_else(|| malformed("a delta for a block that never started"))?;
                match (block, delta["type"].as_str()) {
                    (Block::Text(text), Some("text_delta")) => {
                        let piece = delta["text"].as_str().unwrap_or_default().to_string();
                        text.push_str(&piece);
                        if !piece.is_empty() {
                            return Ok(vec![Ok(BrainEvent::TextDelta { text: piece })]);
                        }
                    }
                    (Block::Thinking { thinking, .. }, Some("thinking_delta")) => {
                        thinking.push_str(delta["thinking"].as_str().unwrap_or_default());
                    }
                    (Block::Thinking { signature, .. }, Some("signature_delta")) => {
                        signature.push_str(delta["signature"].as_str().unwrap_or_default());
                    }
                    (Block::ToolUse { json, .. }, Some("input_json_delta")) => {
                        json.push_str(delta["partial_json"].as_str().unwrap_or_default());
                    }
                    // Citations and future delta types don't change what we keep.
                    _ => {}
                }
            }
            "message_delta" => {
                if let Some(reason) = data["delta"]["stop_reason"].as_str() {
                    self.stop_reason = Some(reason.to_string());
                }
                if let Some(category) = data["delta"]["stop_details"]["category"].as_str() {
                    self.refusal_category = Some(category.to_string());
                }
                self.read_usage(&data["usage"]);
            }
            "message_stop" => {
                if !self.started {
                    return Err(malformed("the message ended before it started"));
                }
                self.finished = true;
                return Ok(self.finish());
            }
            "error" => return Err(errors::from_stream_error(data, &self.key)),
            // `content_block_stop`, `ping` and event types added later need nothing.
            _ => {}
        }
        Ok(Vec::new())
    }

    /// Usage fields are cumulative: the latest value of each wins.
    fn read_usage(&mut self, usage: &Value) {
        let field = |name: &str| usage[name].as_u64();
        if let Some(n) = field("input_tokens") {
            self.tokens.input = n;
        }
        if let Some(n) = field("output_tokens") {
            self.tokens.output = n;
        }
        if let Some(n) = field("cache_read_input_tokens") {
            self.tokens.cache_read = n;
        }
        let split = &usage["cache_creation"];
        match (split["ephemeral_5m_input_tokens"].as_u64(), split["ephemeral_1h_input_tokens"].as_u64()) {
            (Some(m5), Some(h1)) => {
                self.tokens.cache_write_5m = m5;
                self.tokens.cache_write_1h = h1;
            }
            _ => {
                if let Some(n) = field("cache_creation_input_tokens") {
                    self.tokens.cache_write_5m = n;
                }
            }
        }
        if usage["iterations"].as_array().is_some_and(|iterations| iterations.len() > 1) {
            self.several_attempts = true;
        }
    }

    fn usage(&self) -> BrainEvent {
        let cost_micro_usd =
            if self.several_attempts { None } else { pricing::cost_micro_usd(&self.model, &self.tokens) };
        BrainEvent::Usage {
            usage: Usage { input_tokens: self.tokens.total_input(), output_tokens: self.tokens.output, cost_micro_usd },
        }
    }

    /// The final batch, in content order.
    fn finish(&mut self) -> Items {
        let stop = self.stop_reason.clone().unwrap_or_default();
        if stop == "refusal" {
            let category = self.refusal_category.as_deref().map(|c| format!(" (category: {c})")).unwrap_or_default();
            return vec![
                Ok(self.usage()),
                Err(BrainError::Provider(format!("the model declined this request{category}"))),
            ];
        }
        // Before the last fallback boundary only text carries over; the declined model's
        // thinking and tool calls are dropped (they were never final).
        let boundary = self.blocks.iter().filter(|(_, b)| matches!(b, Block::Fallback)).map(|(i, _)| *i).max();
        let truncated = stop == "max_tokens";
        let mut items: Items = Vec::new();
        let mut acted = false;
        for (index, block) in std::mem::take(&mut self.blocks) {
            let declined = boundary.is_some_and(|b| index < b);
            match block {
                Block::Text(text) if !text.is_empty() => items.push(Ok(intent(Intent::Say { text }))),
                Block::Thinking { thinking, signature } if !declined => items.push(Ok(BrainEvent::Reasoning {
                    provider: ProviderId::new(PROVIDER),
                    data: json!({ "type": "thinking", "thinking": thinking, "signature": signature }),
                })),
                Block::Opaque(data) if !declined => {
                    items.push(Ok(BrainEvent::Reasoning { provider: ProviderId::new(PROVIDER), data }));
                }
                // A tool call cut off by the output limit is never run.
                Block::ToolUse { id, name, json } if !declined && !truncated => {
                    let input = if json.trim().is_empty() { Ok(json!({})) } else { serde_json::from_str(&json) };
                    let Ok(input) = input else {
                        return vec![Ok(self.usage()), Err(malformed("a tool call's input is not valid JSON"))];
                    };
                    match tools::to_action(&self.tools, &name, input) {
                        Ok(action) => {
                            acted = true;
                            let request = nixxa_contracts::ActionRequest {
                                id: nixxa_contracts::ActionId::new(id),
                                action,
                                reason: None,
                            };
                            items.push(Ok(intent(Intent::Act { request })));
                        }
                        Err(err) => return vec![Ok(self.usage()), Err(err)],
                    }
                }
                _ => {}
            }
        }
        let done = match stop.as_str() {
            "tool_use" if acted => None,
            "tool_use" | "end_turn" | "stop_sequence" => Some("done".to_string()),
            "max_tokens" => Some("stopped: output limit".to_string()),
            "" => Some("stopped".to_string()),
            other => Some(format!("stopped: {other}")),
        };
        if let Some(summary) = done {
            items.push(Ok(intent(Intent::Done { summary })));
        }
        items.push(Ok(self.usage()));
        items
    }
}

fn intent(intent: Intent) -> BrainEvent {
    BrainEvent::Intent { intent }
}

fn index(data: &Value) -> Result<u64, BrainError> {
    data["index"].as_u64().ok_or_else(|| malformed("a content block has no index"))
}

fn string(block: &Value, field: &str) -> Result<String, BrainError> {
    block[field]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| malformed("a tool call has no id or name"))
}

fn malformed(why: &str) -> BrainError {
    BrainError::Provider(format!("malformed response stream: {why}"))
}
