//! Chat Completions SSE chunks → [`BrainEvent`]s.
//!
//! Text deltas are passed on live. The text and tool calls are held until the stream ends
//! (`[DONE]`, or the end of the body after a finished choice), then emitted in order: the text as
//! one `Say`, each tool call as an `Act`, then `Done` unless the step called tools, then `Usage`.
//! Some servers (e.g. Ollama) send a whole tool call in one chunk and finish with `stop`, so any
//! completed tool call counts, whatever the finish reason, unless the output limit cut it.

use std::collections::BTreeMap;

use nixxa_contracts::{ActionId, ActionRequest, BrainError, BrainEvent, Intent, Usage};
use serde_json::{Value, json};

use super::errors;
use crate::providers::common::tools::{self, ToolMap};

#[derive(Default)]
struct Call {
    id: String,
    name: String,
    arguments: String,
}

/// Decodes one response stream.
pub(crate) struct Decoder {
    tools: ToolMap,
    key: String,
    local: bool,
    model: String,
    text: String,
    calls: BTreeMap<u64, Call>,
    finish_reason: Option<String>,
    usage: Option<(u64, u64)>,
    finished: bool,
}

/// What one event produced.
pub(crate) type Items = Vec<Result<BrainEvent, BrainError>>;

impl Decoder {
    pub(crate) fn new(tools: ToolMap, key: String, local: bool, model: String) -> Self {
        Self {
            tools,
            key,
            local,
            model,
            text: String::new(),
            calls: BTreeMap::new(),
            finish_reason: None,
            usage: None,
            finished: false,
        }
    }

    pub(crate) fn finished(&self) -> bool {
        self.finished
    }

    /// One SSE `data` payload.
    pub(crate) fn on_event(&mut self, data: &str) -> Items {
        if self.finished {
            return Vec::new();
        }
        if data.trim() == "[DONE]" {
            self.finished = true;
            return self.finish();
        }
        let chunk: Value = match serde_json::from_str(data) {
            Ok(chunk) => chunk,
            Err(_) => return vec![Err(malformed("a chunk is not valid JSON"))],
        };
        if !chunk["error"].is_null() {
            self.finished = true;
            let err = errors::from_stream_error(&chunk["error"], &self.key);
            return vec![Err(errors::explain_no_tools(err, &self.model))];
        }
        if let (Some(prompt), Some(completion)) =
            (chunk["usage"]["prompt_tokens"].as_u64(), chunk["usage"]["completion_tokens"].as_u64())
        {
            self.usage = Some((prompt, completion));
        }
        let mut items = Vec::new();
        let Some(choice) = chunk["choices"].as_array().and_then(|choices| choices.first()) else { return items };
        let delta = &choice["delta"];
        if let Some(piece) = delta["content"].as_str().filter(|p| !p.is_empty()) {
            self.text.push_str(piece);
            items.push(Ok(BrainEvent::TextDelta { text: piece.to_string() }));
        }
        if let Some(pieces) = delta["tool_calls"].as_array() {
            for piece in pieces {
                let Some(index) = piece["index"].as_u64() else {
                    return vec![Err(malformed("a tool call has no index"))];
                };
                let call = self.calls.entry(index).or_default();
                if let Some(id) = piece["id"].as_str().filter(|id| !id.is_empty()) {
                    call.id = id.to_string();
                }
                if let Some(name) = piece["function"]["name"].as_str() {
                    call.name.push_str(name);
                }
                if let Some(arguments) = piece["function"]["arguments"].as_str() {
                    call.arguments.push_str(arguments);
                }
            }
        }
        if let Some(reason) = choice["finish_reason"].as_str() {
            self.finish_reason = Some(reason.to_string());
        }
        items
    }

    /// The body ended. Fine after a finished choice; otherwise the stream was cut.
    pub(crate) fn on_end(&mut self) -> Items {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        if self.finish_reason.is_some() {
            return self.finish();
        }
        vec![Err(BrainError::Network("the response stream ended before the answer finished".into()))]
    }

    fn usage(&self) -> Option<Result<BrainEvent, BrainError>> {
        let (input_tokens, output_tokens) = self.usage?;
        // A local server bills nothing; anything else reports no price, so none is guessed.
        let cost_micro_usd = self.local.then_some(0);
        Some(Ok(BrainEvent::Usage { usage: Usage { input_tokens, output_tokens, cost_micro_usd } }))
    }

    fn finish(&mut self) -> Items {
        let reason = self.finish_reason.clone().unwrap_or_default();
        let mut items: Items = Vec::new();
        if reason == "content_filter" {
            items.extend(self.usage());
            items.push(Err(BrainError::Provider("the server filtered the model's answer".into())));
            return items;
        }
        let text = std::mem::take(&mut self.text);
        if !text.trim().is_empty() {
            items.push(Ok(BrainEvent::Intent { intent: Intent::Say { text } }));
        }
        let truncated = reason == "length";
        let mut acted = false;
        if !truncated {
            for (index, call) in std::mem::take(&mut self.calls) {
                let input = if call.arguments.trim().is_empty() {
                    Ok(json!({}))
                } else {
                    serde_json::from_str(&call.arguments)
                };
                let Ok(input) = input else {
                    items.extend(self.usage());
                    items.push(Err(malformed("a tool call's arguments are not valid JSON")));
                    return items;
                };
                match tools::to_action(&self.tools, &call.name, input) {
                    Ok(action) => {
                        acted = true;
                        // Some servers omit the id; an id unique to this answer is enough.
                        let id = if call.id.is_empty() { format!("call_{index}") } else { call.id };
                        let request = ActionRequest { id: ActionId::new(id), action, reason: None };
                        items.push(Ok(BrainEvent::Intent { intent: Intent::Act { request } }));
                    }
                    Err(err) => {
                        items.extend(self.usage());
                        items.push(Err(err));
                        return items;
                    }
                }
            }
        }
        if !acted {
            let summary = match reason.as_str() {
                "stop" | "tool_calls" | "" => "done".to_string(),
                "length" => "stopped: output limit".to_string(),
                other => format!("stopped: {other}"),
            };
            items.push(Ok(BrainEvent::Intent { intent: Intent::Done { summary } }));
        }
        items.extend(self.usage());
        items
    }
}

fn malformed(why: &str) -> BrainError {
    BrainError::Provider(format!("malformed response stream: {why}"))
}
