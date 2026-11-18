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
            // Branch on the action this observation answers, not on how it failed: a failed
            // action must end the turn instead of sending the agent down the same script again.
            let request = context
                .messages
                .iter()
                .rev()
                .find_map(|m| match &m.content {
                    nixxa_contracts::Content::Action { request } => Some(request.clone()),
                    _ => None,
                })
                .filter(|r| r.id == observation.action_id);
            if let Some(request) = &request {
                if let Outcome::Denied { reason } = &observation.outcome {
                    return vec![
                        Intent::Say { text: format!("okay, skipping that: {reason}") },
                        Intent::Done { summary: "denied".into() },
                    ];
                }
                if let Action::Sandbox { action: SandboxAction::Exec { command, .. } } = &request.action {
                    return Self::after_exec(&observation.outcome, command);
                }
                // The knowledge lookup answered; run the scripted sandbox exec.
                return vec![
                    Intent::Say { text: "I looked into that.".into() },
                    Intent::Act { request: Self::exec_request(context) },
                ];
            }
            // The observation does not answer a known action; close the turn.
            return vec![Intent::Done { summary: "no matching action".into() }];
        }
        vec![Intent::Done { summary: "nothing to do".into() }]
    }

    /// Closes the turn after a scripted sandbox exec, whether it succeeded or failed.
    fn after_exec(outcome: &Outcome, command: &str) -> Vec<Intent> {
        if matches!(outcome, Outcome::Exec { .. }) {
            vec![
                Intent::Say { text: "The command finished.".into() },
                Intent::Done { summary: "sandbox exec done".into() },
            ]
        } else {
            vec![
                Intent::Say { text: format!("The command failed: {command}") },
                Intent::Done { summary: "sandbox exec failed".into() },
            ]
        }
    }

    /// The scripted sandbox exec the fake brain proposes after a knowledge lookup.
    fn exec_request(context: &Context) -> ActionRequest {
        ActionRequest {
            id: ActionId::new(format!("act-{}-{}", context.session, context.messages.len())),
            action: Action::Sandbox {
                action: SandboxAction::Exec {
                    command: "echo hello from the sandbox".into(),
                    cwd: None,
                    timeout_ms: Some(5_000),
                },
            },
            reason: Some("run the command".into()),
        }
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
            if let Intent::Say { text } = &intent {
                events.push(Ok(BrainEvent::TextDelta { text: text.clone() }));
            }
            events.push(Ok(BrainEvent::Intent { intent: intent_for_event }));
        }
        events.push(Ok(BrainEvent::Usage { usage: Usage::default() }));
        Ok(Box::pin(futures::stream::iter(events)))
    }
}

#[cfg(test)]
mod tests {
    use futures::StreamExt;
    use nixxa_contracts::AccountRef;
    use nixxa_contracts::conformance::brain::run;
    use nixxa_contracts::{
        Action, ActionId, ActionRequest, Brain, BrainEvent, Content, Context, Intent, KnowledgeAction, Message,
        Observation, Outcome, Role, SandboxAction, SessionId, SourceId, Timestamp,
    };

    use super::ScriptedBrain;

    const T: Timestamp = Timestamp(1_000);

    fn context(messages: Vec<Message>) -> Context {
        Context {
            session: SessionId::new("s1"),
            account: AccountRef::new("scripted", "default"),
            model: None,
            instructions: "Reply with one short sentence.".into(),
            primer: Default::default(),
            messages,
            sandbox_actions: Vec::new(),
            knowledge_tools: Vec::new(),
            limits: Default::default(),
        }
    }

    fn user(text: &str) -> Message {
        Message::text(Role::User, text, T)
    }

    fn agent_action(request: ActionRequest) -> Message {
        Message { role: Role::Agent, content: Content::Action { request }, at: T }
    }

    fn observation(action_id: ActionId, outcome: Outcome) -> Message {
        Message {
            role: Role::Observation,
            content: Content::Observation { observation: Observation::new(action_id, outcome) },
            at: T,
        }
    }

    async fn plan(context: Context) -> Vec<Intent> {
        let mut stream = ScriptedBrain.think(context).await.expect("think");
        let mut intents = Vec::new();
        while let Some(event) = stream.next().await {
            if let BrainEvent::Intent { intent } = event.expect("brain event") {
                intents.push(intent);
            }
        }
        intents
    }

    #[tokio::test]
    async fn scripted_brain_conforms() {
        let brain = ScriptedBrain;
        run(&brain, AccountRef::new("scripted", "default")).await;
    }

    #[tokio::test]
    async fn empty_context_lets_it_speak() {
        let intents = plan(context(Vec::new())).await;
        assert_eq!(intents, vec![Intent::Say { text: "I am listening.".into() }]);
    }

    #[tokio::test]
    async fn user_message_plans_the_knowledge_lookup() {
        let intents = plan(context(vec![user("list the project")])).await;
        assert_eq!(
            intents,
            vec![Intent::Act {
                request: ActionRequest {
                    id: ActionId::new("act-s1-1"),
                    action: Action::Knowledge {
                        action: KnowledgeAction::Call {
                            source: SourceId::new("notes"),
                            tool: "search".into(),
                            args: serde_json::json!({ "query": "list the project" }),
                        },
                    },
                    reason: Some("search notes".into()),
                },
            }]
        );
    }

    #[tokio::test]
    async fn knowledge_result_plans_the_sandbox_exec() {
        let intents = plan(context(vec![
            user("list the project"),
            agent_action(ActionRequest {
                id: ActionId::new("act-s1-1"),
                action: Action::Knowledge {
                    action: KnowledgeAction::Call {
                        source: SourceId::new("notes"),
                        tool: "search".into(),
                        args: serde_json::json!({ "query": "list the project" }),
                    },
                },
                reason: Some("search notes".into()),
            }),
            observation(ActionId::new("act-s1-1"), Outcome::Text { content: "the notes are quiet".into() }),
        ]))
        .await;
        assert_eq!(
            intents,
            vec![
                Intent::Say { text: "I looked into that.".into() },
                Intent::Act {
                    request: ActionRequest {
                        id: ActionId::new("act-s1-3"),
                        action: Action::Sandbox {
                            action: SandboxAction::Exec {
                                command: "echo hello from the sandbox".into(),
                                cwd: None,
                                timeout_ms: Some(5_000)
                            },
                        },
                        reason: Some("run the command".into()),
                    },
                },
            ]
        );
    }

    #[tokio::test]
    async fn exec_result_closes_the_turn() {
        let intents = plan(context(vec![
            user("list the project"),
            agent_action(ActionRequest {
                id: ActionId::new("act-s1-1"),
                action: Action::Knowledge {
                    action: KnowledgeAction::Call {
                        source: SourceId::new("notes"),
                        tool: "search".into(),
                        args: serde_json::json!({ "query": "list the project" }),
                    },
                },
                reason: Some("search notes".into()),
            }),
            observation(ActionId::new("act-s1-1"), Outcome::Text { content: "the notes are quiet".into() }),
            agent_action(ActionRequest {
                id: ActionId::new("act-s1-3"),
                action: Action::Sandbox {
                    action: SandboxAction::Exec {
                        command: "echo hello from the sandbox".into(),
                        cwd: None,
                        timeout_ms: Some(5_000),
                    },
                },
                reason: Some("run the command".into()),
            }),
            observation(
                ActionId::new("act-s1-3"),
                Outcome::Exec {
                    exit_code: Some(0),
                    stdout: "echo hello from the sandbox".into(),
                    stderr: String::new(),
                },
            ),
        ]))
        .await;
        assert_eq!(
            intents,
            vec![
                Intent::Say { text: "The command finished.".into() },
                Intent::Done { summary: "sandbox exec done".into() },
            ]
        );
    }

    #[tokio::test]
    async fn failed_exec_closes_the_turn() {
        let intents = plan(context(vec![
            user("list the project"),
            agent_action(ActionRequest {
                id: ActionId::new("act-s1-1"),
                action: Action::Knowledge {
                    action: KnowledgeAction::Call {
                        source: SourceId::new("notes"),
                        tool: "search".into(),
                        args: serde_json::json!({ "query": "list the project" }),
                    },
                },
                reason: Some("search notes".into()),
            }),
            observation(ActionId::new("act-s1-1"), Outcome::Text { content: "the notes are quiet".into() }),
            agent_action(ActionRequest {
                id: ActionId::new("act-s1-3"),
                action: Action::Sandbox {
                    action: SandboxAction::Exec {
                        command: "echo hello from the sandbox".into(),
                        cwd: None,
                        timeout_ms: Some(5_000),
                    },
                },
                reason: Some("run the command".into()),
            }),
            observation(ActionId::new("act-s1-3"), Outcome::Failed { message: "the command failed".into() }),
        ]))
        .await;
        assert_eq!(
            intents,
            vec![
                Intent::Say { text: "The command failed: echo hello from the sandbox".into() },
                Intent::Done { summary: "sandbox exec failed".into() },
            ]
        );
    }

    #[tokio::test]
    async fn denied_knowledge_action_is_skipped() {
        let intents = plan(context(vec![
            user("list the project"),
            agent_action(ActionRequest {
                id: ActionId::new("act-s1-1"),
                action: Action::Knowledge {
                    action: KnowledgeAction::Call {
                        source: SourceId::new("notes"),
                        tool: "search".into(),
                        args: serde_json::json!({ "query": "list the project" }),
                    },
                },
                reason: Some("search notes".into()),
            }),
            observation(ActionId::new("act-s1-1"), Outcome::Denied { reason: "no notes".into() }),
        ]))
        .await;
        assert_eq!(
            intents,
            vec![
                Intent::Say { text: "okay, skipping that: no notes".into() },
                Intent::Done { summary: "denied".into() },
            ]
        );
    }

    #[tokio::test]
    async fn denied_sandbox_action_is_skipped() {
        let intents = plan(context(vec![
            user("list the project"),
            agent_action(ActionRequest {
                id: ActionId::new("act-s1-1"),
                action: Action::Knowledge {
                    action: KnowledgeAction::Call {
                        source: SourceId::new("notes"),
                        tool: "search".into(),
                        args: serde_json::json!({ "query": "list the project" }),
                    },
                },
                reason: Some("search notes".into()),
            }),
            observation(ActionId::new("act-s1-1"), Outcome::Text { content: "the notes are quiet".into() }),
            agent_action(ActionRequest {
                id: ActionId::new("act-s1-3"),
                action: Action::Sandbox {
                    action: SandboxAction::Exec {
                        command: "echo hello from the sandbox".into(),
                        cwd: None,
                        timeout_ms: Some(5_000),
                    },
                },
                reason: Some("run the command".into()),
            }),
            observation(ActionId::new("act-s1-3"), Outcome::Denied { reason: "not worth it".into() }),
        ]))
        .await;
        assert_eq!(
            intents,
            vec![
                Intent::Say { text: "okay, skipping that: not worth it".into() },
                Intent::Done { summary: "denied".into() },
            ]
        );
    }

    #[tokio::test]
    async fn unknown_action_is_ignored() {
        let intents = plan(context(vec![
            user("list the project"),
            agent_action(ActionRequest {
                id: ActionId::new("act-s1-99"),
                action: Action::Knowledge {
                    action: KnowledgeAction::Call {
                        source: SourceId::new("notes"),
                        tool: "search".into(),
                        args: serde_json::json!({ "query": "list the project" }),
                    },
                },
                reason: Some("search notes".into()),
            }),
            observation(ActionId::new("act-s1-404"), Outcome::Text { content: "the notes are quiet".into() }),
        ]))
        .await;
        assert_eq!(intents, vec![Intent::Done { summary: "no matching action".into() }]);
    }
}
