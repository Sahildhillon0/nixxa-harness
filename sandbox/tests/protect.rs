//! Protected paths in the real sandbox (#5, D-080): every way round the read-only mounts a
//! command could try, from the agent's commands, file actions and the user's terminal, leaves a
//! protected path as it was on the host.

#![cfg(target_os = "linux")]

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use common::{Scratch, run, sandbox, sh, sh_out};
use futures::StreamExt;
use nixxa_contracts::{Outcome, ProtectionKind, Sandbox, SandboxAction, SandboxEvent, TerminalEvent, TerminalSize};
use nixxa_sandbox::bwrap::BwrapSandbox;

const SQL: &str = "create table t (id int);\n";

/// A workspace with `migrations/001.sql`, `db/migrations/001.sql` and `src/main.rs`.
fn tree(scratch: &Scratch) -> std::path::PathBuf {
    let ws = scratch.workspace();
    for dir in ["migrations", "db/migrations", "src"] {
        std::fs::create_dir_all(ws.join(dir)).unwrap();
    }
    std::fs::write(ws.join("migrations/001.sql"), SQL).unwrap();
    std::fs::write(ws.join("db/migrations/001.sql"), SQL).unwrap();
    std::fs::write(ws.join("src/main.rs"), "fn main() {}\n").unwrap();
    ws
}

async fn protected(ws: &Path, rules: &[&str]) -> Option<BwrapSandbox> {
    let sb = sandbox(ws)?;
    let protection = sb.protect(rules.iter().map(|r| r.to_string()).collect()).await.expect("protect");
    assert_eq!(protection.how, ProtectionKind::ReadOnlyMounts);
    Some(sb)
}

/// The protected files are on the host exactly as they were.
fn unchanged(ws: &Path) {
    for file in ["migrations/001.sql", "db/migrations/001.sql"] {
        assert_eq!(std::fs::read_to_string(ws.join(file)).unwrap(), SQL, "{file} changed on the host");
    }
    let names = |dir: &str| {
        let mut names: Vec<String> =
            std::fs::read_dir(ws.join(dir)).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into()).collect();
        names.sort();
        names
    };
    assert_eq!(names("migrations"), ["001.sql"], "nothing was added to migrations/");
    assert_eq!(names("db/migrations"), ["001.sql"]);
}

#[tokio::test]
async fn commands_cannot_remove_move_or_rewrite_a_protected_path() {
    let scratch = Scratch::new("protect-commands");
    let ws = tree(&scratch);
    let Some(sb) = protected(&ws, &["migrations/**", "db/migrations/**"]).await else { return };
    let attempts = [
        "rm -rf migrations",
        "rm -f migrations/001.sql",
        "mv migrations gone",
        "mv migrations/001.sql gone.sql",
        // A parent of a protected path is pinned too.
        "mv db gone-db",
        "rm -rf db",
        "touch migrations/002.sql",
        "mkdir migrations/new",
        "echo x > migrations/001.sql",
        "echo x >> migrations/001.sql",
        "truncate -s 0 migrations/001.sql",
        "chmod 000 migrations/001.sql",
        "chmod 777 migrations",
        "touch -d 2000-01-01 migrations/001.sql",
        // Renamed over, file and directory.
        "echo x > other.sql && mv -f other.sql migrations/001.sql",
        "mkdir -p empty && mv -T empty migrations",
        "rm -rf db/migrations && mkdir db/migrations",
        // Hard links made now: across a mount, refused.
        "ln migrations/001.sql linked.sql && echo x > linked.sql",
        // Through a file descriptor reopened for writing.
        "exec 3<migrations/001.sql; echo x > /proc/self/fd/3",
        "python3 -c 'import os; os.rename(\"migrations\", \"m2\")'",
        "find . -name '*.sql' -exec sed -i s/table/view/ {} +",
    ];
    for attempt in attempts {
        sh(&sb, attempt).await;
        unchanged(&ws);
    }
    // A symbolic link made now points at a read-only file.
    let out = sh_out(&sb, "ln -s migrations/001.sql via && echo x > via; echo rc=$?").await;
    assert!(out.contains("rc=1") || out.contains("Read-only"), "{out}");
    unchanged(&ws);
    // The rest of the workspace is as writable as ever.
    let (code, _, _) = sh(&sb, "echo y > src/main.rs && mkdir -p db/seeds && touch db/seeds/1.sql").await;
    assert_eq!(code, Some(0));
    assert_eq!(std::fs::read_to_string(ws.join("src/main.rs")).unwrap(), "y\n");
}

#[tokio::test]
async fn file_actions_are_refused_through_links_and_odd_spellings() {
    let scratch = Scratch::new("protect-files");
    let ws = tree(&scratch);
    std::os::unix::fs::symlink("../migrations", ws.join("src/m")).unwrap();
    let Some(sb) = protected(&ws, &["migrations/**"]).await else { return };
    for path in ["migrations/001.sql", "./migrations//001.sql", "src/../migrations/001.sql", "src/m/001.sql"] {
        let write = SandboxAction::WriteFile { path: path.into(), content: "x".into() };
        let observation = run(&sb, write).await.expect("write");
        match &observation.outcome {
            Outcome::Failed { message } => assert!(
                message.contains("protected by the project policy") || message.contains("Read-only"),
                "{path}: {message}"
            ),
            other => panic!("{path}: {other:?}"),
        }
    }
    unchanged(&ws);
}

#[tokio::test]
async fn a_hard_link_made_before_is_a_gap_and_is_said_to_be() {
    let scratch = Scratch::new("protect-link");
    let ws = tree(&scratch);
    std::fs::hard_link(ws.join("migrations/001.sql"), ws.join("src/same.sql")).unwrap();
    let Some(sb) = sandbox(&ws) else { return };
    let protection = sb.protect(vec!["migrations/**".into()]).await.expect("protect");
    assert!(
        protection.gaps.iter().any(|gap| gap.contains("`migrations/001.sql` has another hard link")),
        "{protection:?}"
    );
}

#[tokio::test]
async fn the_users_terminal_is_bound_by_the_mounts_too() {
    let scratch = Scratch::new("protect-terminal");
    let ws = tree(&scratch);
    let Some(sb) = protected(&ws, &["migrations/**"]).await else { return };
    let mut open = sb.open_terminal(TerminalSize { cols: 100, rows: 30 }).await.expect("terminal");
    open.terminal
        .write(b"rm -rf migrations; echo x > migrations/001.sql; echo \"<<$((6*7))>>\"\r")
        .await
        .expect("type");
    let started = Instant::now();
    let mut seen = String::new();
    while !seen.contains("<<42>>") {
        let left = Duration::from_secs(15).saturating_sub(started.elapsed());
        match tokio::time::timeout(left, open.events.next()).await {
            Ok(Some(TerminalEvent::Output(bytes))) => seen.push_str(&String::from_utf8_lossy(&bytes)),
            other => panic!("the terminal: {other:?}; {seen}"),
        }
    }
    unchanged(&ws);
}

#[tokio::test]
async fn new_rules_restart_the_sandbox_and_apply() {
    let scratch = Scratch::new("protect-restart");
    let ws = tree(&scratch);
    let Some(sb) = sandbox(&ws) else { return };
    let mut events = sb.subscribe();
    let job = match run(
        &sb,
        SandboxAction::Exec { command: "sleep 300".into(), cwd: None, timeout_ms: None, background: true },
    )
    .await
    .expect("start")
    .outcome
    {
        Outcome::Started { job } => job,
        other => panic!("{other:?}"),
    };
    // Unprotected: writable.
    assert_eq!(sh(&sb, "echo -n '' >> migrations/001.sql").await.0, Some(0));
    sb.protect(vec!["migrations/**".into()]).await.expect("protect");
    // The running job ended with the daemon that ran it.
    let ended = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(event) = events.next().await {
            if matches!(&event, SandboxEvent::JobFinished { job: j, .. } if *j == job) {
                return event;
            }
        }
        panic!("no end");
    })
    .await
    .expect("the job's end");
    assert!(matches!(ended, SandboxEvent::JobFinished { exit_code: None, .. }), "{ended:?}");
    sh(&sb, "rm -rf migrations").await;
    unchanged(&ws);
    // Cleared: writable again, after another restart.
    sb.protect(Vec::new()).await.expect("clear");
    assert_eq!(sh(&sb, "touch migrations/002.sql").await.0, Some(0));
}

#[tokio::test]
async fn a_restore_leaves_protected_paths_as_they_are() {
    let scratch = Scratch::new("protect-undo");
    let ws = tree(&scratch);
    let Some(sb) = sandbox(&ws) else { return };
    let before = sb.checkpoint().await.expect("checkpoint");
    // Changed while unprotected, then protected.
    std::fs::write(ws.join("migrations/001.sql"), SQL).unwrap();
    std::fs::write(ws.join("migrations/002.sql"), SQL).unwrap();
    sb.protect(vec!["migrations/**".into()]).await.expect("protect");
    let undone = sb.restore(&before).await.expect("restore");
    assert!(ws.join("migrations/002.sql").exists(), "the restore left the protected directory as it is");
    assert!(undone.changes.iter().all(|c| !c.path.starts_with("migrations")), "{undone:?}");
    assert!(undone.not_covered.iter().any(|n| n.contains("migrations/** (protected")), "{undone:?}");
}
