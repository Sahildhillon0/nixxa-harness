//! Keeping a request inside the context window, without a network call.
//!
//! The estimate is deliberately pessimistic: one token per three bytes of the JSON that is sent
//! (real tokenisers average closer to four). The oldest whole exchanges (a user message and
//! everything up to the next one) go first, so a `tool_use` is never separated from its
//! `tool_result`, and the newest exchange is always kept.

use nixxa_contracts::{BrainError, Content, Message, Role};
use serde_json::Value;

/// Bytes per token in the estimate.
const BYTES_PER_TOKEN: u64 = 3;

/// A conservative token estimate for anything that will be sent as JSON.
pub(crate) fn estimate(value: &Value) -> u64 {
    (value.to_string().len() as u64).div_ceil(BYTES_PER_TOKEN)
}

fn estimate_message(message: &Message) -> u64 {
    serde_json::to_string(&message.content).map_or(0, |json| json.len() as u64).div_ceil(BYTES_PER_TOKEN) + 4
}

/// Where the kept part of the conversation starts (0 = nothing trimmed).
pub(crate) fn keep_from(conversation: &[Message], budget: u64) -> Result<usize, BrainError> {
    // An exchange starts at each message the user typed.
    let mut starts: Vec<usize> = conversation
        .iter()
        .enumerate()
        .filter(|(_, m)| m.role == Role::User && matches!(m.content, Content::Text { .. }))
        .map(|(i, _)| i)
        .collect();
    if starts.first() != Some(&0) {
        starts.insert(0, 0);
    }
    let costs: Vec<u64> = conversation.iter().map(estimate_message).collect();
    let mut total: u64 = costs.iter().sum();
    let mut kept = 0;
    while total > budget && kept + 1 < starts.len() {
        let end = starts[kept + 1];
        total -= costs[starts[kept]..end].iter().sum::<u64>();
        kept += 1;
    }
    if total > budget {
        return Err(BrainError::ContextTooLarge);
    }
    Ok(starts[kept])
}

#[cfg(test)]
mod tests {
    use nixxa_contracts::{ActionId, Observation, Outcome, Timestamp};

    use super::*;

    fn user(text: &str) -> Message {
        Message::text(Role::User, text, Timestamp(0))
    }

    fn agent(text: &str) -> Message {
        Message::text(Role::Agent, text, Timestamp(0))
    }

    fn result(id: &str, size: usize) -> Message {
        let observation = Observation::new(ActionId::new(id), Outcome::Text { content: "x".repeat(size) });
        Message { role: Role::Observation, content: Content::Observation { observation }, at: Timestamp(0) }
    }

    fn total(conversation: &[Message]) -> u64 {
        conversation.iter().map(estimate_message).sum()
    }

    #[test]
    fn nothing_is_trimmed_when_it_fits() {
        let conversation = vec![user("a"), agent("b"), user("c")];
        assert_eq!(keep_from(&conversation, total(&conversation)), Ok(0));
    }

    #[test]
    fn the_oldest_whole_exchanges_go_first() {
        let conversation = vec![
            user("first"),
            agent("x".repeat(3000).as_str()),
            user("second"),
            result("t1", 3000),
            user("third"),
            agent("short"),
        ];
        let newest = total(&conversation[4..]);
        assert_eq!(keep_from(&conversation, newest), Ok(4));
        assert_eq!(keep_from(&conversation, newest + total(&conversation[2..4])), Ok(2));
    }

    #[test]
    fn a_tool_result_is_never_kept_without_its_exchange() {
        let conversation = vec![user("go"), agent("calling"), result("t1", 9000), user("next")];
        let start = keep_from(&conversation, total(&conversation[3..])).unwrap();
        assert_eq!(start, 3, "the call and its result go together");
    }

    #[test]
    fn too_large_even_alone_is_an_error() {
        let conversation = vec![user("hi"), result("t1", 100_000)];
        assert_eq!(keep_from(&conversation, 100), Err(BrainError::ContextTooLarge));
    }

    #[test]
    fn the_estimate_is_pessimistic() {
        let value = serde_json::json!({ "text": "x".repeat(400) });
        assert!(estimate(&value) >= 400 / 3);
    }
}
