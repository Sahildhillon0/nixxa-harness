//! The store against a hostile workspace: links and swaps, special files, the cap, what is
//! left out, and the speed of a checkpoint when little changed.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use nixxa_contracts::{CheckpointId, SandboxError};

use super::*;

/// `<tmp>/ws` (the workspace), `<tmp>/outside` (must never change or leak), `<tmp>/store`.
struct Scratch {
    base: PathBuf,
}

impl Scratch {
    fn new() -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let base = std::env::temp_dir().join(format!(
            "nixxa-checkpoint-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("ws")).unwrap();
        std::fs::create_dir_all(base.join("outside")).unwrap();
        std::fs::write(base.join("outside/id_ed25519"), "SECRET-KEY-MATERIAL\n").unwrap();
        Self { base: base.canonicalize().unwrap() }
    }

    fn ws(&self) -> PathBuf {
        self.base.join("ws")
    }

    fn at(&self, rel: &str) -> PathBuf {
        self.ws().join(rel)
    }

    #[cfg_attr(not(unix), allow(dead_code))]
    fn outside(&self) -> PathBuf {
        self.base.join("outside")
    }

    fn store(&self, cap: u64) -> Checkpoints {
        Checkpoints::new(&self.ws(), &CheckpointConfig { dir: self.base.join("store").join("run-1"), cap }).unwrap()
    }

    fn write(&self, rel: &str, content: &str) {
        let path = self.at(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.at(rel)).unwrap()
    }

    /// Every blob in the store, as text.
    fn blobs(&self) -> Vec<String> {
        fn walk(dir: &Path, out: &mut Vec<String>) {
            for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, out);
                } else {
                    out.push(String::from_utf8_lossy(&std::fs::read(&path).unwrap()).into_owned());
                }
            }
        }
        let mut out = Vec::new();
        walk(&self.base.join("store").join("run-1").join("objects"), &mut out);
        out
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            // Directories a test locked must be unlocked to be removed.
            let _ = std::process::Command::new("chmod").args(["-R", "u+rwx"]).arg(&self.base).status();
        }
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

fn find<'d>(diff: &'d WorkspaceDiff, path: &str) -> &'d FileChange {
    diff.changes.iter().find(|c| c.path == path).unwrap_or_else(|| panic!("{path} not in {diff:#?}"))
}

#[tokio::test]
async fn unchanged_workspaces_give_the_same_checkpoint_and_changes_a_new_one() {
    let s = Scratch::new();
    s.write("a.txt", "a\n");
    let store = s.store(DEFAULT_CAP);
    let one = store.checkpoint().await.unwrap();
    assert_eq!(store.checkpoint().await.unwrap(), one, "nothing changed");
    s.write("a.txt", "b\n");
    let two = store.checkpoint().await.unwrap();
    assert_ne!(two, one);
    let diff = store.diff(&one, Some(&two)).await.unwrap();
    assert!(find(&diff, "a.txt").diff.as_deref().unwrap().contains("+b"));
}

#[tokio::test]
async fn ids_of_another_store_are_not_this_ones() {
    let s = Scratch::new();
    let store = s.store(DEFAULT_CAP);
    store.checkpoint().await.unwrap();
    for id in ["other-run.0", "run-1.7", "run-1", "run-1.x", ""] {
        let id = CheckpointId::new(id);
        assert!(matches!(store.restore(&id).await, Err(SandboxError::NoCheckpoint(_))), "{id}");
    }
}

#[tokio::test]
async fn the_store_is_private_and_goes_with_the_sandbox() {
    let s = Scratch::new();
    let dir = s.base.join("store").join("run-1");
    let store = s.store(DEFAULT_CAP);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
    }
    drop(store);
    assert!(!dir.exists(), "the store is removed with the sandbox");
}

#[tokio::test]
async fn past_the_cap_a_checkpoint_is_refused_and_keeps_nothing() {
    let s = Scratch::new();
    s.write("small.txt", "1234");
    let store = s.store(10);
    let first = store.checkpoint().await.unwrap();
    s.write("big.txt", &"x".repeat(100));
    let refused = store.checkpoint().await;
    assert!(matches!(&refused, Err(SandboxError::LimitExceeded(why)) if why.contains("full")), "{refused:?}");
    assert_eq!(s.blobs(), vec!["1234".to_string()], "the refused checkpoint's blobs are gone");
    // The earlier checkpoint still restores.
    store.restore(&first).await.unwrap();
    assert!(!s.at("big.txt").exists());
}

#[tokio::test]
async fn gitignore_hides_nothing_and_build_output_is_left_out() {
    let s = Scratch::new();
    s.write(".gitignore", "src/\n*.rs\n");
    s.write("src/lib.rs", "pub fn keep() {}\n");
    s.write("target/debug/big", "build output\n");
    s.write("deep/node_modules/x/index.js", "dep\n");
    let store = s.store(DEFAULT_CAP);
    let before = store.checkpoint().await.unwrap();
    std::fs::remove_dir_all(s.at("src")).unwrap();
    s.write("target/debug/new", "rebuilt\n");
    let diff = store.diff(&before, None).await.unwrap();
    assert!(find(&diff, "src/lib.rs").after.is_none(), "an ignored path is still covered");
    assert_eq!(diff.not_covered, vec!["deep/node_modules/".to_string(), "target/".to_string()]);
    assert!(diff.changes.iter().all(|c| !c.path.starts_with("target")), "{diff:#?}");
    let undone = store.restore(&before).await.unwrap();
    assert!(!undone.not_covered.is_empty());
    assert_eq!(s.read("src/lib.rs"), "pub fn keep() {}\n");
    assert_eq!(s.read("target/debug/new"), "rebuilt\n", "what isn't covered is left as it is");
    assert!(!s.blobs().iter().any(|b| b.contains("build output") || b == "dep\n"), "left out of the store");
}

#[cfg(unix)]
mod unix_attacks {
    use std::os::unix::fs::{PermissionsExt, symlink};

    use nixxa_contracts::EntryState;

    use super::*;

    /// A FIFO (`mknodat` isn't on every Unix).
    fn mkfifo(path: &Path) {
        assert!(std::process::Command::new("mkfifo").arg(path).status().unwrap().success());
    }

    fn mode(path: &Path) -> u32 {
        std::fs::symlink_metadata(path).unwrap().permissions().mode() & 0o7777
    }

    #[tokio::test]
    async fn a_snapshot_keeps_a_link_as_a_link_and_never_reads_its_target() {
        let s = Scratch::new();
        symlink(s.outside().join("id_ed25519"), s.at("key")).unwrap();
        symlink(s.outside(), s.at("dir")).unwrap();
        s.write("ok.txt", "fine\n");
        let store = s.store(DEFAULT_CAP);
        let before = store.checkpoint().await.unwrap();
        assert!(!s.blobs().iter().any(|b| b.contains("SECRET")), "a link's target must never enter the store");
        // Swap the link for another; the diff names the targets, never their contents.
        std::fs::remove_file(s.at("key")).unwrap();
        symlink("/etc/passwd", s.at("key")).unwrap();
        let diff = store.diff(&before, None).await.unwrap();
        let key = find(&diff, "key");
        let outside = s.outside().join("id_ed25519").display().to_string();
        assert_eq!(key.before, Some(EntryState::Symlink { target: outside }));
        assert_eq!(key.diff, None);
        assert!(!format!("{diff:?}").contains("SECRET") && !format!("{diff:?}").contains("root:"));
        assert!(diff.changes.iter().all(|c| !c.path.starts_with("dir/")), "a linked directory isn't entered");
    }

    #[tokio::test]
    async fn special_files_are_never_opened() {
        let s = Scratch::new();
        mkfifo(&s.at("fifo"));
        let listener = std::os::unix::net::UnixListener::bind(s.at("sock")).unwrap();
        s.write("ok.txt", "fine\n");
        let store = s.store(DEFAULT_CAP);
        // A FIFO opened for reading with no writer would block forever.
        let taken = tokio::time::timeout(std::time::Duration::from_secs(10), store.checkpoint()).await;
        let id = taken.expect("a FIFO must not hang the snapshot").unwrap();
        let diff = store.diff(&id, None).await.unwrap();
        assert_eq!(diff.not_covered, vec!["fifo (a special file)".to_string(), "sock (a special file)".to_string()]);
        drop(listener);
    }

    #[tokio::test]
    async fn files_that_change_or_vanish_while_read_dont_break_a_snapshot() {
        let s = Scratch::new();
        for n in 0..200 {
            s.write(&format!("churn/{n}.txt"), "x\n");
        }
        let store = s.store(DEFAULT_CAP);
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let churn = {
            let (root, stop) = (s.at("churn"), stop.clone());
            std::thread::spawn(move || {
                let mut n = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    let path = root.join(format!("{}.txt", n % 200));
                    let _ = std::fs::remove_file(&path);
                    let _ = std::fs::write(&path, n.to_string());
                    let dir = root.join(format!("d{}", n % 7));
                    let _ = std::fs::create_dir(&dir).or_else(|_| std::fs::remove_dir(&dir));
                    n += 1;
                }
            })
        };
        for _ in 0..30 {
            store.checkpoint().await.expect("a snapshot copes with a changing tree");
        }
        stop.store(true, Ordering::Relaxed);
        churn.join().unwrap();
    }

    #[tokio::test]
    async fn restore_never_writes_through_a_directory_swapped_for_a_link() {
        let s = Scratch::new();
        s.write("src/main.rs", "fn main() {}\n");
        s.write("src/deep/mod.rs", "mod x;\n");
        let store = s.store(DEFAULT_CAP);
        let before = store.checkpoint().await.unwrap();
        // The agent replaces src/ with a link to a directory outside (on the host, `~`).
        std::fs::remove_dir_all(s.at("src")).unwrap();
        symlink(s.outside(), s.at("src")).unwrap();
        store.restore(&before).await.unwrap();
        let outside: Vec<_> = std::fs::read_dir(s.outside()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(outside, vec![std::ffi::OsString::from("id_ed25519")], "nothing was written outside");
        assert!(std::fs::symlink_metadata(s.at("src")).unwrap().is_dir(), "src is a real directory again");
        assert_eq!(s.read("src/deep/mod.rs"), "mod x;\n");
    }

    #[tokio::test]
    async fn restore_never_writes_through_a_file_swapped_for_a_link() {
        let s = Scratch::new();
        s.write("a.txt", "original\n");
        let store = s.store(DEFAULT_CAP);
        let before = store.checkpoint().await.unwrap();
        std::fs::remove_file(s.at("a.txt")).unwrap();
        symlink(s.outside().join("id_ed25519"), s.at("a.txt")).unwrap();
        store.restore(&before).await.unwrap();
        assert_eq!(std::fs::read_to_string(s.outside().join("id_ed25519")).unwrap(), "SECRET-KEY-MATERIAL\n");
        assert!(std::fs::symlink_metadata(s.at("a.txt")).unwrap().is_file());
        assert_eq!(s.read("a.txt"), "original\n");
    }

    #[tokio::test]
    async fn restore_replaces_files_instead_of_writing_into_them() {
        // A hard link the agent made to a workspace file must keep its contents: the file is
        // replaced, not written through.
        let s = Scratch::new();
        s.write("a.txt", "original\n");
        let store = s.store(DEFAULT_CAP);
        let before = store.checkpoint().await.unwrap();
        s.write("a.txt", "changed\n");
        std::fs::hard_link(s.at("a.txt"), s.outside().join("hard")).unwrap();
        store.restore(&before).await.unwrap();
        assert_eq!(s.read("a.txt"), "original\n");
        assert_eq!(std::fs::read_to_string(s.outside().join("hard")).unwrap(), "changed\n");
    }

    #[tokio::test]
    async fn the_directory_opener_refuses_links() {
        let s = Scratch::new();
        symlink(s.outside(), s.at("link")).unwrap();
        let root = Dir::open_root(&s.ws()).unwrap();
        assert!(root.open_dir(OsStr::new("link")).is_err(), "a link is never entered");
        assert!(root.open_file(OsStr::new("link")).is_err(), "a link is never read");
        assert!(open_or_make(&root, OsStr::new("link"), b"link").is_err(), "nor made a directory through");
    }

    #[tokio::test]
    async fn restore_brings_back_modes_and_locked_directories() {
        let s = Scratch::new();
        s.write("bin/run.sh", "echo hi\n");
        s.write("ro/locked.txt", "locked\n");
        std::fs::set_permissions(s.at("bin/run.sh"), PermissionsExt::from_mode(0o755)).unwrap();
        std::fs::set_permissions(s.at("ro"), PermissionsExt::from_mode(0o555)).unwrap();
        let store = s.store(DEFAULT_CAP);
        let before = store.checkpoint().await.unwrap();
        std::fs::set_permissions(s.at("bin/run.sh"), PermissionsExt::from_mode(0o600)).unwrap();
        std::fs::set_permissions(s.at("ro"), PermissionsExt::from_mode(0o755)).unwrap();
        std::fs::write(s.at("ro/new.txt"), "new\n").unwrap();
        std::fs::set_permissions(s.at("ro"), PermissionsExt::from_mode(0o000)).unwrap();
        let undone = store.restore(&before).await.unwrap();
        assert!(undone.changes.iter().any(|c| c.path == "bin/run.sh"));
        assert_eq!(mode(&s.at("bin/run.sh")), 0o755);
        assert_eq!(mode(&s.at("ro")), 0o555);
        assert!(!s.at("ro/new.txt").exists(), "a file made inside a locked directory is removed");
        assert_eq!(s.read("ro/locked.txt"), "locked\n");
    }

    #[tokio::test]
    async fn restore_removes_what_was_made_since_of_every_kind() {
        let s = Scratch::new();
        s.write("keep.txt", "keep\n");
        let store = s.store(DEFAULT_CAP);
        let before = store.checkpoint().await.unwrap();
        s.write("new/deep/file.txt", "new\n");
        symlink(s.outside(), s.at("new-link")).unwrap();
        mkfifo(&s.at("new-fifo"));
        // A name restore might pick for its temporary files, taken.
        s.write(&format!(".nixxa-restore-{}-1", std::process::id()), "squatter\n");
        std::fs::remove_file(s.at("keep.txt")).unwrap();
        store.restore(&before).await.unwrap();
        let mut left: Vec<_> = std::fs::read_dir(s.ws()).unwrap().map(|e| e.unwrap().file_name()).collect();
        left.sort();
        assert_eq!(left, vec![std::ffi::OsString::from("keep.txt")]);
        assert_eq!(std::fs::read_dir(s.outside()).unwrap().count(), 1, "the link's target is untouched");
    }

    #[tokio::test]
    async fn equal_metadata_with_new_contents_is_still_seen() {
        // A command can put the mtime back after changing a file; the ctime gives it away (and
        // a recent change is hashed again anyway).
        let s = Scratch::new();
        s.write("a.txt", "aaaa");
        let old = rustix::fs::Timespec { tv_sec: 1_000_000_000, tv_nsec: 0 };
        let times = rustix::fs::Timestamps { last_access: old, last_modification: old };
        let ws = std::fs::File::open(s.ws()).unwrap();
        rustix::fs::utimensat(&ws, "a.txt", &times, rustix::fs::AtFlags::empty()).unwrap();
        let store = s.store(DEFAULT_CAP);
        let one = store.checkpoint().await.unwrap();
        std::fs::write(s.at("a.txt"), "bbbb").unwrap();
        rustix::fs::utimensat(&ws, "a.txt", &times, rustix::fs::AtFlags::empty()).unwrap();
        let two = store.checkpoint().await.unwrap();
        assert_ne!(one, two, "the change must be seen");
        store.restore(&one).await.unwrap();
        assert_eq!(s.read("a.txt"), "aaaa");
    }

    /// The review's probe: a sparse file costs a command nothing, but must not make the store
    /// copy it before refusing it.
    #[tokio::test]
    async fn a_sparse_file_past_the_cap_is_refused_before_anything_is_copied() {
        let s = Scratch::new();
        s.write("small.txt", "small\n");
        let big = std::fs::File::create(s.at("big")).unwrap();
        big.set_len(2 * 1024 * 1024 * 1024).unwrap();
        let store = s.store(1024 * 1024);
        let started = std::time::Instant::now();
        let refused = store.checkpoint().await;
        assert!(matches!(&refused, Err(SandboxError::LimitExceeded(why)) if why.contains("`big`")), "{refused:?}");
        assert!(started.elapsed() < std::time::Duration::from_secs(2), "refused at once: {:?}", started.elapsed());
        let temp = s.base.join("store").join("run-1").join("tmp");
        assert_eq!(std::fs::read_dir(temp).unwrap().count(), 0, "nothing was copied");
        assert!(s.blobs().is_empty(), "and nothing kept");
        // The undo view still works: the file isn't read, just marked.
        std::fs::remove_file(s.at("big")).unwrap();
        let before = store.checkpoint().await.unwrap();
        let big = std::fs::File::create(s.at("big")).unwrap();
        big.set_len(2 * 1024 * 1024 * 1024).unwrap();
        let started = std::time::Instant::now();
        let diff = store.diff(&before, None).await.unwrap();
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        assert!(find(&diff, "big").before.is_none());
        store.restore(&before).await.unwrap();
        assert!(!s.at("big").exists());
    }

    /// How deep the deepest test trees go. On macOS each directory made or removed costs more the
    /// deeper it is (CI: 5,000 levels took 48 s, where Linux takes 0.1 s), so 100,000 would take
    /// hours there; Linux (and its CI job) runs the review's 100,000, and macOS a depth still
    /// well past [`MAX_DEPTH`] and [`REMOVE_DEPTH`].
    const DEEPEST: usize = if cfg!(target_os = "macos") { 1_000 } else { 100_000 };
    const DEEPER: usize = if cfg!(target_os = "macos") { 1_000 } else { 5_000 };

    /// `d/d/d/…` `levels` deep under `name`, made one level at a time (no path is ever long).
    fn deep(s: &Scratch, name: &str, levels: usize) {
        let mut dir = std::fs::File::open(s.ws()).unwrap();
        let mut name = name.to_string();
        for _ in 0..levels {
            rustix::fs::mkdirat(&dir, name.as_str(), rustix::fs::Mode::from_raw_mode(0o755)).unwrap();
            let next = rustix::fs::openat(
                &dir,
                name.as_str(),
                rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::DIRECTORY,
                rustix::fs::Mode::empty(),
            )
            .unwrap();
            dir = std::fs::File::from(next);
            name = "d".into();
        }
        rustix::fs::mkdirat(&dir, "bottom", rustix::fs::Mode::from_raw_mode(0o755)).unwrap();
    }

    /// The review's second probe: 100,000 levels made by a command. Before, the snapshot's
    /// recursion overflowed the blocking thread's stack and aborted Nixxa.
    #[tokio::test]
    async fn a_tree_of_any_depth_neither_crashes_nor_blocks_undo() {
        let s = Scratch::new();
        s.write("keep.txt", "keep\n");
        let store = s.store(DEFAULT_CAP);
        let before = store.checkpoint().await.unwrap();
        deep(&s, "deep", DEEPEST);
        // A checkpoint is refused with the reason, at once.
        let refused = store.checkpoint().await;
        assert!(
            matches!(&refused, Err(SandboxError::Other(why)) if why.contains("nested more than 256")),
            "{refused:?}"
        );
        // The undo view shows it, without going all the way down.
        let diff = store.diff(&before, None).await.unwrap();
        assert!(find(&diff, "deep").before.is_none(), "{:?}", &diff.changes[..3]);
        assert!(diff.changes.len() <= MAX_DEPTH + 2, "{}", diff.changes.len());
        // And the undo removes all of it.
        let undone = store.restore(&before).await.unwrap();
        assert!(undone.changes.iter().any(|c| c.path == "deep"));
        assert!(!s.at("deep").exists(), "the deep tree is gone");
        let left: Vec<_> = std::fs::read_dir(s.ws()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(left, vec![std::ffi::OsString::from("keep.txt")], "no moved pieces are left");
        // Checkpoints work again.
        store.checkpoint().await.unwrap();
    }

    #[tokio::test]
    async fn a_deep_tree_in_a_checkpoint_is_restored_and_a_deeper_one_removed() {
        // Within the limit, a deep tree is kept and put back; past REMOVE_DEPTH it is removed in
        // rounds.
        let s = Scratch::new();
        deep(&s, "kept", 200);
        let store = s.store(DEFAULT_CAP);
        let before = store.checkpoint().await.unwrap();
        std::fs::rename(s.at("kept"), s.at("moved")).unwrap();
        deep(&s, "fresh", DEEPER);
        store.restore(&before).await.unwrap();
        assert!(!s.at("moved").exists() && !s.at("fresh").exists());
        let bottom = (0..199).fold(s.at("kept"), |path, _| path.join("d")).join("bottom");
        assert!(bottom.is_dir(), "the deep tree is back to its bottom");
        assert!(store.diff(&before, None).await.unwrap().is_empty());
    }

    /// The numbers for the PR: `cargo test -p nixxa-sandbox --release -- --ignored --nocapture
    /// a_checkpoint_of_ten_thousand_files`.
    #[tokio::test]
    #[ignore = "a timing, run by hand"]
    async fn a_checkpoint_of_ten_thousand_files() {
        let s = Scratch::new();
        for n in 0..10_000 {
            s.write(&format!("d{}/e{}/f{n}.txt", n % 50, n % 7), &format!("file {n}\n").repeat(20));
        }
        let store = s.store(DEFAULT_CAP);
        let time = |label: &str, start: std::time::Instant| println!("{label}: {:?}", start.elapsed());
        let start = std::time::Instant::now();
        store.checkpoint().await.unwrap();
        time("first checkpoint (copies every file)", start);
        // Let the racy window pass, so unchanged files are recognised without reading them.
        std::thread::sleep(RACY + std::time::Duration::from_millis(100));
        store.checkpoint().await.unwrap();
        let start = std::time::Instant::now();
        store.checkpoint().await.unwrap();
        time("unchanged", start);
        s.write("d3/e3/f3.txt", "changed\n");
        let start = std::time::Instant::now();
        store.checkpoint().await.unwrap();
        time("one file changed", start);
    }

    /// A real tree's first checkpoint, for the PR: `NIXXA_TIMING_DIR=<a repo> cargo test -p
    /// nixxa-sandbox --release -- --ignored --nocapture a_first_checkpoint_of_a_real_tree`. It
    /// only reads that directory.
    #[tokio::test]
    #[ignore = "a timing, run by hand"]
    async fn a_first_checkpoint_of_a_real_tree() {
        let Some(dir) = std::env::var_os("NIXXA_TIMING_DIR") else { return };
        let s = Scratch::new();
        let store = Checkpoints::new(
            &PathBuf::from(dir).canonicalize().unwrap(),
            &CheckpointConfig { dir: s.base.join("store").join("run-1"), cap: DEFAULT_CAP },
        )
        .unwrap();
        let start = std::time::Instant::now();
        let id = store.checkpoint().await.unwrap();
        println!(
            "first checkpoint: {:?}, {} MiB stored",
            start.elapsed(),
            store_size(&s.base.join("store")) / (1024 * 1024)
        );
        std::thread::sleep(RACY + std::time::Duration::from_millis(100));
        store.checkpoint().await.unwrap();
        let start = std::time::Instant::now();
        store.checkpoint().await.unwrap();
        println!("unchanged: {:?}", start.elapsed());
        let diff = store.diff(&id, None).await.unwrap();
        println!("left out: {:?}", diff.not_covered);
    }

    fn store_size(dir: &Path) -> u64 {
        std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| {
                let meta = std::fs::symlink_metadata(e.path()).unwrap();
                if meta.is_dir() { store_size(&e.path()) } else { meta.len() }
            })
            .sum()
    }
}
