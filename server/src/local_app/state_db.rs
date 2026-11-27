//! Backs up Cursor's state database before writing Cursor-owned rows.

use std::path::{Path, PathBuf};

use crate::Result;

/// Copies the database and its WAL/SHM sidecars next to the original.
///
/// Callers pass a dedicated extension per write path so one operation does not
/// overwrite another's pre-write snapshot.
pub(super) fn backup(path: &Path, extension: &str) -> Result<PathBuf> {
    let backup = path.with_extension(extension);
    std::fs::copy(path, &backup)?;
    for suffix in ["-wal", "-shm"] {
        let source = sidecar_path(path, suffix);
        if source.exists() {
            std::fs::copy(&source, sidecar_path(&backup, suffix))?;
        }
    }
    Ok(backup)
}

pub(super) fn sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}
