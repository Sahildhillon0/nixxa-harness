//! Scratch directories for the sandbox (D-048): an empty one for "Sandbox (empty)", or a copy of
//! the launch directory for "Work on a copy". They live under `<data dir>/nixxa/scratch`, one
//! per session; ones older than [`MAX_AGE`] are removed at start-up.
//!
//! The copy keeps symlinks as symlinks (never following them), skips sockets, FIFOs and device
//! nodes, keeps `.git`, and is refused up front past [`SIZE_CAP`].

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// The largest directory "Work on a copy" copies, in bytes of regular files.
pub(crate) const SIZE_CAP: u64 = 2 * 1024 * 1024 * 1024;

/// Scratch directories older than this are removed at start-up.
pub(crate) const MAX_AGE: Duration = Duration::from_secs(3 * 24 * 60 * 60);

/// Creates a new, empty scratch directory under `root`, named after `kind`.
pub(crate) fn new_dir(root: &Path, kind: &str) -> io::Result<PathBuf> {
    fs::create_dir_all(root)?;
    let stamp = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    let dir = root.join(format!("{kind}-{stamp}-{}", std::process::id()));
    fs::create_dir(&dir)?;
    Ok(dir)
}

/// Removes scratch directories under `root` last changed more than `max_age` ago.
pub(crate) fn remove_old(root: &Path, max_age: Duration) {
    let Ok(entries) = fs::read_dir(root) else { return };
    let Some(cutoff) = SystemTime::now().checked_sub(max_age) else { return };
    for entry in entries.flatten() {
        let old = entry.metadata().and_then(|m| m.modified()).is_ok_and(|modified| modified < cutoff);
        if old && entry.file_type().is_ok_and(|t| t.is_dir()) {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

/// Copies the tree at `src` into the existing, empty directory `dst`.
pub(crate) fn copy_tree(src: &Path, dst: &Path) -> Result<(), String> {
    let size = tree_size(src).map_err(|err| format!("can't read {}: {err}", src.display()))?;
    if size > SIZE_CAP {
        return Err(format!(
            "this directory is too large to copy ({} MiB; the limit is {} MiB)",
            size / (1024 * 1024),
            SIZE_CAP / (1024 * 1024)
        ));
    }
    copy_entries(src, dst).map_err(|err| format!("copying {} failed: {err}", src.display()))
}

/// Bytes of regular files under `dir`, without following symlinks.
fn tree_size(dir: &Path) -> io::Result<u64> {
    let mut total = 0u64;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            total = total.saturating_add(tree_size(&entry.path())?);
        } else if kind.is_file() {
            total = total.saturating_add(entry.metadata()?.len());
        }
        if total > SIZE_CAP {
            break;
        }
    }
    Ok(total)
}

fn copy_entries(src: &Path, dst: &Path) -> io::Result<()> {
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let (from, to) = (entry.path(), dst.join(entry.file_name()));
        // `file_type` doesn't follow symlinks.
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            std::os::unix::fs::symlink(fs::read_link(&from)?, &to)?;
        } else if kind.is_dir() {
            fs::create_dir(&to)?;
            copy_entries(&from, &to)?;
        } else if kind.is_file() {
            fs::copy(&from, &to)?;
        }
        // Sockets, FIFOs and device nodes are skipped: opening a FIFO would block.
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn scratch(tag: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!("nixxa-copy-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }

    #[test]
    fn the_copy_keeps_files_dirs_and_links_and_skips_fifos() {
        let s = scratch("tree");
        let (src, dst) = (s.0.join("src"), s.0.join("dst"));
        fs::create_dir_all(src.join(".git/refs")).unwrap();
        fs::write(src.join("main.rs"), "fn main() {}").unwrap();
        fs::write(src.join(".git/HEAD"), "ref: refs/heads/main").unwrap();
        fs::write(s.0.join("outside.txt"), "secret").unwrap();
        std::os::unix::fs::symlink(s.0.join("outside.txt"), src.join("out-link")).unwrap();
        std::os::unix::fs::symlink("main.rs", src.join("in-link")).unwrap();
        let fifo = std::process::Command::new("mkfifo").arg(src.join("pipe")).status();
        fs::create_dir(&dst).unwrap();

        copy_tree(&src, &dst).expect("copies");

        assert_eq!(fs::read_to_string(dst.join("main.rs")).unwrap(), "fn main() {}");
        assert_eq!(fs::read_to_string(dst.join(".git/HEAD")).unwrap(), "ref: refs/heads/main");
        assert!(dst.join(".git/refs").is_dir());
        for link in ["out-link", "in-link"] {
            assert!(fs::symlink_metadata(dst.join(link)).unwrap().file_type().is_symlink(), "{link} stays a link");
        }
        assert_eq!(fs::read_link(dst.join("in-link")).unwrap(), PathBuf::from("main.rs"));
        if fifo.is_ok_and(|s| s.success()) {
            assert!(fs::symlink_metadata(dst.join("pipe")).is_err(), "the FIFO is skipped");
        }
        // Writing the copy leaves the original alone.
        fs::write(dst.join("main.rs"), "changed").unwrap();
        assert_eq!(fs::read_to_string(src.join("main.rs")).unwrap(), "fn main() {}");
    }

    #[test]
    fn new_dirs_are_empty_and_distinct_and_old_ones_go() {
        let s = scratch("dirs");
        let a = new_dir(&s.0, "copy").unwrap();
        let b = new_dir(&s.0, "empty").unwrap();
        assert_ne!(a, b);
        assert_eq!(fs::read_dir(&a).unwrap().count(), 0);
        remove_old(&s.0, MAX_AGE);
        assert!(a.is_dir() && b.is_dir(), "fresh directories stay");
        remove_old(&s.0, Duration::ZERO);
        assert!(!a.exists() && !b.exists(), "old directories go");
    }
}
