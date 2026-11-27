//! The checkpoint store's file operations on Windows, through paths: each operation checks the
//! name with `symlink_metadata` first (a link or junction is never followed or entered), but
//! the check and the operation are two steps, so a name swapped for a link in between is not
//! caught. Only "This directory" (`HostSandbox`, not isolated) runs on Windows, where the
//! agent's commands already run as the user, so this doesn't give them anything they lack.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use super::{Key, Kind, Meta};

/// A directory of the workspace.
pub(crate) struct Dir(PathBuf);

fn meta(md: &std::fs::Metadata) -> Meta {
    let file_type = md.file_type();
    let kind = if file_type.is_symlink() {
        Kind::Symlink
    } else if file_type.is_dir() {
        Kind::Dir
    } else if file_type.is_file() {
        Kind::File
    } else {
        Kind::Other
    };
    let mode = match kind {
        Kind::Dir => 0o755,
        _ if md.permissions().readonly() => 0o444,
        _ => 0o644,
    };
    let nanos = |time: io::Result<std::time::SystemTime>| {
        time.ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos() as i128)
    };
    Meta {
        kind,
        mode,
        size: md.len(),
        key: Key { dev: 0, ino: 0, size: md.len(), mtime: nanos(md.modified()), ctime: nanos(md.created()) },
    }
}

fn not_a(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, format!("not a {what}"))
}

impl Dir {
    pub(crate) fn open_root(path: &Path) -> io::Result<Self> {
        if std::fs::symlink_metadata(path)?.is_dir() { Ok(Self(path.to_path_buf())) } else { Err(not_a("directory")) }
    }

    pub(crate) fn meta(&self) -> io::Result<Meta> {
        Ok(meta(&std::fs::symlink_metadata(&self.0)?))
    }

    pub(crate) fn list(&self) -> io::Result<Vec<(OsString, Meta)>> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&self.0)? {
            let entry = entry?;
            match std::fs::symlink_metadata(entry.path()) {
                Ok(md) => out.push((entry.file_name(), meta(&md))),
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => return Err(err),
            }
        }
        Ok(out)
    }

    fn kind(&self, name: &OsStr) -> io::Result<Kind> {
        Ok(meta(&std::fs::symlink_metadata(self.0.join(name))?).kind)
    }

    pub(crate) fn open_dir(&self, name: &OsStr) -> io::Result<Self> {
        match self.kind(name)? {
            Kind::Dir => Ok(Self(self.0.join(name))),
            _ => Err(not_a("directory")),
        }
    }

    pub(crate) fn open_file(&self, name: &OsStr) -> io::Result<(File, Meta)> {
        if self.kind(name)? != Kind::File {
            return Err(not_a("regular file"));
        }
        let file = File::open(self.0.join(name))?;
        let md = meta(&file.metadata()?);
        if md.kind != Kind::File {
            return Err(not_a("regular file"));
        }
        Ok((file, md))
    }

    pub(crate) fn read_link(&self, name: &OsStr) -> io::Result<Vec<u8>> {
        Ok(std::fs::read_link(self.0.join(name))?.to_string_lossy().into_owned().into_bytes())
    }

    pub(crate) fn remove(&self, name: &OsStr, dir: bool) -> io::Result<()> {
        let path = self.0.join(name);
        let md = std::fs::symlink_metadata(&path)?;
        if md.permissions().readonly() {
            let mut permissions = md.permissions();
            #[allow(clippy::permissions_set_readonly_false)]
            permissions.set_readonly(false);
            std::fs::set_permissions(&path, permissions)?;
        }
        // A directory link is removed as a directory entry, never entered.
        if dir || (md.file_type().is_symlink() && md.is_dir()) {
            std::fs::remove_dir(path)
        } else {
            std::fs::remove_file(path)
        }
    }

    pub(crate) fn make_dir(&self, name: &OsStr) -> io::Result<()> {
        std::fs::create_dir(self.0.join(name))
    }

    pub(crate) fn create(&self, name: &OsStr) -> io::Result<File> {
        std::fs::OpenOptions::new().write(true).create_new(true).open(self.0.join(name))
    }

    pub(crate) fn rename(&self, from: &OsStr, to: &OsStr) -> io::Result<()> {
        std::fs::rename(self.0.join(from), self.0.join(to))
    }

    pub(crate) fn rename_into(&self, from: &OsStr, into: &Dir, to: &OsStr) -> io::Result<()> {
        std::fs::rename(self.0.join(from), into.0.join(to))
    }

    pub(crate) fn has(&self, name: &OsStr) -> io::Result<bool> {
        match std::fs::symlink_metadata(self.0.join(name)) {
            Ok(_) => Ok(true),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(err),
        }
    }

    pub(crate) fn symlink(&self, target: &[u8], name: &OsStr) -> io::Result<()> {
        let target = String::from_utf8_lossy(target).into_owned();
        std::os::windows::fs::symlink_file(target, self.0.join(name))
    }

    pub(crate) fn set_mode(&self, _mode: u32) -> io::Result<()> {
        Ok(())
    }

    pub(crate) fn unlock(&self, _name: &OsStr) -> io::Result<()> {
        Ok(())
    }
}

/// Windows keeps only the read-only bit.
pub(crate) fn set_file_mode(file: &File, mode: u32) -> io::Result<()> {
    let mut permissions = file.metadata()?.permissions();
    permissions.set_readonly(mode & 0o222 == 0);
    file.set_permissions(permissions)
}

pub(crate) fn make_private_dir(path: &Path) -> io::Result<()> {
    std::fs::create_dir(path)
}

pub(crate) fn name_bytes(name: &OsStr) -> Vec<u8> {
    name.to_string_lossy().into_owned().into_bytes()
}
