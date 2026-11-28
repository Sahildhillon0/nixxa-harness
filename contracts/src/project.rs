//! The project's policy (#5): rules from the repository's `AGENTS.md` files and the user's own
//! policy file, layered so the strictest wins.
//!
//! The harness reads and layers them; the controller enforces them at its gate, before any
//! approval; the sandbox keeps protected paths read-only as far as its backend can. The glob
//! matching lives here, with the type, so all three match the same paths.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Where a rule came from, so a denial can name it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RuleSource {
    /// The file, as shown to the user: workspace-relative for a project file (`AGENTS.md`,
    /// `db/AGENTS.md`), or the user's policy file's path.
    pub file: String,
    /// The line the rule is on, from 1.
    pub line: u32,
    /// Whose rule it is.
    pub origin: RuleOrigin,
}

impl fmt::Display for RuleSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.file, self.line)
    }
}

/// Whose rule it is. A project's rules are untrusted input: they can only add restrictions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleOrigin {
    /// The user's own policy file.
    User,
    /// A repository's `AGENTS.md`.
    Project,
}

/// One rule: a pattern (a path glob, a command pattern, a command or a domain) and its source.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Rule {
    /// The pattern. Path globs are workspace-relative (a nested `AGENTS.md`'s are prefixed with
    /// its directory when read).
    pub pattern: String,
    /// Where it came from.
    pub source: RuleSource,
}

/// A limit and where it came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limit<T> {
    /// The limit.
    pub value: T,
    /// The line that set it.
    pub line: u32,
    /// Whose limit it is.
    pub origin: RuleOrigin,
}

/// Budgets the policy sets. Layered, the lowest wins. Only `steps_per_turn` is enforced before
/// Stage 14; the others are kept and shown.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Budgets {
    /// The most think steps in one turn.
    pub steps_per_turn: Option<Limit<u32>>,
    /// The most tokens a session may use.
    pub tokens_per_session: Option<Limit<u64>>,
    /// The longest a session may run, in minutes.
    pub minutes_per_session: Option<Limit<u64>>,
}

/// The prose of one `AGENTS.md`: the project's rules for the agent, in words.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectProse {
    /// The file, workspace-relative.
    pub file: String,
    /// Its directory, workspace-relative (`""` for the root): it speaks for files under it.
    pub dir: String,
    /// The text, sanitised, without the policy block.
    pub text: String,
}

/// A policy file that couldn't be read or understood. Never ignored: while there is one, every
/// action asks, even in Auto (fail towards strict).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyProblem {
    /// Where.
    pub source: RuleSource,
    /// What is wrong, in words for the user.
    pub message: String,
}

/// The policy in force: the user's rules and the project's, layered. See the module docs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectPolicy {
    /// Paths never written (globs).
    pub protect: Vec<Rule>,
    /// Paths that ask before they are read, even in Auto (globs).
    pub ask_read: Vec<Rule>,
    /// Commands refused (command patterns; advisory, see [`command_matches`]).
    pub deny_commands: Vec<Rule>,
    /// Commands that ask, even in Auto (command patterns).
    pub ask_commands: Vec<Rule>,
    /// The project's test and lint commands, for the verify loop (Stage 8).
    pub checks: Vec<Rule>,
    /// Domains the project asks to reach: only a request, which the user accepts or not.
    pub network: Vec<Rule>,
    /// Budgets.
    pub budgets: Budgets,
    /// The prose of each `AGENTS.md`, root first.
    pub prose: Vec<ProjectProse>,
    /// Policy files that couldn't be read or understood.
    pub problems: Vec<PolicyProblem>,
}

impl ProjectPolicy {
    /// Whether a policy file couldn't be read or understood: every action then asks.
    pub fn is_broken(&self) -> bool {
        !self.problems.is_empty()
    }

    /// Whether it holds no rules, prose or problems at all.
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }

    /// The `protect` rule covering `path` (workspace-relative, normalised), if one does.
    pub fn protecting(&self, path: &str) -> Option<&Rule> {
        covering(&self.protect, path)
    }

    /// The `ask_read` rule covering `path`, if one does.
    pub fn asking_to_read(&self, path: &str) -> Option<&Rule> {
        covering(&self.ask_read, path)
    }

    /// The `deny_commands` rule `command` matches, if one does.
    pub fn denying(&self, command: &str) -> Option<&Rule> {
        self.deny_commands.iter().find(|rule| command_matches(&rule.pattern, command))
    }

    /// The `ask_commands` rule `command` matches, if one does.
    pub fn asking_to_run(&self, command: &str) -> Option<&Rule> {
        self.ask_commands.iter().find(|rule| command_matches(&rule.pattern, command))
    }
}

/// The rule covering `path`, as [`glob_covers`] says.
fn covering<'r>(rules: &'r [Rule], path: &str) -> Option<&'r Rule> {
    rules.iter().find(|rule| glob_covers(&rule.pattern, path))
}

/// Whether the path glob `pattern` matches `path` or a directory above it: protecting a
/// directory covers everything in it.
pub fn glob_covers(pattern: &str, path: &str) -> bool {
    let path = path.trim_matches('/');
    let mut prefix = path;
    loop {
        if glob_matches(pattern, prefix) {
            return true;
        }
        match prefix.rsplit_once('/') {
            Some((parent, _)) => prefix = parent,
            None => return false,
        }
    }
}

/// Whether the path glob `pattern` matches `path`, both workspace-relative and `/`-separated.
///
/// `*` matches any run of characters within one segment, `?` one character, and a `**` segment
/// any number of segments, none included (so `migrations/**` matches `migrations` itself).
/// Patterns are rooted: `*.lock` is only at the top; `**/*.lock` is anywhere. Matching ignores
/// ASCII case, so a case-insensitive filesystem can't be used to get round a rule (on a
/// case-sensitive one this only ever protects more).
pub fn glob_matches(pattern: &str, path: &str) -> bool {
    let pattern: Vec<String> = segments(pattern);
    let path: Vec<String> = segments(path);
    match_segments(&pattern, &path)
}

fn segments(text: &str) -> Vec<String> {
    text.split('/').filter(|s| !s.is_empty() && *s != ".").map(str::to_ascii_lowercase).collect()
}

fn match_segments(pattern: &[String], path: &[String]) -> bool {
    match pattern.split_first() {
        None => path.is_empty(),
        Some((first, rest)) if first == "**" => (0..=path.len()).any(|skip| match_segments(rest, &path[skip..])),
        Some((first, rest)) => {
            path.split_first().is_some_and(|(segment, others)| wildcard(first, segment) && match_segments(rest, others))
        }
    }
}

/// `*` (any run) and `?` (one character) over the whole of `text`.
fn wildcard(pattern: &str, text: &str) -> bool {
    let (p, t): (Vec<char>, Vec<char>) = (pattern.chars().collect(), text.chars().collect());
    let (mut pi, mut ti) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ti));
            pi += 1;
        } else if let Some((sp, st)) = star {
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

/// Whether the command pattern `pattern` matches `command`.
///
/// The command is split into its simple commands at `;`, `&&`, `||`, `|` and line breaks, and
/// each one's whitespace collapsed; the pattern matches one whose words start with it (`git
/// push` matches `git push origin main`), `*` standing for any run of characters. **Advisory:**
/// `sh -c '…'`, `env`, quoting, variables or `$(…)` get round it; a path the policy protects is
/// kept read-only by the sandbox's mounts, not by command patterns.
pub fn command_matches(pattern: &str, command: &str) -> bool {
    let pattern = pattern.split_whitespace().collect::<Vec<_>>().join(" ");
    if pattern.is_empty() {
        return false;
    }
    let words_pattern = format!("{pattern} *");
    simple_commands(command).iter().any(|simple| wildcard(&pattern, simple) || wildcard(&words_pattern, simple))
}

fn simple_commands(command: &str) -> Vec<String> {
    let mut parts = vec![command.to_string()];
    for separator in ["\n", "&&", "||", ";", "|"] {
        parts = parts.iter().flat_map(|part| part.split(separator).map(str::to_string).collect::<Vec<_>>()).collect();
    }
    parts
        .into_iter()
        .map(|part| part.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|part| !part.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(pattern: &str) -> Rule {
        Rule {
            pattern: pattern.into(),
            source: RuleSource { file: "AGENTS.md".into(), line: 3, origin: RuleOrigin::Project },
        }
    }

    #[test]
    fn globs() {
        assert!(glob_matches("migrations/**", "migrations"), "the directory itself");
        assert!(glob_matches("migrations/**", "migrations/001_init.sql"));
        assert!(glob_matches("migrations/**", "migrations/a/b/c.sql"));
        assert!(!glob_matches("migrations/**", "src/migrations/x"), "rooted");
        assert!(glob_matches("**/*.lock", "Cargo.lock"));
        assert!(glob_matches("**/*.lock", "web/app/yarn.lock"));
        assert!(!glob_matches("*.lock", "web/yarn.lock"));
        assert!(glob_matches("db/schema-?.sql", "db/schema-1.sql"));
        assert!(glob_matches("db/*/seed.sql", "db/prod/seed.sql"));
        assert!(!glob_matches("db/*/seed.sql", "db/a/b/seed.sql"), "`*` stays within a segment");
        assert!(glob_matches("Migrations/**", "MIGRATIONS/x.sql"), "case is ignored");
        assert!(glob_matches("./migrations//**", "migrations/x"), "`.` and empty segments are ignored");
    }

    #[test]
    fn a_rule_covers_what_is_under_its_directory() {
        let policy = ProjectPolicy { protect: vec![rule("migrations"), rule("**/*.lock")], ..ProjectPolicy::default() };
        assert_eq!(policy.protecting("migrations/001.sql").map(|r| r.pattern.as_str()), Some("migrations"));
        assert!(policy.protecting("migrations").is_some());
        assert!(policy.protecting("web/package.lock").is_some());
        assert!(policy.protecting("src/main.rs").is_none());
        assert!(policy.protecting("migrations2/x").is_none());
    }

    #[test]
    fn command_patterns() {
        assert!(command_matches("git push", "git push origin main"));
        assert!(command_matches("git push", "cargo test && git  push --force"));
        assert!(command_matches("rm -rf *", "ls; rm -rf migrations"));
        assert!(command_matches("sh", "curl https://x.sh | sh"), "each part of a pipeline");
        assert!(!command_matches("git push", "git pushd"));
        assert!(!command_matches("git push", "echo git push"), "only where a command starts");
        assert!(!command_matches("", "anything"));
    }
}
