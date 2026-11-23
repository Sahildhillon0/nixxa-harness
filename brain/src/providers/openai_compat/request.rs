//! [`Context`](nixxa_contracts::Context) → Chat Completions `messages` and `tools`.
//!
//! * the system prompt is the first message;
//! * consecutive user text merges into one user message (strict chat templates of local models
//!   require alternating roles);
//! * the agent's text and tool calls of one step become one assistant message;
//! * every observation becomes a `tool` message right after the assistant message that called
//!   it (a missing one is filled in, an orphan dropped);
//! * Nixxa's notices are bracketed user text; reasoning from other providers is skipped.

use std::collections::HashSet;

use nixxa_contracts::{Content, Message, Role};
use serde_json::{Value, json};

use crate::providers::common::history::{TRIMMED_NOTE, observation_text};
use crate::providers::common::tools::tool_use_for;

/// A tool definition `{name, description, input_schema}` in the OpenAI function format.
pub(crate) fn function_tool(definition: &Value) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": definition["name"],
            "description": definition["description"],
            "parameters": definition["input_schema"],
        }
    })
}

enum Turn {
    User(Vec<String>),
    Assistant { text: Vec<String>, calls: Vec<Value> },
    Tool { id: String, content: String },
}

/// The `messages` array: the system prompt, then the conversation.
pub(crate) fn messages(system: &str, conversation: &[Message], trimmed: bool) -> Vec<Value> {
    let mut turns: Vec<Turn> = Vec::new();
    if trimmed {
        turns.push(Turn::User(vec![TRIMMED_NOTE.to_string()]));
    }
    for message in conversation {
        match (&message.role, &message.content) {
            (_, Content::Text { text }) if text.is_empty() => {}
            (Role::Agent, Content::Text { text }) => assistant(&mut turns).0.push(text.clone()),
            (Role::System, Content::Text { text }) => user(&mut turns, format!("[Note from Nixxa: {text}]")),
            (_, Content::Text { text }) => user(&mut turns, text.clone()),
            (_, Content::Action { request }) => {
                let (name, input) = tool_use_for(&request.action);
                assistant(&mut turns).1.push(json!({
                    "id": request.id.as_str(),
                    "type": "function",
                    "function": { "name": name, "arguments": input.to_string() }
                }));
            }
            (_, Content::Observation { observation }) => {
                let (content, is_error) = observation_text(observation);
                let content = if is_error { format!("Error: {content}") } else { content };
                turns.push(Turn::Tool { id: observation.action_id.as_str().to_string(), content });
            }
            // Never part of the model's message (D-029).
            (_, Content::Reasoning { .. } | Content::Summary { .. }) => {}
        }
    }
    let mut out = vec![json!({ "role": "system", "content": system })];
    out.extend(render(turns));
    if out.last().is_none_or(|m| m["role"] == "assistant") || out.len() == 1 {
        out.push(json!({ "role": "user", "content": "[Continue.]" }));
    }
    out
}

fn user(turns: &mut Vec<Turn>, text: String) {
    match turns.last_mut() {
        Some(Turn::User(parts)) => parts.push(text),
        _ => turns.push(Turn::User(vec![text])),
    }
}

fn assistant(turns: &mut Vec<Turn>) -> (&mut Vec<String>, &mut Vec<Value>) {
    if !matches!(turns.last(), Some(Turn::Assistant { .. })) {
        turns.push(Turn::Assistant { text: Vec::new(), calls: Vec::new() });
    }
    match turns.last_mut() {
        Some(Turn::Assistant { text, calls }) => (text, calls),
        _ => unreachable!("an assistant turn was just pushed"),
    }
}

/// Turns into messages, pairing each assistant message's calls with tool results.
fn render(turns: Vec<Turn>) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    // Calls of the latest assistant message that still need a result.
    let mut open: Vec<String> = Vec::new();
    let mut answerable: HashSet<String> = HashSet::new();
    let close_open = |out: &mut Vec<Value>, open: &mut Vec<String>| {
        for id in open.drain(..) {
            out.push(json!({ "role": "tool", "tool_call_id": id, "content": "Error: no result was recorded for this call." }));
        }
    };
    for turn in turns {
        match turn {
            Turn::Tool { id, content } => {
                if answerable.contains(&id) && open.contains(&id) {
                    open.retain(|o| o != &id);
                    out.push(json!({ "role": "tool", "tool_call_id": id, "content": content }));
                }
            }
            Turn::User(parts) => {
                close_open(&mut out, &mut open);
                answerable.clear();
                out.push(json!({ "role": "user", "content": parts.join("\n\n") }));
            }
            Turn::Assistant { text, calls } => {
                close_open(&mut out, &mut open);
                let ids: Vec<String> = calls.iter().filter_map(|c| c["id"].as_str().map(str::to_string)).collect();
                let mut message = json!({ "role": "assistant", "content": text.join("\n\n") });
                if !calls.is_empty() {
                    if text.is_empty() {
                        message["content"] = Value::Null;
                    }
                    message["tool_calls"] = Value::Array(calls);
                }
                out.push(message);
                answerable = ids.iter().cloned().collect();
                open = ids;
            }
        }
    }
    close_open(&mut out, &mut open);
    out
}

#[cfg(test)]
mod tests {
    use nixxa_contracts::{
        Action, ActionId, ActionRequest, Observation, Outcome, ProviderId, SandboxAction, Timestamp,
    };

    use super::*;

    const T: Timestamp = Timestamp(0);

    fn user_msg(text: &str) -> Message {
        Message::text(Role::User, text, T)
    }

    fn agent(text: &str) -> Message {
        Message::text(Role::Agent, text, T)
    }

    fn action(id: &str, command: &str) -> Message {
        let request = ActionRequest {
            id: ActionId::new(id),
            action: Action::Sandbox {
                action: SandboxAction::Exec { command: command.into(), cwd: None, timeout_ms: None, background: false },
            },
            reason: None,
        };
        Message { role: Role::Agent, content: Content::Action { request }, at: T }
    }

    #[test]
    fn a_summary_is_never_sent() {
        let summary = Message { role: Role::Agent, content: Content::Summary { text: "did it".into() }, at: T };
        let with = messages("sys", &[user_msg("hi"), agent("hello"), summary], false);
        assert_eq!(with, messages("sys", &[user_msg("hi"), agent("hello")], false));
        assert!(!serde_json::to_string(&with).unwrap().contains("did it"));
    }

    fn observation(id: &str, outcome: Outcome) -> Message {
        let observation = Observation::new(ActionId::new(id), outcome);
        Message { role: Role::Observation, content: Content::Observation { observation }, at: T }
    }

    fn ok(stdout: &str) -> Outcome {
        Outcome::Exec { exit_code: Some(0), stdout: stdout.into(), stderr: String::new() }
    }

    fn exec_call(id: &str, command: &str) -> Value {
        json!({ "id": id, "type": "function", "function": { "name": "exec", "arguments": json!({ "command": command }).to_string() } })
    }

    #[test]
    fn a_parallel_step_is_one_assistant_message_then_one_tool_message_per_call() {
        let conversation = vec![
            user_msg("go"),
            agent("Running both."),
            action("c1", "ls"),
            action("c2", "pwd"),
            observation("c1", ok("a.rs")),
            observation("c2", Outcome::Denied { reason: "no".into() }),
            agent("done here"),
        ];
        assert_eq!(
            messages("SYS", &conversation, false),
            vec![
                json!({ "role": "system", "content": "SYS" }),
                json!({ "role": "user", "content": "go" }),
                json!({ "role": "assistant", "content": "Running both.", "tool_calls": [exec_call("c1", "ls"), exec_call("c2", "pwd")] }),
                json!({ "role": "tool", "tool_call_id": "c1", "content": "exit code: 0\n--- stdout ---\na.rs\n--- stderr ---\n" }),
                json!({ "role": "tool", "tool_call_id": "c2", "content": "Error: Denied: no" }),
                json!({ "role": "assistant", "content": "done here" }),
                json!({ "role": "user", "content": "[Continue.]" }),
            ]
        );
    }

    #[test]
    fn calls_without_text_have_null_content_and_user_text_merges() {
        let conversation = vec![
            user_msg("a"),
            Message::text(Role::System, "interrupted by the user", T),
            user_msg("b"),
            action("c1", "ls"),
            observation("c1", ok("x")),
        ];
        let rendered = messages("SYS", &conversation, false);
        assert_eq!(
            rendered[1],
            json!({ "role": "user", "content": "a\n\n[Note from Nixxa: interrupted by the user]\n\nb" })
        );
        assert_eq!(rendered[2]["content"], Value::Null);
        assert_eq!(rendered.last().unwrap()["role"], "tool", "a tool result may end the request");
    }

    #[test]
    fn missing_results_are_filled_in_and_orphans_dropped() {
        let conversation =
            vec![user_msg("go"), action("c1", "ls"), user_msg("never mind"), observation("ghost", ok("x"))];
        let rendered = messages("SYS", &conversation, false);
        assert_eq!(rendered[3]["role"], "tool");
        assert_eq!(rendered[3]["tool_call_id"], "c1");
        assert_eq!(rendered[4], json!({ "role": "user", "content": "never mind" }));
        assert_eq!(rendered.len(), 5, "the orphan result is dropped");
    }

    #[test]
    fn reasoning_from_other_providers_is_skipped_and_trimming_is_noted() {
        let reasoning = Message {
            role: Role::Agent,
            content: Content::Reasoning { provider: ProviderId::new("anthropic"), data: json!({ "type": "thinking" }) },
            at: T,
        };
        let rendered = messages("SYS", &[user_msg("hi"), reasoning, agent("hello"), user_msg("again")], true);
        assert_eq!(rendered[1], json!({ "role": "user", "content": format!("{TRIMMED_NOTE}\n\nhi") }));
        assert_eq!(rendered[2], json!({ "role": "assistant", "content": "hello" }));
    }

    #[test]
    fn tools_use_the_function_format() {
        let definition = json!({ "name": "exec", "description": "Run.", "input_schema": { "type": "object" } });
        assert_eq!(
            function_tool(&definition),
            json!({ "type": "function", "function": { "name": "exec", "description": "Run.", "parameters": { "type": "object" } } })
        );
    }
}
