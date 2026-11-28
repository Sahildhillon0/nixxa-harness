//! The policy view (`ctrl+p`, D-081): the rules in force and where each came from, how the
//! sandbox keeps protected paths, the files that couldn't be read, and a change to the policy
//! files waiting for the user. No I/O here: reading is in [`crate::policy`], applying in the
//! TUI loop.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use nixxa_contracts::{ProjectPolicy, Protection, ProtectionKind, Rule, RuleOrigin, RuleSource};

use crate::policy::Sources;

/// A change to the policy files, waiting for the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Pending {
    pub(crate) sources: Sources,
    pub(crate) policy: ProjectPolicy,
    /// What would change, one line each.
    pub(crate) changes: Vec<String>,
}

/// What a key in the view asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PolicyKey {
    Nothing,
    Close,
    /// Put the pending change in force.
    Accept,
}

/// The view's state; it lives for the run (one policy per workplace).
#[derive(Debug, Default)]
pub(crate) struct PolicyView {
    pub(crate) open: bool,
    /// The files as read for the policy in force.
    pub(crate) sources: Sources,
    /// The policy in force.
    pub(crate) policy: ProjectPolicy,
    /// What the sandbox said it keeps, or why it keeps nothing.
    pub(crate) protection: Option<Result<Protection, String>>,
    /// A change waiting for the user.
    pub(crate) pending: Option<Pending>,
    /// Files as read for a change the user turned down: not offered again until they change.
    pub(crate) declined: Option<Sources>,
}

impl PolicyView {
    /// A fresh read of the files: a change to offer, if they differ from both the policy in
    /// force and what the user turned down. `None` when there is nothing new.
    pub(crate) fn offer(&mut self, read: Sources) -> Option<&Pending> {
        let already = self.pending.as_ref().is_some_and(|p| p.sources == read);
        if read == self.sources || self.declined.as_ref() == Some(&read) || already {
            if read == self.sources {
                self.pending = None;
            }
            return None;
        }
        let policy = read.policy();
        let changes = describe(&self.policy, &policy);
        self.pending = Some(Pending { sources: read, policy, changes });
        self.pending.as_ref()
    }

    /// The pending change, now in force. The caller applies it.
    pub(crate) fn accept(&mut self) -> Option<Pending> {
        let pending = self.pending.take()?;
        self.sources = pending.sources.clone();
        self.policy = pending.policy.clone();
        self.declined = None;
        Some(pending)
    }

    /// A key while the view is open.
    pub(crate) fn key(&mut self, key: KeyEvent) -> PolicyKey {
        let ctrl_p = key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('p');
        match key.code {
            KeyCode::Esc => self.close(),
            _ if ctrl_p => self.close(),
            KeyCode::Char('y') if self.pending.is_some() => PolicyKey::Accept,
            KeyCode::Char('n') if self.pending.is_some() => {
                self.declined = self.pending.take().map(|p| p.sources);
                PolicyKey::Nothing
            }
            _ => PolicyKey::Nothing,
        }
    }

    fn close(&mut self) -> PolicyKey {
        self.open = false;
        PolicyKey::Close
    }

    /// The status bar's part, if there is anything to say.
    pub(crate) fn status(&self) -> Option<(String, Tone)> {
        if self.pending.is_some() {
            return Some(("policy: changed, ctrl+p".into(), Tone::Pending));
        }
        if self.policy.is_broken() {
            return Some(("policy: unreadable, every action asks".into(), Tone::Bad));
        }
        let rules = rule_count(&self.policy);
        if rules == 0 {
            return None;
        }
        let gate_only = match &self.protection {
            Some(Ok(p)) => p.how != ProtectionKind::ReadOnlyMounts && !self.policy.protect.is_empty(),
            Some(Err(_)) => !self.policy.protect.is_empty(),
            None => false,
        };
        let plural = if rules == 1 { "" } else { "s" };
        let how = if gate_only { ", gate only" } else { "" };
        Some((format!("policy: {rules} rule{plural}{how}"), Tone::Ok))
    }

    /// What the view shows, line by line, every string from a file through `safe::line` by the
    /// caller.
    pub(crate) fn lines(&self) -> Vec<(Tone, String)> {
        let mut out: Vec<(Tone, String)> = Vec::new();
        if let Some(pending) = &self.pending {
            out.push((Tone::Pending, "A policy file changed. The change isn't in force until you accept it.".into()));
            for change in &pending.changes {
                out.push((Tone::Pending, format!("  • {change}")));
            }
            for problem in &pending.policy.problems {
                out.push((Tone::Bad, format!("  ✗ {}: {}", where_from(&problem.source), problem.message)));
            }
            out.push((
                Tone::Pending,
                "y accept (a sandbox restarts to apply new mounts: its jobs and the terminal end) · n keep the policy \
                 in force"
                    .into(),
            ));
            out.push((Tone::Plain, String::new()));
        }
        let files: Vec<&str> = self.sources.project.iter().map(|(f, _)| f.as_str()).collect();
        out.push((Tone::Heading, "Files".into()));
        if files.is_empty() && self.sources.user.is_none() {
            out.push((Tone::Dim, "No AGENTS.md here, and no policy.toml of yours.".into()));
        }
        if let Some((file, _)) = &self.sources.user {
            out.push((Tone::Plain, format!("  your policy: {file}")));
        }
        for file in files {
            out.push((Tone::Plain, format!("  {file}")));
        }
        for note in &self.sources.notes {
            out.push((Tone::Dim, format!("  {note}")));
        }
        for problem in &self.policy.problems {
            out.push((Tone::Bad, format!("  ✗ {}: {}", where_from(&problem.source), problem.message)));
        }
        if self.policy.is_broken() {
            out.push((Tone::Bad, "  While a policy file can't be read, every action asks, even in Auto.".into()));
        }
        let lists: [(&str, &[Rule]); 6] = [
            ("Protected (never changed)", &self.policy.protect),
            ("Asks before reading", &self.policy.ask_read),
            ("Commands refused (advisory: a command can be disguised)", &self.policy.deny_commands),
            ("Commands that ask", &self.policy.ask_commands),
            ("Checks", &self.policy.checks),
            ("Network the project asks for (ctrl+n, r to allow)", &self.policy.network),
        ];
        for (title, rules) in lists {
            if rules.is_empty() {
                continue;
            }
            out.push((Tone::Plain, String::new()));
            out.push((Tone::Heading, title.into()));
            for rule in rules {
                out.push((Tone::Plain, format!("  {}  ({})", rule.pattern, where_from(&rule.source))));
            }
        }
        let budgets = &self.policy.budgets;
        let limits: Vec<String> = [
            budgets.steps_per_turn.map(|l| format!("{} steps a turn ({})", l.value, origin(l.origin, l.line))),
            budgets
                .tokens_per_session
                .map(|l| format!("{} tokens a session, from Stage 14 ({})", l.value, origin(l.origin, l.line))),
            budgets
                .minutes_per_session
                .map(|l| format!("{} minutes a session, from Stage 14 ({})", l.value, origin(l.origin, l.line))),
        ]
        .into_iter()
        .flatten()
        .collect();
        if !limits.is_empty() {
            out.push((Tone::Plain, String::new()));
            out.push((Tone::Heading, "Budgets (the lowest wins)".into()));
            out.extend(limits.into_iter().map(|l| (Tone::Plain, format!("  {l}"))));
        }
        if !self.policy.protect.is_empty() {
            out.push((Tone::Plain, String::new()));
            out.push((Tone::Heading, "How protected paths are kept".into()));
            match &self.protection {
                Some(Ok(p)) => {
                    let how = match p.how {
                        ProtectionKind::ReadOnlyMounts => {
                            "mounted read-only in the sandbox: no command, file action or terminal can change them"
                        }
                        ProtectionKind::FileActions => {
                            "file actions are refused, but commands can change them: nothing is mounted here, so the \
                             gate is the only enforcement for commands"
                        }
                        ProtectionKind::None => "only the gate refuses file actions",
                    };
                    out.push((Tone::Plain, format!("  {how}")));
                    if !p.kept.is_empty() {
                        out.push((Tone::Dim, format!("  kept: {}", p.kept.join(", "))));
                    }
                    for rule in &p.unmatched {
                        out.push((Tone::Dim, format!("  {rule} matches nothing yet: only the gate holds it")));
                    }
                    for gap in &p.gaps {
                        out.push((Tone::Bad, format!("  ! {gap}")));
                    }
                }
                Some(Err(why)) => out.push((Tone::Bad, format!("  only the gate refuses file actions: {why}"))),
                None => out.push((Tone::Dim, "  not applied yet".into())),
            }
        }
        out
    }
}

/// How a line is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tone {
    Plain,
    Dim,
    Heading,
    Ok,
    Pending,
    Bad,
}

fn where_from(source: &RuleSource) -> String {
    match source.origin {
        RuleOrigin::Project => source.to_string(),
        RuleOrigin::User => format!("your policy, line {}", source.line),
    }
}

fn origin(origin: RuleOrigin, line: u32) -> String {
    match origin {
        RuleOrigin::Project => format!("project, line {line}"),
        RuleOrigin::User => format!("your policy, line {line}"),
    }
}

fn rule_count(policy: &ProjectPolicy) -> usize {
    policy.protect.len() + policy.ask_read.len() + policy.deny_commands.len() + policy.ask_commands.len()
}

/// What would change, one line each, for the user.
fn describe(old: &ProjectPolicy, new: &ProjectPolicy) -> Vec<String> {
    let mut lines = Vec::new();
    let lists: [(&str, &[Rule], &[Rule]); 6] = [
        ("protect", &old.protect, &new.protect),
        ("ask_read", &old.ask_read, &new.ask_read),
        ("deny_commands", &old.deny_commands, &new.deny_commands),
        ("ask_commands", &old.ask_commands, &new.ask_commands),
        ("checks", &old.checks, &new.checks),
        ("network (a request)", &old.network, &new.network),
    ];
    for (what, before, after) in lists {
        let has = |rules: &[Rule], pattern: &str| rules.iter().any(|r| r.pattern == pattern);
        for rule in after.iter().filter(|r| !has(before, &r.pattern)) {
            lines.push(format!("{what}: + {} ({})", rule.pattern, where_from(&rule.source)));
        }
        for rule in before.iter().filter(|r| !has(after, &r.pattern)) {
            lines.push(format!("{what}: - {} (was {})", rule.pattern, where_from(&rule.source)));
        }
    }
    if old.budgets != new.budgets {
        lines.push("budgets changed".into());
    }
    if old.prose != new.prose {
        lines.push("the prose (the project's rules in words) changed".into());
    }
    if lines.is_empty() && new.problems.is_empty() {
        lines.push("nothing that changes the rules (only formatting)".into());
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::key;

    fn sources(text: &str) -> Sources {
        Sources { project: vec![("AGENTS.md".into(), Ok(text.into()))], ..Sources::default() }
    }

    fn in_force(text: &str) -> PolicyView {
        let sources = sources(text);
        PolicyView { policy: sources.policy(), sources, ..PolicyView::default() }
    }

    const PROTECTED: &str = "```nixxa-policy\nprotect = [\"migrations/**\"]\n```\n";

    #[test]
    fn a_changed_file_waits_for_the_user() {
        let mut view = in_force(PROTECTED);
        assert!(view.offer(sources(PROTECTED)).is_none(), "the same files: nothing to offer");
        let loosened = "```nixxa-policy\nprotect = []\n```\n";
        let pending = view.offer(sources(loosened)).expect("a change").clone();
        assert!(pending.changes.iter().any(|c| c.contains("protect: - migrations/**")), "{:?}", pending.changes);
        // Nothing changed until accepted.
        assert_eq!(view.policy.protect[0].pattern, "migrations/**");
        assert_eq!(view.status().map(|(s, _)| s), Some("policy: changed, ctrl+p".into()));
        // Turned down: not offered again for the same files.
        assert_eq!(view.key(key(KeyCode::Char('n'))), PolicyKey::Nothing);
        assert!(view.pending.is_none());
        assert!(view.offer(sources(loosened)).is_none());
        assert_eq!(view.policy.protect.len(), 1);
        // Changed again: offered; accepted: in force.
        let other = "```nixxa-policy\nprotect = [\"schema/**\"]\n```\n";
        assert!(view.offer(sources(other)).is_some());
        assert_eq!(view.key(key(KeyCode::Char('y'))), PolicyKey::Accept);
        let accepted = view.accept().expect("accepted");
        assert_eq!(accepted.policy.protect[0].pattern, "schema/**");
        assert_eq!(view.policy.protect[0].pattern, "schema/**");
    }

    #[test]
    fn the_status_says_how_protected_paths_are_kept() {
        let mut view = in_force(PROTECTED);
        view.protection = Some(Ok(Protection { how: ProtectionKind::ReadOnlyMounts, ..Protection::default() }));
        assert_eq!(view.status().map(|(s, _)| s), Some("policy: 1 rule".into()));
        view.protection = Some(Ok(Protection { how: ProtectionKind::FileActions, ..Protection::default() }));
        assert_eq!(view.status().map(|(s, _)| s), Some("policy: 1 rule, gate only".into()));
        let broken = in_force("```nixxa-policy\nauto = true\n```\n");
        assert_eq!(broken.status().map(|(_, t)| t), Some(Tone::Bad));
        assert!(broken.lines().iter().any(|(_, l)| l.contains("every action asks")));
        assert_eq!(PolicyView::default().status(), None);
    }

    #[test]
    fn the_view_names_every_rules_source() {
        let mut view = in_force(PROTECTED);
        view.protection = Some(Ok(Protection {
            how: ProtectionKind::ReadOnlyMounts,
            kept: vec!["migrations".into()],
            unmatched: Vec::new(),
            gaps: vec!["`migrations/1.sql` has another hard link".into()],
        }));
        let lines: Vec<String> = view.lines().into_iter().map(|(_, l)| l).collect();
        assert!(lines.iter().any(|l| l == "  migrations/**  (AGENTS.md:2)"), "{lines:#?}");
        assert!(lines.iter().any(|l| l.contains("mounted read-only")));
        assert!(lines.iter().any(|l| l.contains("another hard link")));
    }
}
