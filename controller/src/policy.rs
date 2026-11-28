//! The policy gate, before any approval and in every mode (wake-up turns included). Stage 2:
//! ask the human before every action. Stage 7: reading a file that usually holds secrets asks
//! even in Auto (D-076), and the project policy (#5, D-080) refuses writes to protected paths
//! and refused commands, and asks, even in Auto, for its `ask_read` paths and `ask_commands`;
//! while a policy file can't be read, every action asks.

use std::path::Path;

use nixxa_contracts::{Action, ActionRequest, Asking, KnowledgeAction, ProjectPolicy, SandboxAction, Verdict};

use crate::project::{self, workspace_path};
use crate::sensitive;

/// Decides what happens to a proposed action before it runs, and whether Auto may answer.
/// `root` is the workspace on the host, for paths the agent writes in full.
pub(crate) fn gate(request: &ActionRequest, policy: &ProjectPolicy, root: &Path) -> (Verdict, Asking) {
    let (verdict, asking) = decide(request, policy, root);
    match verdict {
        Verdict::Ask(reason) if policy.is_broken() => (
            Verdict::Ask(format!(
                "{reason} (asked even in Auto: a project policy file can't be read, so every action asks; see ctrl+p)"
            )),
            Asking::Always,
        ),
        verdict => (verdict, asking),
    }
}

fn decide(request: &ActionRequest, policy: &ProjectPolicy, root: &Path) -> (Verdict, Asking) {
    let Action::Sandbox { action } = &request.action else {
        return (Verdict::Ask(format!("approval needed to {}", describe(&request.action))), Asking::ByMode);
    };
    match action {
        SandboxAction::WriteFile { path, .. } | SandboxAction::Patch { path, .. } => {
            if let Some(rule) = policy.protecting(&workspace_path(path, root)) {
                return (
                    Verdict::Deny(format!(
                        "refused by the project policy: `{path}` is protected by {}, so it can't be changed in this \
                         session",
                        project::rule(rule)
                    )),
                    Asking::ByMode,
                );
            }
        }
        SandboxAction::Exec { command, .. } => {
            if let Some(rule) = policy.denying(command) {
                return (
                    Verdict::Deny(format!(
                        "refused by the project policy: the command matches {}, which is refused in this session",
                        project::rule(rule)
                    )),
                    Asking::ByMode,
                );
            }
            if let Some(rule) = policy.asking_to_run(command) {
                return (
                    Verdict::Ask(format!(
                        "approval needed to {}, asked even in Auto: the project policy asks for commands like {}",
                        describe(&request.action),
                        project::rule(rule)
                    )),
                    Asking::Always,
                );
            }
        }
        SandboxAction::ReadFile { path } => {
            if let Some(rule) = policy.asking_to_read(&workspace_path(path, root)) {
                return (
                    Verdict::Ask(format!(
                        "approval needed to read `{path}`, asked even in Auto: the project policy asks first for {}",
                        project::rule(rule)
                    )),
                    Asking::Always,
                );
            }
            if let Some(pattern) = sensitive::pattern(path) {
                return (
                    Verdict::Ask(format!(
                        "approval needed to read `{path}`, asked even in Auto: files like this (`{pattern}`) usually \
                         hold secrets. Known kinds of secret are replaced before the agent sees the file, but others \
                         would reach the model provider"
                    )),
                    Asking::Always,
                );
            }
        }
        _ => {}
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
    use nixxa_contracts::{ActionId, JobId, KnowledgeEntry, Rule, RuleOrigin, RuleSource, SourceId};

    use super::*;

    fn request(action: Action) -> ActionRequest {
        ActionRequest { id: ActionId::new("a"), action, reason: None }
    }

    fn none() -> ProjectPolicy {
        ProjectPolicy::default()
    }

    fn root() -> &'static Path {
        Path::new("/home/u/proj")
    }

    fn rule(pattern: &str) -> Rule {
        Rule {
            pattern: pattern.into(),
            source: RuleSource { file: "AGENTS.md".into(), line: 12, origin: RuleOrigin::Project },
        }
    }

    fn sandbox(action: SandboxAction) -> ActionRequest {
        request(Action::Sandbox { action })
    }

    fn write(path: &str) -> ActionRequest {
        sandbox(SandboxAction::WriteFile { path: path.into(), content: String::new() })
    }

    fn patch(path: &str) -> ActionRequest {
        sandbox(SandboxAction::Patch {
            path: path.into(),
            edits: vec![nixxa_contracts::Edit { search: "a".into(), replace: "b".into() }],
            expected_hash: String::new(),
        })
    }

    fn exec(command: &str) -> ActionRequest {
        sandbox(SandboxAction::Exec { command: command.into(), cwd: None, timeout_ms: None, background: false })
    }

    #[test]
    fn writes_to_protected_paths_are_refused_with_the_rule_however_spelt() {
        let policy = ProjectPolicy { protect: vec![rule("migrations/**")], ..ProjectPolicy::default() };
        for path in [
            "migrations/001.sql",
            "./migrations/001.sql",
            "migrations//001.sql",
            "src/../migrations/001.sql",
            "/workspace/migrations/001.sql",
            "/home/u/proj/migrations/001.sql",
            "Migrations/001.sql",
            "migrations",
        ] {
            for request in [write(path), patch(path)] {
                let (verdict, _) = gate(&request, &policy, root());
                assert!(
                    matches!(&verdict, Verdict::Deny(why) if why.contains("`migrations/**` (AGENTS.md:12)")),
                    "{path}: {verdict:?}"
                );
            }
        }
        assert!(matches!(gate(&write("src/main.rs"), &policy, root()).0, Verdict::Ask(_)));
        assert!(matches!(gate(&write("migrations2/x"), &policy, root()).0, Verdict::Ask(_)));
        // Reading a protected path is fine.
        let read = sandbox(SandboxAction::ReadFile { path: "migrations/001.sql".into() });
        assert_eq!(gate(&read, &policy, root()).1, Asking::ByMode);
    }

    #[test]
    fn commands_are_refused_or_asked_and_ask_read_asks_even_in_auto() {
        let policy = ProjectPolicy {
            deny_commands: vec![rule("git push")],
            ask_commands: vec![rule("npm publish")],
            ask_read: vec![rule("data/**")],
            ..ProjectPolicy::default()
        };
        assert!(matches!(gate(&exec("cargo test && git push"), &policy, root()), (Verdict::Deny(_), _)));
        let (verdict, asking) = gate(&exec("npm publish --dry-run"), &policy, root());
        assert_eq!(asking, Asking::Always);
        assert!(matches!(&verdict, Verdict::Ask(why) if why.contains("`npm publish` (AGENTS.md:12)")), "{verdict:?}");
        let read = sandbox(SandboxAction::ReadFile { path: "./data/users.csv".into() });
        assert_eq!(gate(&read, &policy, root()).1, Asking::Always);
        assert_eq!(gate(&exec("ls"), &policy, root()).1, Asking::ByMode);
    }

    #[test]
    fn a_policy_that_cant_be_read_makes_everything_ask_even_in_auto() {
        let policy = ProjectPolicy {
            problems: vec![nixxa_contracts::PolicyProblem {
                source: RuleSource { file: "AGENTS.md".into(), line: 4, origin: RuleOrigin::Project },
                message: "unknown key `auto`".into(),
            }],
            ..ProjectPolicy::default()
        };
        let (verdict, asking) = gate(&exec("ls"), &policy, root());
        assert_eq!(asking, Asking::Always);
        assert!(matches!(&verdict, Verdict::Ask(why) if why.contains("can't be read")), "{verdict:?}");
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
            assert!(matches!(gate(&request(action), &none(), root()), (Verdict::Ask(_), Asking::ByMode)));
        }
    }

    #[test]
    fn reading_a_file_that_holds_secrets_asks_even_in_auto() {
        let read = |path: &str| request(Action::Sandbox { action: SandboxAction::ReadFile { path: path.into() } });
        let (verdict, asking) = gate(&read("app/.env"), &none(), root());
        assert_eq!(asking, Asking::Always);
        assert!(
            matches!(&verdict, Verdict::Ask(why) if why.contains("`.env`") && why.contains("even in Auto")),
            "{verdict:?}"
        );
        assert_eq!(gate(&read("src/main.rs"), &none(), root()).1, Asking::ByMode);
        // Writing one is an ordinary write: nothing leaves.
        let write =
            Action::Sandbox { action: SandboxAction::WriteFile { path: ".env".into(), content: String::new() } };
        assert_eq!(gate(&request(write), &none(), root()).1, Asking::ByMode);
    }

    #[test]
    fn describes_the_action() {
        let exec = Action::Sandbox {
            action: SandboxAction::Exec { command: "ls".into(), cwd: None, timeout_ms: None, background: false },
        };
        assert_eq!(
            gate(&request(exec), &none(), root()).0,
            Verdict::Ask("approval needed to run `ls` in the sandbox".into())
        );
    }
}
