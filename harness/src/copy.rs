//! Copying a directory tree for "work on a copy".

use std::fs;
use std::path::{Path, PathBuf};

const SIZE_CAP: u64 = 2 * 1024 * 1024 * 1024;

pub(crate) fn copy_dir(src: &Path, dst: &Path) -> Result<(), String> {
    let size = dir_size(src)?;
    if size > SIZE_CAP {
        return Err(format!("directory is too large to copy ({} bytes, cap {})", size, SIZE_CAP));
    }
    fs::create_dir_all(dst).map_err(|e| e.to_string())?;
    copy_recursive(src, dst)
}

fn dir_size(path: &Path) -> Result<u64, String> {
    let mut total = 0u64;
    for entry in fs::read_dir(path).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let meta = entry.file_type().map_err(|e| e.to_string())?;
        if meta.is_symlink() {
            continue;
        }
        if meta.is_dir() {
            total += dir_size(&entry.path())?;
        } else {
            total += entry.metadata().map_err(|e| e.to_string())?.len();
        }
    }
    Ok(total)
}

fn copy_recursive(src: &Path, dst: &Path) -> Result<(), String> {
    for entry in fs::read_dir(src).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        let ft = entry.file_type().map_err(|e| e.to_string())?;
        if ft.is_symlink() {
            let target = fs::read_link(&src_path).map_err(|e| e.to_string())?;
            #[cfg(unix)]
            std::os::unix::fs::symlink(target, &dst_path).map_err(|e| e.to_string())?;
            #[cfg(not(unix))]
            fs::copy(&src_path, &dst_path).map_err(|e| e.to_string())?;
        } else if ft.is_dir() {
            fs::create_dir_all(&dst_path).map_err(|e| e.to_string())?;
            copy_recursive(&src_path, &dst_path)?;
        } else {
            fs::copy(&src_path, &dst_path).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

#[allow(dead_code)]
pub(crate) fn scratch_root() -> PathBuf {
    // simple: temp dir
    std::env::temp_dir().join("nixxa-scratch")
}

#[allow(dead_code)]
pub(crate) fn cleanup_old() {
    let root = scratch_root();
    let _ = fs::create_dir_all(&root);
    let cutoff = std::time::SystemTime::now() - std::time::Duration::from_secs(3 * 24 * 3600);
    if let Ok(entries) = fs::read_dir(&root) {
        for entry in entries.flatten() {
            if let Ok(meta) = entry.metadata() {
                if let Ok(modified) = meta.modified() {
                    if modified < cutoff {
                        let _ = fs::remove_dir_all(entry.path());
                    }
                }
            }
        }
    }
}
