//! The policy gate. Stage 2 policy: ask the human before every action.

use nixxa_contracts::{Action, ActionRequest, KnowledgeAction, SandboxAction, Verdict};

/// Decides what happens to a proposed action before it runs.
pub(crate) fn gate(request: &ActionRequest) -> Verdict {
    Verdict::Ask(format!("approval needed to {}", describe(&request.action)))
}

/// A short, human-readable description of an action for approval prompts.
pub(crate) fn describe(action: &Action) -> String {
    match action {
        Action::Sandbox { action } => match action {
            SandboxAction::Exec { command, .. } => format!("run `{command}` in the sandbox"),
            SandboxAction::ReadFile { path } => format!("read `{path}`"),
            SandboxAction::WriteFile { path, .. } => format!("write `{path}`"),
            SandboxAction::ListDir { path } => format!("list `{path}`"),
        },
        Action::Knowledge { action } => match action {
            KnowledgeAction::Call { source, tool, .. } => format!("call `{tool}` on {source}"),
            KnowledgeAction::Record { source, entry } => format!("record `{}` in {source}", entry.title),
        },
    }
}

#[cfg(test)]
mod tests {
    use nixxa_contracts::{ActionId, KnowledgeEntry, SourceId};

    use super::*;

    fn request(action: Action) -> ActionRequest {
        ActionRequest { id: ActionId::new("a"), action, reason: None }
    }

    #[test]
    fn every_action_asks() {
        let actions = [
            Action::Sandbox { action: SandboxAction::Exec { command: "ls".into(), cwd: None, timeout_ms: None } },
            Action::Sandbox { action: SandboxAction::ReadFile { path: "a".into() } },
            Action::Sandbox { action: SandboxAction::WriteFile { path: "a".into(), content: String::new() } },
            Action::Sandbox { action: SandboxAction::ListDir { path: ".".into() } },
            Action::Knowledge {
                action: KnowledgeAction::Call {
                    source: SourceId::new("notes"),
                    tool: "search".into(),
                    args: serde_json::Value::Null,
                },
            },
            Action::Knowledge {
                action: KnowledgeAction::Record {
                    source: SourceId::new("notes"),
                    entry: KnowledgeEntry { title: "t".into(), body: String::new(), tags: Vec::new() },
                },
            },
        ];
        for action in actions {
            assert!(matches!(gate(&request(action)), Verdict::Ask(_)));
        }
    }

    #[test]
    fn describes_the_action() {
        let exec =
            Action::Sandbox { action: SandboxAction::Exec { command: "ls".into(), cwd: None, timeout_ms: None } };
        assert_eq!(gate(&request(exec)), Verdict::Ask("approval needed to run `ls` in the sandbox".into()));
    }
}
