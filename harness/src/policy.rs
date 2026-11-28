//! The project policy (#5, D-078, D-079): the repository's `AGENTS.md` files, read as untrusted
//! input, and the user's own `policy.toml`, layered so the strictest rule wins.
//!
//! * **Where:** `AGENTS.md` at the workspace root and in subdirectories (not `.git`,
//!   `node_modules`, `target` and the like), as many as [`MAX_FILES`], [`MAX_DEPTH`] deep. A
//!   nested file's path rules are relative to its directory, and its prose speaks for the files
//!   under it. `CLAUDE.md`, `.cursorrules` and other tools' files are not read.
//! * **How:** never through a symbolic link (the file, or a directory on the way), at most
//!   [`MAX_FILE`] bytes, UTF-8 only, control and bidirectional-override characters removed.
//! * **The policy** is a fenced ` ```nixxa-policy ` TOML block; the rest is prose. Unknown keys,
//!   bad TOML, bad globs or hosts, a second block or an unclosed one are [`PolicyProblem`]s:
//!   shown to the user, and while there is one every action asks, even in Auto.
//! * **Layering:** every rule from every file applies (a project can add rules, never remove
//!   the user's); budgets take the lowest; the project's `network` is only a request; there is
//!   no key that turns Auto on, so one is an unknown key.

use std::path::{Path, PathBuf};

use nixxa_contracts::{
    Budgets, DomainRule, Limit, PolicyProblem, ProjectPolicy, ProjectProse, Rule, RuleOrigin, RuleSource,
};
use serde::Deserialize;

/// The project file.
pub(crate) const FILE: &str = "AGENTS.md";
/// The info string of the fenced policy block.
pub(crate) const BLOCK: &str = "nixxa-policy";
/// The largest policy file read, in bytes.
pub(crate) const MAX_FILE: u64 = 64 * 1024;
/// The most `AGENTS.md` files read.
pub(crate) const MAX_FILES: usize = 32;
/// How deep below the root `AGENTS.md` files are looked for.
pub(crate) const MAX_DEPTH: usize = 8;
/// The most directory entries looked at.
const MAX_WALKED: usize = 100_000;
/// Directories never looked in: dependencies, build output, version control.
const SKIPPED: &[&str] = &[".git", ".hg", ".svn", "node_modules", "target", ".venv", "venv", "__pycache__"];

/// The policy files as read, kept to see whether they change: each project file
/// (workspace-relative) and the user's, with its text or why it couldn't be read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Sources {
    pub(crate) project: Vec<(String, Result<String, String>)>,
    pub(crate) user: Option<(String, Result<String, String>)>,
    /// What the search left out, in words (not a problem: the files it found are complete).
    pub(crate) notes: Vec<String>,
}

impl Sources {
    /// Reads the project's files under `root` (none for an empty sandbox: pass `None`) and the
    /// user's file at `user` (if there is one). Blocking.
    pub(crate) fn read(root: Option<&Path>, user: Option<&Path>) -> Self {
        let mut sources = Self::default();
        if let Some(root) = root {
            let (files, notes) = find(root);
            sources.notes = notes;
            sources.project = files.into_iter().map(|rel| (rel.clone(), read_safely(root, &rel))).collect();
        }
        if let Some(path) = user {
            let shown = path.display().to_string();
            match std::fs::symlink_metadata(path) {
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                _ => sources.user = Some((shown, read_file(path))),
            }
        }
        sources
    }

    /// The policy these files make, layered.
    pub(crate) fn policy(&self) -> ProjectPolicy {
        let mut layered = ProjectPolicy::default();
        if let Some((file, text)) = &self.user {
            let parsed = match text {
                Ok(text) => parse_user(file, text),
                Err(why) => Parsed::problem(file, 1, RuleOrigin::User, why),
            };
            layered.add(parsed);
        }
        for (file, text) in &self.project {
            let parsed = match text {
                Ok(text) => parse_project(file, text),
                Err(why) => Parsed::problem(file, 1, RuleOrigin::Project, why),
            };
            layered.add(parsed);
        }
        layered
    }
}

/// The `AGENTS.md` files under `root`, root first, never through a symbolic link; and what
/// the search left out.
fn find(root: &Path) -> (Vec<String>, Vec<String>) {
    let mut found = Vec::new();
    let mut notes = Vec::new();
    let mut walked = 0usize;
    // Breadth first, so nearer files are found before the cap.
    let mut queue = std::collections::VecDeque::from([(root.to_path_buf(), String::new(), 0usize)]);
    while let Some((dir, rel, depth)) = queue.pop_front() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            walked += 1;
            if walked > MAX_WALKED {
                notes.push(format!(
                    "the workspace has more than {MAX_WALKED} entries: {FILE} files past them weren't looked for"
                ));
                return (found, notes);
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
            let Ok(meta) = std::fs::symlink_metadata(entry.path()) else { continue };
            if name == FILE && !meta.is_dir() {
                if found.len() == MAX_FILES {
                    notes.push(format!("more than {MAX_FILES} {FILE} files: `{path}` and the rest weren't read"));
                    return (found, notes);
                }
                found.push(path);
            } else if meta.is_dir() && depth < MAX_DEPTH && !SKIPPED.contains(&name.as_str()) {
                queue.push_back((entry.path(), path, depth + 1));
            }
        }
    }
    (found, notes)
}

/// Reads `root/rel`, refusing a symbolic link, anything but a regular file, and a file that
/// changes under it between looking and opening (the agent may be running).
fn read_safely(root: &Path, rel: &str) -> Result<String, String> {
    let path = root.join(rel);
    // A directory on the way that is a symbolic link would take the read elsewhere.
    let mut on_the_way = root.to_path_buf();
    for part in Path::new(rel).parent().into_iter().flat_map(Path::components) {
        on_the_way.push(part);
        if std::fs::symlink_metadata(&on_the_way).map(|m| m.file_type().is_symlink()).unwrap_or(true) {
            return Err(format!("`{}` is a symbolic link or gone: not read", on_the_way.display()));
        }
    }
    read_file(&path)
}

/// Reads one policy file: a regular file, not a symbolic link, at most [`MAX_FILE`] bytes of
/// UTF-8, sanitised.
fn read_file(path: &Path) -> Result<String, String> {
    use std::io::Read;
    let before = std::fs::symlink_metadata(path).map_err(|err| format!("can't be read: {err}"))?;
    if before.file_type().is_symlink() {
        return Err("is a symbolic link, which isn't followed (it could point outside the workspace): not read".into());
    }
    if !before.is_file() {
        return Err("isn't a regular file: not read".into());
    }
    if before.len() > MAX_FILE {
        return Err(format!("is too large ({} bytes; the limit is {MAX_FILE}): not read", before.len()));
    }
    let file = std::fs::File::open(path).map_err(|err| format!("can't be read: {err}"))?;
    let after = file.metadata().map_err(|err| format!("can't be read: {err}"))?;
    if !same_file(&before, &after) {
        return Err("changed while it was being read: not read".into());
    }
    let mut bytes = Vec::new();
    file.take(MAX_FILE + 1).read_to_end(&mut bytes).map_err(|err| format!("can't be read: {err}"))?;
    if bytes.len() as u64 > MAX_FILE {
        return Err(format!("is too large (the limit is {MAX_FILE} bytes): not read"));
    }
    let text = String::from_utf8(bytes).map_err(|_| "isn't UTF-8 text: not read".to_string())?;
    Ok(sanitised(&text))
}

#[cfg(unix)]
fn same_file(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    a.dev() == b.dev() && a.ino() == b.ino()
}

#[cfg(not(unix))]
fn same_file(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    a.len() == b.len() && b.is_file()
}

/// `text` with line breaks as `\n`, and without control characters (but tab and line break)
/// or bidirectional overrides, which could hide text from the user while the model reads it.
pub(crate) fn sanitised(text: &str) -> String {
    let bidi = |c: char| matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{200E}' | '\u{200F}');
    text.replace("\r\n", "\n").chars().filter(|&c| (c == '\n' || c == '\t' || !c.is_control()) && !bidi(c)).collect()
}

/// One file's contribution.
#[derive(Debug, Default)]
struct Parsed {
    policy: ProjectPolicy,
}

impl Parsed {
    fn problem(file: &str, line: u32, origin: RuleOrigin, message: impl Into<String>) -> Self {
        let source = RuleSource { file: file.into(), line, origin };
        Self {
            policy: ProjectPolicy {
                problems: vec![PolicyProblem { source, message: message.into() }],
                ..ProjectPolicy::default()
            },
        }
    }
}

trait Layer {
    fn add(&mut self, parsed: Parsed);
}

impl Layer for ProjectPolicy {
    /// Adds one file's rules: every rule kept, the lowest budget wins.
    fn add(&mut self, parsed: Parsed) {
        let p = parsed.policy;
        self.protect.extend(p.protect);
        self.ask_read.extend(p.ask_read);
        self.deny_commands.extend(p.deny_commands);
        self.ask_commands.extend(p.ask_commands);
        self.checks.extend(p.checks);
        self.network.extend(p.network);
        self.prose.extend(p.prose);
        self.problems.extend(p.problems);
        self.budgets.steps_per_turn = lowest(self.budgets.steps_per_turn, p.budgets.steps_per_turn);
        self.budgets.tokens_per_session = lowest(self.budgets.tokens_per_session, p.budgets.tokens_per_session);
        self.budgets.minutes_per_session = lowest(self.budgets.minutes_per_session, p.budgets.minutes_per_session);
    }
}

fn lowest<T: Ord + Copy>(a: Option<Limit<T>>, b: Option<Limit<T>>) -> Option<Limit<T>> {
    match (a, b) {
        (Some(a), Some(b)) => Some(if b.value < a.value { b } else { a }),
        (a, b) => a.or(b),
    }
}

/// The TOML of a policy block or file. Every key is optional; any other key is an error.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    #[serde(default)]
    protect: Vec<toml::Spanned<String>>,
    #[serde(default)]
    ask_read: Vec<toml::Spanned<String>>,
    #[serde(default)]
    deny_commands: Vec<toml::Spanned<String>>,
    #[serde(default)]
    ask_commands: Vec<toml::Spanned<String>>,
    #[serde(default)]
    checks: Vec<toml::Spanned<String>>,
    #[serde(default)]
    network: Vec<toml::Spanned<String>>,
    #[serde(default)]
    budgets: RawBudgets,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBudgets {
    steps_per_turn: Option<toml::Spanned<u32>>,
    tokens_per_session: Option<toml::Spanned<u64>>,
    minutes_per_session: Option<toml::Spanned<u64>>,
}

/// An `AGENTS.md` at `file` (workspace-relative): its prose and its policy block.
fn parse_project(file: &str, text: &str) -> Parsed {
    let dir = file.rsplit_once('/').map_or(String::new(), |(dir, _)| dir.to_string());
    let block = match split_block(text) {
        Ok(split) => split,
        Err((line, why)) => return Parsed::problem(file, line, RuleOrigin::Project, why),
    };
    let mut parsed = match &block.toml {
        Some((toml, first_line)) => parse_toml(file, toml, *first_line, RuleOrigin::Project, &dir),
        None => Parsed::default(),
    };
    let prose = block.prose.trim();
    if !prose.is_empty() {
        parsed.policy.prose.push(ProjectProse { file: file.into(), dir, text: prose.to_string() });
    }
    parsed
}

/// The user's `policy.toml`: the same keys but `network` (the user sets the network with
/// `ctrl+n`).
fn parse_user(file: &str, text: &str) -> Parsed {
    let parsed = parse_toml(file, text, 1, RuleOrigin::User, "");
    if let Some(rule) = parsed.policy.network.first() {
        return Parsed::problem(
            file,
            rule.source.line,
            RuleOrigin::User,
            "`network` can't be set in your policy file: choose hosts with ctrl+n",
        );
    }
    parsed
}

/// A file split into its prose and its policy block.
struct Split {
    prose: String,
    /// The block's TOML and the file line its first line is on.
    toml: Option<(String, u32)>,
}

/// Finds the one fenced `nixxa-policy` block (``` or ~~~, at least three, at the start of a
/// line). The error is the line and why.
fn split_block(text: &str) -> Result<Split, (u32, String)> {
    let mut prose = String::new();
    let mut toml: Option<(String, u32)> = None;
    let mut open: Option<(String, u32, String)> = None;
    for (n, line) in text.lines().enumerate() {
        let number = n as u32 + 1;
        match &mut open {
            Some((fence, start, body)) => {
                if line.trim_end() == fence {
                    if toml.is_some() {
                        return Err((*start, format!("a second `{BLOCK}` block: put every rule in one")));
                    }
                    toml = Some((std::mem::take(body), *start + 1));
                    open = None;
                } else {
                    body.push_str(line);
                    body.push('\n');
                }
            }
            None => {
                let fence: String = line.chars().take_while(|&c| c == '`' || c == '~').collect();
                let info = line[fence.len()..].trim();
                let same = fence.chars().all(|c| fence.starts_with(c));
                if fence.len() >= 3 && same && info.split_whitespace().next() == Some(BLOCK) {
                    open = Some((fence, number, String::new()));
                } else {
                    prose.push_str(line);
                    prose.push('\n');
                }
            }
        }
    }
    if let Some((_, start, _)) = open {
        return Err((start, format!("the `{BLOCK}` block isn't closed")));
    }
    Ok(Split { prose, toml })
}

/// Parses policy TOML whose first line is `first_line` of `file`. Path globs get `dir/` in
/// front (a nested `AGENTS.md`).
fn parse_toml(file: &str, toml: &str, first_line: u32, origin: RuleOrigin, dir: &str) -> Parsed {
    let line_of = |offset: usize| first_line + toml[..offset.min(toml.len())].matches('\n').count() as u32;
    let raw: Raw = match toml::from_str(toml) {
        Ok(raw) => raw,
        Err(err) => {
            let line = err.span().map_or(first_line, |span| line_of(span.start));
            let message = err.message().trim().to_string();
            return Parsed::problem(file, line, origin, format!("the policy can't be read: {message}"));
        }
    };
    let mut policy = ProjectPolicy::default();
    let mut problems = Vec::new();
    let source = |line: u32| RuleSource { file: file.into(), line, origin };
    let rules = |list: Vec<toml::Spanned<String>>, kind: Kind, problems: &mut Vec<PolicyProblem>| -> Vec<Rule> {
        let mut out = Vec::new();
        for item in list {
            let line = line_of(item.span().start);
            let pattern = item.into_inner();
            match check(&pattern, kind, dir) {
                Ok(pattern) => out.push(Rule { pattern, source: source(line) }),
                Err(why) => problems.push(PolicyProblem { source: source(line), message: why }),
            }
        }
        out
    };
    policy.protect = rules(raw.protect, Kind::Path, &mut problems);
    policy.ask_read = rules(raw.ask_read, Kind::Path, &mut problems);
    policy.deny_commands = rules(raw.deny_commands, Kind::Command, &mut problems);
    policy.ask_commands = rules(raw.ask_commands, Kind::Command, &mut problems);
    policy.checks = rules(raw.checks, Kind::Command, &mut problems);
    policy.network = rules(raw.network, Kind::Host, &mut problems);
    let limit = |value: Option<toml::Spanned<u64>>, problems: &mut Vec<PolicyProblem>| -> Option<Limit<u64>> {
        let value = value?;
        let line = line_of(value.span().start);
        match *value.get_ref() {
            0 => {
                problems.push(PolicyProblem { source: source(line), message: "a budget must be more than 0".into() });
                None
            }
            n => Some(Limit { value: n, line, origin }),
        }
    };
    let steps = raw.budgets.steps_per_turn.map(|s| toml::Spanned::new(s.span(), u64::from(*s.get_ref())));
    policy.budgets = Budgets {
        steps_per_turn: limit(steps, &mut problems).map(|l| Limit {
            value: u32::try_from(l.value).unwrap_or(u32::MAX),
            line: l.line,
            origin,
        }),
        tokens_per_session: limit(raw.budgets.tokens_per_session, &mut problems),
        minutes_per_session: limit(raw.budgets.minutes_per_session, &mut problems),
    };
    policy.problems = problems;
    Parsed { policy }
}

#[derive(Clone, Copy)]
enum Kind {
    Path,
    Command,
    Host,
}

/// A checked pattern (a nested file's path glob with its directory in front), or why not.
fn check(pattern: &str, kind: Kind, dir: &str) -> Result<String, String> {
    let pattern = pattern.trim();
    if pattern.is_empty() {
        return Err("an empty pattern".into());
    }
    match kind {
        Kind::Path => {
            if pattern.starts_with('/') || pattern.starts_with('\\') || pattern.contains(':') {
                return Err(format!("`{pattern}` must be relative to the directory of its file"));
            }
            if pattern.split(['/', '\\']).any(|part| part == "..") {
                return Err(format!("`{pattern}` can't leave the directory of its file (`..`)"));
            }
            Ok(if dir.is_empty() { pattern.to_string() } else { format!("{dir}/{pattern}") })
        }
        Kind::Command => Ok(pattern.to_string()),
        Kind::Host => {
            DomainRule::parse(pattern).map(|rule| rule.to_string()).map_err(|why| format!("`{pattern}`: {why}"))
        }
    }
}

/// The user's policy file, if the platform has a config directory.
pub(crate) fn user_file() -> Option<PathBuf> {
    crate::paths::policy_path()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nixxa-policy-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    const AGENTS: &str = "# Project\n\nUse tabs.\n\n```nixxa-policy\nprotect = [\"migrations/**\"]\ndeny_commands = [\"git push\"]\nnetwork = [\"registry.npmjs.org\"]\nchecks = [\"cargo test\"]\n\n[budgets]\nsteps_per_turn = 40\n```\n\nThanks.\n";

    #[test]
    fn prose_and_rules_with_where_each_came_from() {
        let parsed = parse_project("AGENTS.md", AGENTS).policy;
        assert!(parsed.problems.is_empty(), "{:?}", parsed.problems);
        assert_eq!(parsed.protect[0].pattern, "migrations/**");
        assert_eq!(parsed.protect[0].source.line, 6, "the line in the file, not in the block");
        assert_eq!(parsed.deny_commands[0].source.line, 7);
        assert_eq!(parsed.network[0].pattern, "registry.npmjs.org");
        assert_eq!(parsed.budgets.steps_per_turn.map(|l| (l.value, l.line)), Some((40, 12)));
        assert_eq!(parsed.prose.len(), 1);
        assert!(parsed.prose[0].text.contains("Use tabs.") && parsed.prose[0].text.contains("Thanks."));
        assert!(!parsed.prose[0].text.contains("protect"), "the block isn't prose");
    }

    #[test]
    fn a_nested_files_globs_are_relative_to_its_directory() {
        let parsed =
            parse_project("db/AGENTS.md", "```nixxa-policy\nprotect = [\"migrations/**\"]\n```\nDB notes.\n").policy;
        assert_eq!(parsed.protect[0].pattern, "db/migrations/**");
        assert_eq!(parsed.prose[0].dir, "db");
    }

    #[test]
    fn a_broken_policy_is_a_problem_with_its_line_never_ignored() {
        for (text, line, words) in [
            ("```nixxa-policy\nmode = \"auto\"\n```\n", 2, "unknown field"),
            ("```nixxa-policy\nprotect = \"x\"\n```\n", 2, "can't be read"),
            ("```nixxa-policy\nprotect = [\"/etc/**\"]\n```\n", 2, "must be relative"),
            ("```nixxa-policy\nprotect = [\"../up/**\"]\n```\n", 2, "can't leave"),
            ("```nixxa-policy\nnetwork = [\"*.com\"]\n```\n", 2, "top-level"),
            ("```nixxa-policy\n[budgets]\nsteps_per_turn = 0\n```\n", 3, "more than 0"),
            ("text\n```nixxa-policy\nprotect = []\n", 2, "isn't closed"),
            ("```nixxa-policy\n```\n```nixxa-policy\n```\n", 3, "a second"),
            ("```nixxa-policy\n[budgets]\nhours = 1\n```\n", 3, "unknown field"),
        ] {
            let parsed = parse_project("AGENTS.md", text).policy;
            assert_eq!(parsed.problems.len(), 1, "{text}: {:?}", parsed.problems);
            let problem = &parsed.problems[0];
            assert!(problem.message.contains(words), "{text}: {problem:?}");
            assert_eq!(problem.source.line, line, "{text}: {problem:?}");
        }
        // Other fences are prose.
        let parsed = parse_project("AGENTS.md", "```toml\nprotect = [\"x\"]\n```\n").policy;
        assert!(parsed.protect.is_empty() && parsed.problems.is_empty());
    }

    fn rule_patterns(rules: &[Rule]) -> Vec<&str> {
        rules.iter().map(|r| r.pattern.as_str()).collect()
    }

    #[test]
    fn layered_the_strictest_wins_and_a_project_cant_loosen_anything() {
        let sources = Sources {
            user: Some((
                "policy.toml".into(),
                Ok("protect = [\"secrets/**\"]\ndeny_commands = [\"rm -rf /\"]\n[budgets]\nsteps_per_turn = 20\n".into()),
            )),
            project: vec![(
                "AGENTS.md".into(),
                Ok("```nixxa-policy\nprotect = []\nnetwork = [\"evil.example\"]\n[budgets]\nsteps_per_turn = 500\ntokens_per_session = 9\n```\n".into()),
            )],
            notes: Vec::new(),
        };
        let policy = sources.policy();
        // It can't drop a user rule.
        assert_eq!(rule_patterns(&policy.protect), ["secrets/**"]);
        assert_eq!(rule_patterns(&policy.deny_commands), ["rm -rf /"]);
        // It can't raise a budget, but it can lower one.
        assert_eq!(policy.budgets.steps_per_turn.map(|l| (l.value, l.origin)), Some((20, RuleOrigin::User)));
        assert_eq!(policy.budgets.tokens_per_session.map(|l| l.value), Some(9));
        // Its network is a request (the controller and the harness never apply it).
        assert_eq!(rule_patterns(&policy.network), ["evil.example"]);
        assert!(policy.problems.is_empty());
        // There is no key that turns Auto on: one is an unknown key, so everything asks.
        let auto = Sources {
            project: vec![("AGENTS.md".into(), Ok("```nixxa-policy\nauto = true\n```\n".into()))],
            ..Sources::default()
        };
        assert!(auto.policy().is_broken());
        // The user's file can't set the network.
        let user =
            Sources { user: Some(("policy.toml".into(), Ok("network = [\"x.org\"]\n".into()))), ..Sources::default() };
        assert!(user.policy().problems[0].message.contains("ctrl+n"));
    }

    #[test]
    fn files_are_found_nearest_first_and_never_through_links() {
        let root = scratch("find");
        std::fs::write(root.join("AGENTS.md"), AGENTS).unwrap();
        std::fs::create_dir_all(root.join("db")).unwrap();
        std::fs::write(root.join("db/AGENTS.md"), "DB rules.\n").unwrap();
        std::fs::create_dir_all(root.join("node_modules/x")).unwrap();
        std::fs::write(root.join("node_modules/x/AGENTS.md"), "no").unwrap();
        let sources = Sources::read(Some(&root), None);
        let files: Vec<&str> = sources.project.iter().map(|(f, _)| f.as_str()).collect();
        assert_eq!(files, ["AGENTS.md", "db/AGENTS.md"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_agents_md_is_never_read() {
        let root = scratch("link");
        let outside = scratch("outside");
        std::fs::write(outside.join("id_ed25519"), "-----BEGIN OPENSSH PRIVATE KEY-----\nsecret\n").unwrap();
        std::os::unix::fs::symlink(outside.join("id_ed25519"), root.join("AGENTS.md")).unwrap();
        // A directory on the way that is a link.
        std::fs::create_dir_all(outside.join("d")).unwrap();
        std::fs::write(outside.join("d/AGENTS.md"), "secret too\n").unwrap();
        std::os::unix::fs::symlink(outside.join("d"), root.join("sub")).unwrap();
        let sources = Sources::read(Some(&root), None);
        assert_eq!(sources.project.len(), 1, "the linked directory isn't searched: {:?}", sources.project);
        let (_, read) = &sources.project[0];
        assert!(read.as_ref().is_err_and(|why| why.contains("symbolic link")), "{read:?}");
        let policy = sources.policy();
        assert!(policy.prose.is_empty());
        assert!(policy.is_broken(), "fail towards strict");
        assert!(!format!("{policy:?}").contains("secret"));
        assert!(read_safely(&root, "sub/AGENTS.md").is_err(), "nor through a linked directory");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn an_oversized_or_binary_file_is_a_problem_and_text_is_sanitised() {
        let root = scratch("size");
        std::fs::write(root.join("AGENTS.md"), "x".repeat(MAX_FILE as usize + 1)).unwrap();
        let read = &Sources::read(Some(&root), None).project[0].1;
        assert!(read.as_ref().is_err_and(|why| why.contains("too large")), "{read:?}");
        std::fs::write(root.join("AGENTS.md"), [0xff, 0xfe, 0x00]).unwrap();
        assert!(Sources::read(Some(&root), None).project[0].1.is_err());
        std::fs::write(root.join("AGENTS.md"), "a\u{1b}[2Jb\u{202E}c\r\nd\u{7}").unwrap();
        assert_eq!(Sources::read(Some(&root), None).project[0].1.as_deref(), Ok("a[2Jbc\nd"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_changed_file_reads_differently() {
        let root = scratch("change");
        std::fs::write(root.join("AGENTS.md"), AGENTS).unwrap();
        let before = Sources::read(Some(&root), None);
        assert_eq!(before, Sources::read(Some(&root), None));
        std::fs::write(root.join("AGENTS.md"), AGENTS.replace("migrations", "schema")).unwrap();
        assert_ne!(before, Sources::read(Some(&root), None));
        let _ = std::fs::remove_dir_all(&root);
    }
}
