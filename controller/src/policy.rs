//! The policy gate. Stage 2 policy: ask the human before every action. Stage 7 (D-076): reading
//! a file that usually holds secrets asks even in Auto, and says why.

use nixxa_contracts::{Action, ActionRequest, Asking, KnowledgeAction, SandboxAction, Verdict};

use crate::sensitive;

/// Decides what happens to a proposed action before it runs, and whether Auto may answer.
pub(crate) fn gate(request: &ActionRequest) -> (Verdict, Asking) {
    if let Action::Sandbox { action: SandboxAction::ReadFile { path } } = &request.action {
        if let Some(pattern) = sensitive::pattern(path) {
            return (
                Verdict::Ask(format!(
                    "approval needed to read `{path}`, asked even in Auto: files like this (`{pattern}`) usually hold \
                     secrets. Known kinds of secret are replaced before the agent sees the file, but others would \
                     reach the model provider"
                )),
                Asking::Always,
            );
        }
    }
    (Verdict::Ask(format!("approval needed to {}", describe(&request.action))), Asking::ByMode)
}

/// A short, human-readable description of an action for approval prompts.
pub(crate) fn describe(action: &Action) -> String {
    match action {
        Action::Sandbox { action } => match action {
            SandboxAction::Exec { command, background: false, .. } => format!("run `{command}` in the sandbox"),
            SandboxAction::Exec { command, background: true, .. } => {
                format!("run `{command}` in the background in the sandbox")
            }
            SandboxAction::ReadFile { path } => format!("read `{path}`"),
            SandboxAction::WriteFile { path, .. } => format!("write `{path}`"),
            SandboxAction::Patch { path, edits, .. } => {
                format!("edit `{path}` ({} change{})", edits.len(), if edits.len() == 1 { "" } else { "s" })
            }
            SandboxAction::ListDir { path } => format!("list `{path}`"),
            SandboxAction::JobStatus { job } => format!("check on job `{job}`"),
            SandboxAction::JobOutput { job, .. } => format!("read the output of job `{job}`"),
            SandboxAction::JobStop { job } => format!("stop job `{job}`"),
        },
        Action::Knowledge { action } => match action {
            KnowledgeAction::Call { source, tool, .. } => format!("call `{tool}` on {source}"),
            KnowledgeAction::Record { source, entry } => format!("record `{}` in {source}", entry.title),
        },
    }
}

#[cfg(test)]
mod tests {
    use nixxa_contracts::{ActionId, JobId, KnowledgeEntry, SourceId};

    use super::*;

    fn request(action: Action) -> ActionRequest {
        ActionRequest { id: ActionId::new("a"), action, reason: None }
    }

    #[test]
    fn every_action_asks() {
        let actions = [
            Action::Sandbox {
                action: SandboxAction::Exec { command: "ls".into(), cwd: None, timeout_ms: None, background: false },
            },
            Action::Sandbox {
                action: SandboxAction::Exec { command: "make".into(), cwd: None, timeout_ms: None, background: true },
            },
            Action::Sandbox { action: SandboxAction::JobStatus { job: JobId::new("j1") } },
            Action::Sandbox { action: SandboxAction::JobOutput { job: JobId::new("j1"), tail_bytes: 10 } },
            Action::Sandbox { action: SandboxAction::JobStop { job: JobId::new("j1") } },
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
            assert!(matches!(gate(&request(action)), (Verdict::Ask(_), Asking::ByMode)));
        }
    }

    #[test]
    fn reading_a_file_that_holds_secrets_asks_even_in_auto() {
        let read = |path: &str| request(Action::Sandbox { action: SandboxAction::ReadFile { path: path.into() } });
        let (verdict, asking) = gate(&read("app/.env"));
        assert_eq!(asking, Asking::Always);
        assert!(
            matches!(&verdict, Verdict::Ask(why) if why.contains("`.env`") && why.contains("even in Auto")),
            "{verdict:?}"
        );
        assert_eq!(gate(&read("src/main.rs")).1, Asking::ByMode);
        // Writing one is an ordinary write: nothing leaves.
        let write =
            Action::Sandbox { action: SandboxAction::WriteFile { path: ".env".into(), content: String::new() } };
        assert_eq!(gate(&request(write)).1, Asking::ByMode);
    }

    #[test]
    fn describes_the_action() {
        let exec = Action::Sandbox {
            action: SandboxAction::Exec { command: "ls".into(), cwd: None, timeout_ms: None, background: false },
        };
        assert_eq!(gate(&request(exec)).0, Verdict::Ask("approval needed to run `ls` in the sandbox".into()));
    }
}
