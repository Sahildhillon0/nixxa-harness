//! Patches through the daemon, and checkpoints over the mounted workspace, against an agent
//! that runs commands inside the sandbox to attack the host-side snapshot and restore.

#![cfg(target_os = "linux")]

mod common;

use common::{Scratch, is_failed, next_id, run, sh_out};
use nixxa_contracts::{Edit, Outcome, Sandbox, SandboxAction};

/// `<base>/outside` (a host directory the sandbox can't see but a link can name) with a secret.
fn outside(scratch: &Scratch) -> std::path::PathBuf {
    let dir = scratch.base.join("outside");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("id_ed25519"), "SECRET-KEY-MATERIAL\n").unwrap();
    dir
}

fn store_blobs(scratch: &Scratch) -> String {
    let mut all = String::new();
    let mut stack = vec![scratch.base.clone()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            let meta = std::fs::symlink_metadata(&path).unwrap();
            if meta.is_dir() {
                stack.push(path);
            } else if meta.is_file() && path.components().any(|c| c.as_os_str() == "objects") {
                all.push_str(&String::from_utf8_lossy(&std::fs::read(&path).unwrap()));
            }
        }
    }
    all
}

#[tokio::test]
async fn a_patch_goes_through_the_daemon_and_never_through_a_link() {
    let scratch = Scratch::new("undo-patch");
    let Some(sb) = common::sandbox(&scratch.workspace()) else { return };
    let secret = outside(&scratch);
    sh_out(
        &sb,
        &format!(
            "printf 'fn a() {{}}\\n' > lib.rs && ln -s {} host-link && ln -s lib.rs inner-link",
            secret.join("id_ed25519").display()
        ),
    )
    .await;
    let Outcome::File { hash, .. } = run(&sb, SandboxAction::ReadFile { path: "lib.rs".into() }).await.unwrap().outcome
    else {
        panic!("a read carries the hash")
    };
    let edit = |path: &str, hash: &str| SandboxAction::Patch {
        path: path.into(),
        edits: vec![Edit { search: "fn a".into(), replace: "fn b".into() }],
        expected_hash: hash.into(),
    };
    for link in ["host-link", "inner-link"] {
        assert!(is_failed(&run(&sb, edit(link, &hash)).await.unwrap()), "{link}");
    }
    let done = run(&sb, edit("lib.rs", &hash)).await.unwrap();
    assert!(matches!(&done.outcome, Outcome::Patched { diff, .. } if diff.contains("+fn b() {}")), "{done:?}");
    // The old hash is now stale.
    let stale = run(&sb, edit("lib.rs", &hash)).await.unwrap();
    assert!(matches!(&stale.outcome, Outcome::Failed { message } if message.contains("hash is now")), "{stale:?}");
    assert_eq!(std::fs::read_to_string(secret.join("id_ed25519")).unwrap(), "SECRET-KEY-MATERIAL\n");
    let _ = next_id();
}

#[tokio::test]
async fn links_the_agent_makes_to_host_files_never_enter_the_store() {
    let scratch = Scratch::new("undo-links");
    let Some(sb) = common::sandbox(&scratch.workspace()) else { return };
    let secret = outside(&scratch);
    // Inside the sandbox the link points nowhere; on the host it would name the key.
    sh_out(
        &sb,
        &format!(
            "ln -s {} key && ln -s {} dir && echo ok > ok.txt",
            secret.join("id_ed25519").display(),
            secret.display()
        ),
    )
    .await;
    let before = sb.checkpoint().await.unwrap();
    assert!(!store_blobs(&scratch).contains("SECRET"), "a link's host target must never be read");
    sh_out(&sb, "rm key dir").await;
    let undone = sb.restore(&before).await.unwrap();
    assert!(undone.changes.iter().any(|c| c.path == "key"));
    assert!(!format!("{undone:?}").contains("SECRET"));
    let link = std::fs::read_link(scratch.workspace().join("key")).unwrap();
    assert_eq!(link, secret.join("id_ed25519"), "the link comes back as a link");
}

#[tokio::test]
async fn undo_after_the_agent_swaps_a_directory_for_a_link_to_the_host() {
    let scratch = Scratch::new("undo-swap");
    let Some(sb) = common::sandbox(&scratch.workspace()) else { return };
    let secret = outside(&scratch);
    sh_out(&sb, "mkdir -p src/deep && echo 'fn main() {}' > src/main.rs && echo inner > src/deep/x.txt").await;
    let before = sb.checkpoint().await.unwrap();
    sh_out(&sb, &format!("rm -rf src && ln -s {} src", secret.display())).await;
    sb.restore(&before).await.unwrap();
    let names: Vec<_> = std::fs::read_dir(&secret).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert_eq!(names, vec![std::ffi::OsString::from("id_ed25519")], "nothing was written on the host");
    assert_eq!(sh_out(&sb, "test -d src && ! test -L src && cat src/deep/x.txt").await.trim(), "inner");
}

#[tokio::test]
async fn gitignore_cannot_hide_a_file_the_agent_deletes() {
    let scratch = Scratch::new("undo-gitignore");
    let Some(sb) = common::sandbox(&scratch.workspace()) else { return };
    sh_out(&sb, "mkdir src && echo precious > src/keep.rs").await;
    let first = sb.checkpoint().await.unwrap();
    // The agent ignores src/, then deletes it in a later action.
    sh_out(&sb, "printf 'src/\\n*\\n' > .gitignore").await;
    let second = sb.checkpoint().await.unwrap();
    sh_out(&sb, "rm -rf src").await;
    let diff = sb.diff(&second, None).await.unwrap();
    assert!(diff.changes.iter().any(|c| c.path == "src/keep.rs" && c.after.is_none()), "{diff:?}");
    sb.restore(&second).await.unwrap();
    assert_eq!(sh_out(&sb, "cat src/keep.rs").await.trim(), "precious");
    sb.restore(&first).await.unwrap();
    assert_eq!(sh_out(&sb, "test -e .gitignore || echo gone").await.trim(), "gone");
}

#[tokio::test]
async fn a_fifo_the_agent_makes_does_not_hang_a_checkpoint() {
    let scratch = Scratch::new("undo-fifo");
    let Some(sb) = common::sandbox(&scratch.workspace()) else { return };
    sh_out(&sb, "mkfifo pipe && echo ok > ok.txt").await;
    let taken = tokio::time::timeout(std::time::Duration::from_secs(10), sb.checkpoint()).await;
    let id = taken.expect("a FIFO must not hang the snapshot").unwrap();
    let diff = sb.diff(&id, None).await.unwrap();
    assert_eq!(diff.not_covered, vec!["pipe (a special file)".to_string()]);
}

#[tokio::test]
async fn a_deep_tree_the_agent_builds_neither_crashes_nor_blocks_the_next_action_or_undo() {
    let scratch = Scratch::new("undo-deep");
    let Some(sb) = common::sandbox(&scratch.workspace()) else { return };
    sh_out(&sb, "echo keep > keep.txt").await;
    let before = sb.checkpoint().await.unwrap();
    // Relative names only, so no path limit applies; past the snapshot's depth limit.
    sh_out(&sb, "i=0; while [ $i -lt 1500 ]; do mkdir d && cd d || exit 1; i=$((i+1)); done; echo deep").await;
    let refused = sb.checkpoint().await;
    assert!(
        matches!(&refused, Err(nixxa_contracts::SandboxError::Other(why)) if why.contains("nested more than")),
        "{refused:?}"
    );
    // The next action still runs.
    assert_eq!(sh_out(&sb, "echo still here").await.trim(), "still here");
    let diff = sb.diff(&before, None).await.unwrap();
    assert!(diff.changes.iter().any(|c| c.path == "d" && c.before.is_none()));
    sb.restore(&before).await.unwrap();
    assert_eq!(sh_out(&sb, "ls -A").await.trim(), "keep.txt", "the deep tree is gone");
}
