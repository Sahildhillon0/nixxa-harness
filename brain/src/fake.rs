//! Fakes for the brain contract.

use async_trait::async_trait;
use serde_json::json;

use nixxa_contracts::{
    Action, ActionId, ActionRequest, Brain, BrainError, BrainEvent, BrainInfo, BrainStream, Context, Intent,
    KnowledgeAction, Outcome, Role, SandboxAction, SourceId, Usage,
};

/// A deterministic, stateless brain fake.
///
/// It decides from the last message in the [`Context`]: a user message triggers a scripted
/// knowledge lookup, the resulting knowledge observation triggers a scripted sandbox exec, the
/// sandbox result closes the turn, and a denial short-circuits with a skip.
pub struct ScriptedBrain;

impl ScriptedBrain {
    /// The scripted intent sequence for one think step, derived only from the context.
    fn next_intents(&self, context: &Context) -> Vec<Intent> {
        let Some(last) = context.messages.last() else {
            return vec![Intent::Say { text: "I am listening.".into() }];
        };
        if last.role == Role::User {
            let text = match &last.content {
                nixxa_contracts::Content::Text { text } => text.clone(),
                _ => "the user".into(),
            };
            return vec![Intent::Act {
                request: ActionRequest {
                    id: ActionId::new(format!("act-{}-{}", context.session, context.messages.len())),
                    action: Action::Knowledge {
                        action: KnowledgeAction::Call {
                            source: SourceId::new("notes"),
                            tool: "search".into(),
                            args: json!({ "query": text }),
                        },
                    },
                    reason: Some("search notes".into()),
                },
            }];
        }
        if last.role == Role::Observation {
            let nixxa_contracts::Content::Observation { observation } = &last.content else {
                return vec![Intent::Done { summary: "end".into() }];
            };
            if let Outcome::Denied { reason } = &observation.outcome {
                return vec![
                    Intent::Say { text: format!("okay, skipping that: {reason}") },
                    Intent::Done { summary: "denied".into() },
                ];
            }
            if let Outcome::Exec { .. } = &observation.outcome {
                return vec![
                    Intent::Say { text: "The command finished.".into() },
                    Intent::Done { summary: "sandbox exec done".into() },
                ];
            }
            // A knowledge observation: say a short line, then propose the sandbox exec.
            return vec![
                Intent::Say { text: "I looked into that.".into() },
                Intent::Act {
                    request: ActionRequest {
                        id: ActionId::new(format!("act-{}-{}", context.session, context.messages.len())),
                        action: Action::Sandbox {
                            action: SandboxAction::Exec {
                                command: "echo hello from the sandbox".into(),
                                cwd: None,
                                timeout_ms: Some(5_000),
                            },
                        },
                        reason: Some("run the command".into()),
                    },
                },
            ];
        }
        vec![Intent::Done { summary: "nothing to do".into() }]
    }
}

#[async_trait]
impl Brain for ScriptedBrain {
    fn describe(&self) -> BrainInfo {
        BrainInfo { provider: "scripted".into(), model: "scripted-1".into() }
    }

    async fn think(&self, context: Context) -> Result<BrainStream, BrainError> {
        let intents = self.next_intents(&context);
        let mut events: Vec<Result<BrainEvent, BrainError>> = Vec::new();
        for intent in intents {
            let intent_for_event = intent.clone();
            let delta = match &intent {
                Intent::Say { text } => text.clone(),
                Intent::Act { request } => format!("I will {}.", request.reason.as_deref().unwrap_or("do that")),
                Intent::Done { summary } => summary.clone(),
            };
            events.push(Ok(BrainEvent::TextDelta { text: delta }));
            events.push(Ok(BrainEvent::Intent { intent: intent_for_event }));
        }
        events.push(Ok(BrainEvent::Usage { usage: Usage::default() }));
        Ok(Box::pin(futures::stream::iter(events)))
    }
}

#[cfg(all(test, feature = "conformance"))]
mod tests {
    use nixxa_contracts::AccountRef;
    use nixxa_contracts::conformance::brain::run;

    use super::ScriptedBrain;

    #[tokio::test]
    async fn scripted_brain_conforms() {
        let brain = ScriptedBrain;
        run(&brain, AccountRef::new("scripted", "default")).await;
    }
}
