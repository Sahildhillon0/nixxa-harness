//! Protected paths (#5): what the project policy keeps from being changed.
//!
//! Every backend refuses a file action (`WriteFile`, `Patch`) on a protected path, checking the
//! path as given and as it resolves through symbolic links inside the workspace. A restore
//! leaves protected paths as they are. The bubblewrap backend also mounts each protected path
//! that exists read-only, so no command can change it either ([`plan`] works out which).

use std::path::{Path, PathBuf};

use nixxa_contracts::{Protection, ProtectionKind, glob_covers, glob_matches};

/// The most paths kept read-only. Past it, the rest are only refused to file actions (and held
/// by the controller's gate).
pub(crate) const MAX_KEPT: usize = 256;

/// The most workspace entries looked at for matches.
pub(crate) const MAX_WALKED: usize = 200_000;

/// The protected-path globs in force (workspace-relative).
#[derive(Debug, Clone, Default)]
pub(crate) struct Rules(Vec<String>);

impl Rules {
    pub(crate) fn new(rules: Vec<String>) -> Self {
        Self(rules)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The rule covering the workspace-relative `path`, written as the agent wrote it.
    pub(crate) fn covering(&self, path: &str) -> Option<&str> {
        let path = normal(path);
        self.0.iter().map(String::as_str).find(|rule| glob_covers(rule, &path))
    }

    /// Why a file action on `path` must not run, if it mustn't: the path as written, or as it
    /// resolves under `root` (canonical) through symbolic links, is protected.
    pub(crate) fn refuse(&self, root: &Path, path: &str) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        let rule = self.covering(path).or_else(|| resolved(root, path).and_then(|real| self.covering(&real)))?;
        Some(format!("`{path}` is protected by the project policy (`{rule}`), so it can't be changed"))
    }
}

/// `path` with `\` as `/`, and `.`, empty and `..` segments resolved within the workspace.
fn normal(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split(['/', '\\']) {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            part => parts.push(part),
        }
    }
    parts.join("/")
}

/// Where `path` really is under `root`, symbolic links followed (as far as it exists), as a
/// workspace-relative path; `None` when it resolves outside.
fn resolved(root: &Path, path: &str) -> Option<String> {
    let mut probe = root.join(normal(path));
    let mut rest: Vec<std::ffi::OsString> = Vec::new();
    let real = loop {
        match probe.canonicalize() {
            Ok(real) => break real,
            Err(_) => {
                rest.push(probe.file_name()?.to_os_string());
                probe = probe.parent()?.to_path_buf();
            }
        }
    };
    let mut real = real.strip_prefix(root).ok()?.to_path_buf();
    real.extend(rest.iter().rev());
    Some(real.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect::<Vec<_>>().join("/"))
}

/// What a set of rules covers in a workspace, worked out by walking it.
#[derive(Debug, Default)]
pub(crate) struct Plan {
    /// The paths to keep read-only: workspace-relative, and on the host.
    pub(crate) kept: Vec<(String, PathBuf)>,
    /// What to tell the controller.
    pub(crate) protection: Protection,
}

/// Walks `root` (canonical) for the entries `rules` match, without following symbolic links
/// and without going into a matched directory. Blocking.
pub(crate) fn plan(root: &Path, rules: &[String], how: ProtectionKind) -> Plan {
    let mut plan = Plan { protection: Protection { how, ..Protection::default() }, ..Plan::default() };
    if rules.is_empty() {
        return plan;
    }
    let mut stack: Vec<(PathBuf, String)> = vec![(root.to_path_buf(), String::new())];
    let mut walked = 0usize;
    let mut linked: Vec<String> = Vec::new();
    while let Some((dir, rel)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            walked += 1;
            if walked > MAX_WALKED {
                plan.protection.gaps.push(format!(
                    "the workspace has more than {MAX_WALKED} entries: protected paths past them are only refused to \
                     file actions"
                ));
                stack.clear();
                break;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = if rel.is_empty() { name } else { format!("{rel}/{name}") };
            let Ok(meta) = std::fs::symlink_metadata(entry.path()) else { continue };
            if rules.iter().any(|rule| glob_matches(rule, &path)) {
                if meta.file_type().is_symlink() {
                    plan.protection.gaps.push(format!(
                        "`{path}` is a symbolic link: it can't be kept read-only, only refused to file actions"
                    ));
                    continue;
                }
                hard_links(&entry.path(), &path, &meta, &mut linked);
                plan.kept.push((path, entry.path()));
            } else if meta.is_dir() {
                stack.push((entry.path(), path));
            }
        }
    }
    plan.kept.sort();
    if plan.kept.len() > MAX_KEPT {
        let over = plan.kept.split_off(MAX_KEPT);
        plan.protection.gaps.push(format!(
            "{} more protected paths than the {MAX_KEPT} that can be kept read-only (from `{}`): those are only \
             refused to file actions",
            over.len(),
            over[0].0
        ));
    }
    for path in linked {
        plan.protection
            .gaps
            .push(format!("`{path}` has another hard link: changing the file through that other name changes it too"));
    }
    plan.protection.kept = plan.kept.iter().map(|(rel, _)| rel.clone()).collect();
    plan.protection.unmatched =
        rules.iter().filter(|rule| !plan.kept.iter().any(|(path, _)| glob_matches(rule, path))).cloned().collect();
    plan
}

/// Notes the files at or under `host` that have another hard link (a name the mount doesn't
/// cover). Looks at no more than a few thousand entries under a directory.
#[cfg(unix)]
fn hard_links(host: &Path, rel: &str, meta: &std::fs::Metadata, linked: &mut Vec<String>) {
    use std::os::unix::fs::MetadataExt;
    if meta.is_file() {
        if meta.nlink() > 1 {
            linked.push(rel.to_string());
        }
        return;
    }
    let mut stack = vec![(host.to_path_buf(), rel.to_string())];
    let mut seen = 0;
    while let Some((dir, rel)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            seen += 1;
            if seen > 4096 {
                return;
            }
            let path = format!("{rel}/{}", entry.file_name().to_string_lossy());
            let Ok(meta) = std::fs::symlink_metadata(entry.path()) else { continue };
            if meta.is_dir() {
                stack.push((entry.path(), path));
            } else if meta.is_file() && meta.nlink() > 1 {
                linked.push(path);
            }
        }
    }
}

#[cfg(not(unix))]
fn hard_links(_host: &Path, _rel: &str, _meta: &std::fs::Metadata, _linked: &mut Vec<String>) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nixxa-protect-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    #[test]
    fn file_actions_are_refused_however_the_path_is_spelt() {
        let root = scratch("refuse");
        std::fs::create_dir_all(root.join("migrations")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        let rules = Rules::new(vec!["migrations/**".into()]);
        for path in
            ["migrations/1.sql", "./migrations//1.sql", "src/../migrations/1.sql", "MIGRATIONS/1.sql", "migrations"]
        {
            assert!(rules.refuse(&root, path).is_some(), "{path}");
        }
        assert!(rules.refuse(&root, "src/main.rs").is_none());
        // A symbolic link inside the workspace to the protected directory.
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(root.join("migrations"), root.join("src/m")).unwrap();
            let why = rules.refuse(&root, "src/m/1.sql").expect("through the link");
            assert!(why.contains("`migrations/**`"), "{why}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_plan_keeps_what_exists_and_says_what_it_cant() {
        let root = scratch("plan");
        std::fs::create_dir_all(root.join("migrations/old")).unwrap();
        std::fs::write(root.join("migrations/old/1.sql"), "x").unwrap();
        std::fs::create_dir_all(root.join("web")).unwrap();
        std::fs::write(root.join("web/yarn.lock"), "x").unwrap();
        std::fs::write(root.join("Cargo.lock"), "x").unwrap();
        std::fs::write(root.join("main.rs"), "x").unwrap();
        let rules = vec!["migrations/**".into(), "**/*.lock".into(), "generated/**".into()];
        let plan = plan(&root, &rules, ProtectionKind::ReadOnlyMounts);
        assert_eq!(plan.protection.kept, ["Cargo.lock", "migrations", "web/yarn.lock"], "a matched directory whole");
        assert_eq!(plan.protection.unmatched, ["generated/**"]);
        assert!(plan.protection.gaps.is_empty(), "{:?}", plan.protection.gaps);
        #[cfg(unix)]
        {
            std::fs::hard_link(root.join("migrations/old/1.sql"), root.join("other.sql")).unwrap();
            let plan = super::plan(&root, &rules, ProtectionKind::ReadOnlyMounts);
            assert!(plan.protection.gaps.iter().any(|g| g.contains("`migrations/old/1.sql` has another hard link")));
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}
