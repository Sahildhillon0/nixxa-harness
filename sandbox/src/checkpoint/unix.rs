//! The checkpoint store's file operations on Unix: everything relative to a held directory
//! descriptor, one name at a time, and nothing ever follows a symbolic link.
//!
//! * Names are single components read from the directory itself (or the store's own trees,
//!   which were read that way), so there is no path to walk and no `..`.
//! * Directories are opened `O_DIRECTORY | O_NOFOLLOW`: a name that is (or became) a symbolic
//!   link fails to open instead of leading elsewhere.
//! * Files are read `O_NOFOLLOW | O_NONBLOCK | O_NOCTTY` after `fstatat(AT_SYMLINK_NOFOLLOW)`
//!   said they are regular, and checked again with `fstat` once open: a FIFO, socket or device
//!   is never opened for reading (no hang), and one swapped in between is refused.
//! * Writes create a new file `O_CREAT | O_EXCL | O_NOFOLLOW` beside the target and rename it
//!   over the name, so they never write through an existing file, link or hard link.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use rustix::fs::{AtFlags, FileType, Mode, OFlags};

use super::{Key, Kind, Meta};

/// An open directory of the workspace.
pub(crate) struct Dir(OwnedFd);

impl Dir {
    /// Opens the workspace root itself (a path the harness gave, already canonical).
    pub(crate) fn open_root(path: &Path) -> io::Result<Self> {
        Ok(Self(rustix::fs::open(path, OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC, Mode::empty())?))
    }

    /// The directory's own metadata.
    pub(crate) fn meta(&self) -> io::Result<Meta> {
        Ok(meta(&rustix::fs::fstat(&self.0)?))
    }

    /// The entries, each with its own metadata (links not followed). Entries that vanish while
    /// being listed are left out.
    pub(crate) fn list(&self) -> io::Result<Vec<(OsString, Meta)>> {
        let mut out = Vec::new();
        for entry in rustix::fs::Dir::read_from(&self.0)? {
            let entry = entry?;
            let name = entry.file_name();
            if name == c"." || name == c".." {
                continue;
            }
            match rustix::fs::statat(&self.0, name, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(st) => out.push((OsStr::from_bytes(name.to_bytes()).to_owned(), meta(&st))),
                Err(rustix::io::Errno::NOENT) => {}
                Err(err) => return Err(err.into()),
            }
        }
        Ok(out)
    }

    /// A subdirectory, never through a link.
    pub(crate) fn open_dir(&self, name: &OsStr) -> io::Result<Self> {
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        Ok(Self(rustix::fs::openat(&self.0, name, flags, Mode::empty())?))
    }

    /// A regular file for reading, and its metadata once open. Anything else is refused.
    pub(crate) fn open_file(&self, name: &OsStr) -> io::Result<(File, Meta)> {
        let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::NOCTTY | OFlags::CLOEXEC;
        let fd = rustix::fs::openat(&self.0, name, flags, Mode::empty())?;
        let st = rustix::fs::fstat(&fd)?;
        let meta = meta(&st);
        if meta.kind != Kind::File {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a regular file"));
        }
        // Reads block normally from here on.
        rustix::fs::fcntl_setfl(&fd, OFlags::empty())?;
        Ok((File::from(fd), meta))
    }

    /// Where a symbolic link points, as written.
    pub(crate) fn read_link(&self, name: &OsStr) -> io::Result<Vec<u8>> {
        Ok(rustix::fs::readlinkat(&self.0, name, Vec::new())?.into_bytes())
    }

    /// Removes a name: a file, link or special file, or an empty directory.
    pub(crate) fn remove(&self, name: &OsStr, dir: bool) -> io::Result<()> {
        let flags = if dir { AtFlags::REMOVEDIR } else { AtFlags::empty() };
        Ok(rustix::fs::unlinkat(&self.0, name, flags)?)
    }

    /// Makes a directory only its owner can use (its mode is set once it is filled).
    pub(crate) fn make_dir(&self, name: &OsStr) -> io::Result<()> {
        Ok(rustix::fs::mkdirat(&self.0, name, Mode::from_raw_mode(0o700))?)
    }

    /// Creates a new file that must not exist yet, readable and writable by its owner only.
    pub(crate) fn create(&self, name: &OsStr) -> io::Result<File> {
        let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        Ok(File::from(rustix::fs::openat(&self.0, name, flags, Mode::from_raw_mode(0o600))?))
    }

    /// Renames `from` to `to` within this directory, replacing `to`'s entry (never what it
    /// points to).
    pub(crate) fn rename(&self, from: &OsStr, to: &OsStr) -> io::Result<()> {
        Ok(rustix::fs::renameat(&self.0, from, &self.0, to)?)
    }

    /// Makes a symbolic link `name` pointing at `target`.
    pub(crate) fn symlink(&self, target: &[u8], name: &OsStr) -> io::Result<()> {
        Ok(rustix::fs::symlinkat(OsStr::from_bytes(target), &self.0, name)?)
    }

    /// Sets this directory's own mode.
    pub(crate) fn set_mode(&self, mode: u32) -> io::Result<()> {
        Ok(rustix::fs::fchmod(&self.0, raw_mode(mode))?)
    }

    /// Gives a subdirectory its owner's full rights, so it can be opened, without following a
    /// link: on Linux through a descriptor that only names it (`O_PATH`), elsewhere with
    /// `fchmodat(AT_SYMLINK_NOFOLLOW)`.
    pub(crate) fn unlock(&self, name: &OsStr) -> io::Result<()> {
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::{AsFd, AsRawFd};
            let flags = OFlags::PATH | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
            let fd = rustix::fs::openat(&self.0, name, flags, Mode::empty())?;
            let mode = meta(&rustix::fs::fstat(&fd)?).mode;
            let proc = format!("/proc/self/fd/{}", fd.as_fd().as_raw_fd());
            Ok(rustix::fs::chmod(proc.as_str(), Mode::from_raw_mode((mode | 0o700) & 0o7777))?)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let st = rustix::fs::statat(&self.0, name, AtFlags::SYMLINK_NOFOLLOW)?;
            let meta = meta(&st);
            if meta.kind != Kind::Dir {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a directory"));
            }
            Ok(rustix::fs::chmodat(&self.0, name, raw_mode(meta.mode | 0o700), AtFlags::SYMLINK_NOFOLLOW)?)
        }
    }
}

/// Sets a file's mode through its descriptor.
pub(crate) fn set_file_mode(file: &File, mode: u32) -> io::Result<()> {
    Ok(rustix::fs::fchmod(file, raw_mode(mode))?)
}

/// Permission bits as a `Mode` (its raw type is `u32` on Linux, `u16` on macOS; the bits fit).
#[allow(clippy::unnecessary_cast)]
fn raw_mode(mode: u32) -> Mode {
    Mode::from_raw_mode((mode & 0o7777) as rustix::fs::RawMode)
}

/// The mode a new store directory gets: only its owner can use it.
pub(crate) fn make_private_dir(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().mode(0o700).create(path)?;
    // The umask may have taken bits off; it can't have added any, but make sure.
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o700))
}

/// The bytes of a name, for paths shown to the user and the agent.
pub(crate) fn name_bytes(name: &OsStr) -> Vec<u8> {
    name.as_bytes().to_vec()
}

// The field types of `struct stat` differ between Unixes (`u32` here, `u16` there); the casts
// are exact on each.
#[allow(clippy::unnecessary_cast)]
fn meta(st: &rustix::fs::Stat) -> Meta {
    let kind = match FileType::from_raw_mode(st.st_mode as _) {
        FileType::RegularFile => Kind::File,
        FileType::Directory => Kind::Dir,
        FileType::Symlink => Kind::Symlink,
        _ => Kind::Other,
    };
    let size = st.st_size as u64;
    Meta {
        kind,
        mode: (st.st_mode as u32) & 0o7777,
        size,
        key: Key {
            dev: st.st_dev as u64,
            ino: st.st_ino as u64,
            size,
            mtime: i128::from(st.st_mtime as i64) * 1_000_000_000 + i128::from(st.st_mtime_nsec as i64),
            ctime: i128::from(st.st_ctime as i64) * 1_000_000_000 + i128::from(st.st_ctime_nsec as i64),
        },
    }
}
