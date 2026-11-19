//! `ReadFile`, `WriteFile` and `ListDir`, done by the daemon itself, beneath the workspace only.
//!
//! A path is first checked as text ([`relative`]): an absolute path is refused unless it is
//! under `/workspace`, and `..` is refused anywhere. It is then opened with
//! `openat2(RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS)` relative to a descriptor for the workspace
//! root, so the kernel refuses any resolution (an absolute symlink, a `..` inside a symlink, a
//! `/proc` magic link) that would leave it. Directories created on the way to a written file
//! are walked one component at a time with symlinks refused.

use std::io::{Read, Write};
use std::os::fd::{AsFd, OwnedFd};

use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags, ResolveFlags};
use rustix::io::Errno;

use nixxa_contracts::{DirEntry, Outcome};

/// Where the workspace is mounted inside the sandbox.
pub(crate) const WORKSPACE: &str = "/workspace";

const BENEATH: ResolveFlags = ResolveFlags::BENEATH.union(ResolveFlags::NO_MAGICLINKS);

/// Opens the workspace root; every other path is resolved beneath this descriptor.
pub(crate) fn open_root(path: &str) -> std::io::Result<OwnedFd> {
    Ok(rustix::fs::open(path, OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC, Mode::empty())?)
}

/// The workspace-relative form of `path` (`"."` for the root), or why it is refused.
pub(crate) fn relative(path: &str) -> Result<String, &'static str> {
    if path.contains('\0') {
        return Err("the path contains a NUL byte");
    }
    let rest = match path.strip_prefix(WORKSPACE) {
        Some("") => "",
        Some(rest) if rest.starts_with('/') => rest,
        _ if path.starts_with('/') => return Err("absolute paths outside /workspace are refused"),
        _ => path,
    };
    let mut parts = Vec::new();
    for part in rest.split('/') {
        match part {
            "" | "." => {}
            ".." => return Err("`..` is not allowed in paths"),
            part => parts.push(part),
        }
    }
    Ok(if parts.is_empty() { ".".into() } else { parts.join("/") })
}

/// Reads a UTF-8 text file of at most `cap` bytes.
pub(crate) fn read(root: &OwnedFd, path: &str, cap: u64) -> Outcome {
    let rel = match relative(path) {
        Ok(rel) => rel,
        Err(why) => return failed(why),
    };
    let fd = match rustix::fs::openat2(root, rel.as_str(), OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty(), BENEATH) {
        Ok(fd) => fd,
        Err(err) => return failed(errno_words(err)),
    };
    match rustix::fs::fstat(&fd) {
        Ok(st) if FileType::from_raw_mode(st.st_mode) == FileType::Directory => return failed("that is a directory"),
        Ok(st) if FileType::from_raw_mode(st.st_mode) != FileType::RegularFile => {
            return failed("that is not a regular file");
        }
        Ok(st) if st.st_size as u64 > cap => {
            return failed(format!("the file is too large to read ({} bytes, limit {cap})", st.st_size));
        }
        Ok(_) => {}
        Err(err) => return failed(errno_words(err)),
    }
    let mut bytes = Vec::new();
    if let Err(err) = std::fs::File::from(fd).take(cap + 1).read_to_end(&mut bytes) {
        return failed(err.to_string());
    }
    if bytes.len() as u64 > cap {
        return failed(format!("the file is too large to read (limit {cap} bytes)"));
    }
    match String::from_utf8(bytes) {
        Ok(content) => Outcome::Text { content },
        Err(_) => failed("the file is not UTF-8 text"),
    }
}

/// Creates or replaces a file, creating its parent directories.
pub(crate) fn write(root: &OwnedFd, path: &str, content: &str, cap: u64) -> Outcome {
    if content.len() as u64 > cap {
        return failed(format!("the content is too large to write ({} bytes, limit {cap})", content.len()));
    }
    let rel = match relative(path) {
        Ok(rel) => rel,
        Err(why) => return failed(why),
    };
    if rel == "." {
        return failed("that is the workspace root");
    }
    let mut parts: Vec<&str> = rel.split('/').collect();
    let Some(name) = parts.pop() else { return failed("that is the workspace root") };
    let mut dir: Option<OwnedFd> = None;
    for part in parts {
        let at = dir.as_ref().map_or(root.as_fd(), |d| d.as_fd());
        let next = match open_dir_step(at, part) {
            Err(Errno::NOENT) => match rustix::fs::mkdirat(at, part, Mode::from_raw_mode(0o755)) {
                Ok(()) | Err(Errno::EXIST) => open_dir_step(at, part),
                Err(err) => Err(err),
            },
            other => other,
        };
        match next {
            Ok(fd) => dir = Some(fd),
            Err(err) => return failed(errno_words(err)),
        }
    }
    let at = dir.as_ref().map_or(root.as_fd(), |d| d.as_fd());
    let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::TRUNC | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let fd = match rustix::fs::openat2(at, name, flags, Mode::from_raw_mode(0o644), BENEATH) {
        Ok(fd) => fd,
        Err(err) => return failed(errno_words(err)),
    };
    match std::fs::File::from(fd).write_all(content.as_bytes()) {
        Ok(()) => Outcome::Written { bytes: content.len() as u64 },
        Err(err) => failed(err.to_string()),
    }
}

/// One directory component, never through a symlink.
fn open_dir_step(at: impl AsFd, name: &str) -> rustix::io::Result<OwnedFd> {
    rustix::fs::openat2(
        at,
        name,
        OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
        BENEATH | ResolveFlags::NO_SYMLINKS,
    )
}

/// Lists a directory, sorted by name. Symlinks are listed as themselves, not followed.
pub(crate) fn list(root: &OwnedFd, path: &str) -> Outcome {
    let rel = match relative(path) {
        Ok(rel) => rel,
        Err(why) => return failed(why),
    };
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    let fd = match rustix::fs::openat2(root, rel.as_str(), flags, Mode::empty(), BENEATH) {
        Ok(fd) => fd,
        Err(err) => return failed(errno_words(err)),
    };
    let dir = match Dir::read_from(&fd) {
        Ok(dir) => dir,
        Err(err) => return failed(errno_words(err)),
    };
    let mut entries = Vec::new();
    for entry in dir {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => return failed(errno_words(err)),
        };
        let name = entry.file_name();
        if name == c"." || name == c".." {
            continue;
        }
        let st = rustix::fs::statat(&fd, name, AtFlags::SYMLINK_NOFOLLOW).ok();
        let kind = st.as_ref().map(|st| FileType::from_raw_mode(st.st_mode));
        let is_dir = kind == Some(FileType::Directory);
        let size = match (kind, st) {
            (Some(FileType::RegularFile), Some(st)) => Some(st.st_size as u64),
            _ => None,
        };
        entries.push(DirEntry { name: name.to_string_lossy().into_owned(), is_dir, size });
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    Outcome::Listing { entries }
}

/// Checks `cwd` is a directory beneath the workspace and returns its path inside the sandbox.
pub(crate) fn working_dir(root: &OwnedFd, cwd: Option<&str>) -> Result<String, String> {
    let rel = relative(cwd.unwrap_or(".")).map_err(str::to_owned)?;
    let flags = OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC;
    rustix::fs::openat2(root, rel.as_str(), flags, Mode::empty(), BENEATH).map_err(errno_words)?;
    Ok(if rel == "." { WORKSPACE.into() } else { format!("{WORKSPACE}/{rel}") })
}

fn failed(message: impl Into<String>) -> Outcome {
    Outcome::Failed { message: message.into() }
}

fn errno_words(err: Errno) -> String {
    match err {
        Errno::NOENT => "no such file or directory".into(),
        Errno::XDEV => "the path leads outside the workspace".into(),
        Errno::LOOP => "the path goes through a symbolic link, which is not followed here".into(),
        Errno::NOTDIR => "a part of the path is not a directory".into(),
        Errno::ISDIR => "that is a directory".into(),
        Errno::ACCESS | Errno::PERM => "permission denied".into(),
        Errno::EXIST => "it already exists".into(),
        Errno::NOSPC | Errno::DQUOT => "no space left".into(),
        Errno::FBIG => "the file is too large".into(),
        other => std::io::Error::from(other).to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;
    use std::path::PathBuf;

    use super::*;

    struct Scratch(PathBuf);

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// `<tmp>/ws` (the root) and `<tmp>/outside/secret.txt` (must stay unreachable).
    fn scratch(tag: &str) -> (Scratch, OwnedFd) {
        let base = std::env::temp_dir().join(format!("nixxa-sandboxd-files-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("ws")).unwrap();
        std::fs::create_dir_all(base.join("outside")).unwrap();
        std::fs::write(base.join("outside/secret.txt"), "secret").unwrap();
        let root = open_root(base.join("ws").to_str().unwrap()).unwrap();
        (Scratch(base), root)
    }

    fn is_failed(o: &Outcome) -> bool {
        matches!(o, Outcome::Failed { .. })
    }

    #[test]
    fn relative_paths_are_normalised() {
        assert_eq!(relative("").unwrap(), ".");
        assert_eq!(relative(".").unwrap(), ".");
        assert_eq!(relative("./a//b/./c/").unwrap(), "a/b/c");
        assert_eq!(relative("/workspace").unwrap(), ".");
        assert_eq!(relative("/workspace/").unwrap(), ".");
        assert_eq!(relative("/workspace/src/main.rs").unwrap(), "src/main.rs");
    }

    #[test]
    fn escaping_paths_are_refused_as_text() {
        for bad in [
            "/etc/passwd",
            "/",
            "/workspacex/a",
            "/tmp/a",
            "..",
            "../x",
            "a/../../x",
            "a/..",
            "/workspace/../etc",
            "a\0b",
        ] {
            assert!(relative(bad).is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn writes_create_folders_and_read_back() {
        let (s, root) = scratch("rw");
        assert_eq!(write(&root, "a/b/c.txt", "hi", 100), Outcome::Written { bytes: 2 });
        assert_eq!(std::fs::read_to_string(s.0.join("ws/a/b/c.txt")).unwrap(), "hi");
        assert_eq!(read(&root, "a/b/c.txt", 100), Outcome::Text { content: "hi".into() });
        assert_eq!(read(&root, "/workspace/a/b/c.txt", 100), Outcome::Text { content: "hi".into() });
        match list(&root, "a") {
            Outcome::Listing { entries } => {
                assert_eq!(entries, vec![DirEntry { name: "b".into(), is_dir: true, size: None }])
            }
            other => panic!("{other:?}"),
        }
        match list(&root, "") {
            Outcome::Listing { entries } => assert_eq!(entries.len(), 1),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn caps_and_kinds_are_enforced() {
        let (_s, root) = scratch("caps");
        assert!(is_failed(&write(&root, "big.txt", "0123456789", 5)));
        assert_eq!(write(&root, "ok.txt", "0123456789", 10), Outcome::Written { bytes: 10 });
        assert!(is_failed(&read(&root, "ok.txt", 5)));
        assert!(is_failed(&read(&root, "missing.txt", 5)));
        assert!(is_failed(&write(&root, ".", "x", 5)));
        std::fs::create_dir_all(_s.0.join("ws/d")).unwrap();
        assert!(is_failed(&read(&root, "d", 5)));
        std::fs::write(_s.0.join("ws/bin"), [0xff, 0xfe]).unwrap();
        assert!(is_failed(&read(&root, "bin", 5)));
    }

    #[test]
    fn symlinks_out_of_the_workspace_are_refused() {
        let (s, root) = scratch("links");
        let ws = s.0.join("ws");
        symlink(s.0.join("outside/secret.txt"), ws.join("abs")).unwrap();
        symlink("../outside/secret.txt", ws.join("rel")).unwrap();
        symlink(s.0.join("outside"), ws.join("dir")).unwrap();
        symlink("/proc/self/root/etc/hostname", ws.join("magic")).unwrap();
        for link in ["abs", "rel", "dir/secret.txt", "magic"] {
            assert!(is_failed(&read(&root, link, 100)), "reading {link} must fail");
            assert!(is_failed(&write(&root, link, "pwned", 100)), "writing {link} must fail");
        }
        assert!(is_failed(&write(&root, "dir/new.txt", "pwned", 100)));
        assert!(is_failed(&list(&root, "dir")));
        assert_eq!(std::fs::read_to_string(s.0.join("outside/secret.txt")).unwrap(), "secret");
        assert!(!s.0.join("outside/new.txt").exists());
    }

    #[test]
    fn symlinks_inside_are_listed_not_followed() {
        let (s, root) = scratch("listlinks");
        symlink(s.0.join("outside"), s.0.join("ws/out")).unwrap();
        match list(&root, ".") {
            Outcome::Listing { entries } => {
                assert_eq!(entries, vec![DirEntry { name: "out".into(), is_dir: false, size: None }])
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn working_dirs_stay_inside() {
        let (s, root) = scratch("cwd");
        std::fs::create_dir_all(s.0.join("ws/src")).unwrap();
        symlink(s.0.join("outside"), s.0.join("ws/out")).unwrap();
        assert_eq!(working_dir(&root, None).unwrap(), "/workspace");
        assert_eq!(working_dir(&root, Some("src")).unwrap(), "/workspace/src");
        assert!(working_dir(&root, Some("out")).is_err());
        assert!(working_dir(&root, Some("..")).is_err());
        assert!(working_dir(&root, Some("/tmp")).is_err());
        assert!(working_dir(&root, Some("missing")).is_err());
    }
}
