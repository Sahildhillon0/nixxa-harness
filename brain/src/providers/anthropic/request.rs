//! [`Context`](nixxa_contracts::Context) → Messages API request parts.
//!
//! The rendering is a pure function of the stored conversation, and the conversation is
//! append-only, so every request repeats the previous one's prefix byte for byte. Signed
//! thinking blocks depend on that.

use std::collections::HashSet;

use nixxa_contracts::{Content, Message, Observation, Role};
use serde_json::{Value, json};

use crate::providers::common::history::{TRIMMED_NOTE, observation_text};
use crate::providers::common::tools::tool_use_for;

/// The provider id this brain stamps on its reasoning.
pub(crate) const PROVIDER: &str = "anthropic";

/// The `messages` array for a conversation. `keep_reasoning` is false when older turns were
/// trimmed: the prefix changed, so no earlier thinking block would still be valid.
pub(crate) fn messages(conversation: &[Message], keep_reasoning: bool, trimmed: bool) -> Vec<Value> {
    let mut turns: Vec<(&'static str, Vec<Value>)> = Vec::new();
    if trimmed {
        turns.push(("user", vec![text_block(TRIMMED_NOTE)]));
    }
    for message in conversation {
        let Some((role, block)) = block_for(message, keep_reasoning) else { continue };
        match turns.last_mut() {
            Some((last, blocks)) if *last == role => blocks.push(block),
            _ => turns.push((role, vec![block])),
        }
    }
    pair_tool_results(&mut turns);
    if turns.first().is_some_and(|(role, _)| *role == "assistant") {
        turns.insert(0, ("user", vec![text_block("[The conversation continues.]")]));
    }
    // A request may not end on an assistant turn (no prefill on current models).
    if turns.last().is_none_or(|(role, _)| *role == "assistant") {
        turns.push(("user", vec![text_block("[Continue.]")]));
    }
    turns.into_iter().map(|(role, content)| json!({ "role": role, "content": content })).collect()
}

fn block_for(message: &Message, keep_reasoning: bool) -> Option<(&'static str, Value)> {
    Some(match (&message.role, &message.content) {
        (_, Content::Text { text }) if text.is_empty() => return None,
        (Role::User, Content::Text { text }) => ("user", text_block(text)),
        (Role::Agent, Content::Text { text }) => ("assistant", text_block(text)),
        (Role::System, Content::Text { text }) => ("user", text_block(&format!("[Note from Nixxa: {text}]"))),
        (Role::Observation, Content::Text { text }) => ("user", text_block(text)),
        (_, Content::Action { request }) => {
            let (name, input) = tool_use_for(&request.action);
            ("assistant", json!({ "type": "tool_use", "id": request.id.as_str(), "name": name, "input": input }))
        }
        (_, Content::Observation { observation }) => ("user", tool_result(observation)),
        (_, Content::Reasoning { provider, data }) if keep_reasoning && provider.as_str() == PROVIDER => {
            ("assistant", data.clone())
        }
        (_, Content::Reasoning { .. }) => return None,
        // Never part of the model's message (D-029); the controller doesn't send it, and if it
        // ever did, sending it would edit the history.
        (_, Content::Summary { .. }) => return None,
    })
}

fn text_block(text: &str) -> Value {
    json!({ "type": "text", "text": text })
}

fn tool_result(observation: &Observation) -> Value {
    let (content, is_error) = observation_text(observation);
    let mut block = json!({ "type": "tool_result", "tool_use_id": observation.action_id.as_str(), "content": content });
    if is_error {
        block["is_error"] = Value::Bool(true);
    }
    block
}

/// Every `tool_use` must be answered by a `tool_result` at the start of the next user turn, and a
/// `tool_result` must answer a `tool_use` in the turn before it. The controller always records
/// both, so this only repairs odd histories (e.g. one written by another brain).
fn pair_tool_results(turns: &mut Vec<(&'static str, Vec<Value>)>) {
    let mut i = 0;
    while i < turns.len() {
        let asked: Vec<String> = if turns[i].0 == "assistant" {
            turns[i]
                .1
                .iter()
                .filter(|b| b["type"] == "tool_use")
                .filter_map(|b| b["id"].as_str().map(str::to_string))
                .collect()
        } else {
            Vec::new()
        };
        let next = i + 1;
        if turns[i].0 == "assistant" && !asked.is_empty() && turns.get(next).is_none_or(|(role, _)| *role != "user") {
            turns.insert(next, ("user", Vec::new()));
        }
        if let Some((_, blocks)) = turns.get_mut(next).filter(|(role, _)| *role == "user") {
            let asked_set: HashSet<&str> = asked.iter().map(String::as_str).collect();
            let (mut results, rest): (Vec<Value>, Vec<Value>) =
                std::mem::take(blocks).into_iter().partition(|b| b["type"] == "tool_result");
            results.retain(|r| r["tool_use_id"].as_str().is_some_and(|id| asked_set.contains(id)));
            for id in &asked {
                if !results.iter().any(|r| r["tool_use_id"] == id.as_str()) {
                    results.push(json!({ "type": "tool_result", "tool_use_id": id, "content": "No result was recorded for this call.", "is_error": true }));
                }
            }
            results.extend(rest);
            *blocks = results;
        }
        i += 1;
    }
    // Results with no call before them are dropped above; a user turn left empty goes too.
    if turns.first().is_some_and(|(role, _)| *role == "user") {
        if let Some((_, blocks)) = turns.first_mut() {
            blocks.retain(|b| b["type"] != "tool_result");
        }
    }
    turns.retain(|(_, blocks)| !blocks.is_empty());
}

#[cfg(test)]
mod tests {
    use nixxa_contracts::{Action, ActionId, ActionRequest, Outcome, ProviderId, SandboxAction, Timestamp};

    use super::*;

    const T: Timestamp = Timestamp(0);

    fn user(text: &str) -> Message {
        Message::text(Role::User, text, T)
    }

    fn agent(text: &str) -> Message {
        Message::text(Role::Agent, text, T)
    }

    fn action(id: &str) -> Message {
        let request = ActionRequest {
            id: ActionId::new(id),
            action: Action::Sandbox {
                action: SandboxAction::Exec { command: "ls".into(), cwd: None, timeout_ms: None, background: false },
            },
            reason: None,
        };
        Message { role: Role::Agent, content: Content::Action { request }, at: T }
    }

    fn observation(id: &str, outcome: Outcome) -> Message {
        let observation = Observation::new(ActionId::new(id), outcome);
        Message { role: Role::Observation, content: Content::Observation { observation }, at: T }
    }

    fn reasoning(provider: &str, signature: &str) -> Message {
        let data = json!({ "type": "thinking", "thinking": "", "signature": signature });
        Message { role: Role::Agent, content: Content::Reasoning { provider: ProviderId::new(provider), data }, at: T }
    }

    fn exec_ok(stdout: &str) -> Outcome {
        Outcome::Exec { exit_code: Some(0), stdout: stdout.into(), stderr: String::new() }
    }

    #[test]
    fn a_summary_is_never_sent() {
        let summary = Message { role: Role::Agent, content: Content::Summary { text: "did it".into() }, at: T };
        let rendered = messages(&[user("hi"), agent("hello"), summary], true, false);
        assert_eq!(
            rendered,
            vec![
                json!({ "role": "user", "content": [{ "type": "text", "text": "hi" }] }),
                json!({ "role": "assistant", "content": [{ "type": "text", "text": "hello" }] }),
                json!({ "role": "user", "content": [{ "type": "text", "text": "[Continue.]" }] }),
            ]
        );
    }

    #[test]
    fn a_tool_loop_maps_to_alternating_turns() {
        let conversation = vec![
            user("list it"),
            reasoning(PROVIDER, "sig-1"),
            agent("Looking."),
            action("toolu_1"),
            observation("toolu_1", exec_ok("a.rs")),
            reasoning(PROVIDER, "sig-2"),
            agent("There is a.rs."),
            agent("done"),
        ];
        let rendered = messages(&conversation, true, false);
        assert_eq!(
            rendered,
            vec![
                json!({ "role": "user", "content": [{ "type": "text", "text": "list it" }] }),
                json!({ "role": "assistant", "content": [
                    { "type": "thinking", "thinking": "", "signature": "sig-1" },
                    { "type": "text", "text": "Looking." },
                    { "type": "tool_use", "id": "toolu_1", "name": "exec", "input": { "command": "ls" } }
                ]}),
                json!({ "role": "user", "content": [{
                    "type": "tool_result", "tool_use_id": "toolu_1",
                    "content": "exit code: 0\n--- stdout ---\na.rs\n--- stderr ---\n"
                }]}),
                json!({ "role": "assistant", "content": [
                    { "type": "thinking", "thinking": "", "signature": "sig-2" },
                    { "type": "text", "text": "There is a.rs." },
                    { "type": "text", "text": "done" }
                ]}),
                json!({ "role": "user", "content": [{ "type": "text", "text": "[Continue.]" }] }),
            ]
        );
    }

    #[test]
    fn parallel_results_share_one_user_message_and_errors_are_flagged() {
        let conversation = vec![
            user("go"),
            action("a"),
            action("b"),
            observation("a", Outcome::Denied { reason: "no".into() }),
            observation("b", Outcome::Failed { message: "missing".into() }),
            Message::text(Role::System, "interrupted by the user", T),
            user("next"),
        ];
        let rendered = messages(&conversation, true, false);
        assert_eq!(rendered.len(), 3);
        assert_eq!(
            rendered[2]["content"],
            json!([
                { "type": "tool_result", "tool_use_id": "a", "content": "Denied: no", "is_error": true },
                { "type": "tool_result", "tool_use_id": "b", "content": "Failed: missing", "is_error": true },
                { "type": "text", "text": "[Note from Nixxa: interrupted by the user]" },
                { "type": "text", "text": "next" }
            ])
        );
    }

    #[test]
    fn reasoning_is_replayed_only_for_this_provider_and_only_when_kept() {
        let conversation = vec![user("hi"), reasoning("other", "x"), reasoning(PROVIDER, "y"), agent("hello")];
        let kept = messages(&conversation, true, false);
        assert_eq!(kept[1]["content"].as_array().unwrap().len(), 2);
        assert_eq!(kept[1]["content"][0]["signature"], "y");
        let dropped = messages(&conversation, false, true);
        assert_eq!(dropped[0]["content"][0]["text"], TRIMMED_NOTE);
        assert!(!dropped.iter().any(|t| t["content"].as_array().unwrap().iter().any(|b| b["type"] == "thinking")));
    }

    #[test]
    fn a_missing_result_is_repaired_and_orphan_results_dropped() {
        let conversation = vec![user("go"), action("a"), user("never mind")];
        let rendered = messages(&conversation, true, false);
        assert_eq!(rendered[2]["content"][0]["tool_use_id"], "a");
        assert_eq!(rendered[2]["content"][0]["is_error"], true);
        assert_eq!(rendered[2]["content"][1]["text"], "never mind");

        let orphan = vec![observation("ghost", exec_ok("x")), user("hi")];
        assert_eq!(
            messages(&orphan, true, false),
            vec![json!({ "role": "user", "content": [{ "type": "text", "text": "hi" }] })]
        );
    }

    #[test]
    fn rendering_is_stable_as_the_conversation_grows() {
        let mut conversation =
            vec![user("a"), reasoning(PROVIDER, "s1"), action("t1"), observation("t1", exec_ok("x"))];
        let first = messages(&conversation, true, false);
        conversation.extend([reasoning(PROVIDER, "s2"), agent("ok"), agent("done"), user("b")]);
        let second = messages(&conversation, true, false);
        assert_eq!(second[..first.len()], first[..], "earlier turns are repeated exactly");
    }
}
