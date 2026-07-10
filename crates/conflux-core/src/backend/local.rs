//! A backend that mirrors to another local directory.
//!
//! Doubles as the engine's offline test harness and as a real "local mirror"
//! feature. The remote change id is the blake3 content hash, so change
//! detection is exact.

use super::{walk_empty_dirs, walk_snapshot, Backend, RemoteSnapshot};
use crate::config::{Remote, Sync};
use crate::error::Result;
use crate::hash::hash_bytes;
use crate::model::RemoteMeta;
use crate::relpath::RelPath;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::SystemTime;

/// Mirrors a group to `<remote.url>/<sync.remote_path>`.
pub struct LocalBackend {
    base: PathBuf,
}

/// The on-disk directory a local-backed group mirrors into:
/// `<remote.url>/<sync.remote_path>`. Exposed so the daemon can watch it for the
/// `watch-both` trigger without constructing a full backend.
pub fn base_path(remote: &Remote, sync: &Sync) -> PathBuf {
    let mut base = PathBuf::from(&remote.url);
    for part in sync.remote_path.split('/').filter(|s| !s.is_empty()) {
        base.push(part);
    }
    base
}

impl LocalBackend {
    /// Build the backend; the base directory is the remote url joined with the
    /// group's remote path.
    pub fn new(remote: &Remote, sync: &Sync) -> Self {
        LocalBackend {
            base: base_path(remote, sync),
        }
    }

    fn full(&self, path: &RelPath) -> PathBuf {
        path.to_local(&self.base)
    }
}

impl Backend for LocalBackend {
    fn snapshot(&self) -> Result<RemoteSnapshot> {
        walk_snapshot(&self.base, None)
    }

    fn read(&self, path: &RelPath) -> Result<Vec<u8>> {
        Ok(std::fs::read(self.full(path))?)
    }

    fn write(&self, path: &RelPath, data: &[u8], mtime: Option<SystemTime>) -> Result<RemoteMeta> {
        let full = self.full(path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&full, data)?;
        // Preserve the source's modification time so the mirror is a 1:1 copy.
        if let Some(mtime) = mtime {
            if let Ok(f) = std::fs::File::options().write(true).open(&full) {
                let _ = f.set_modified(mtime);
            }
        }
        let mtime = std::fs::metadata(&full)
            .ok()
            .and_then(|m| m.modified().ok());
        Ok(RemoteMeta {
            id: hash_bytes(data),
            mtime,
            size: data.len() as u64,
        })
    }

    fn remove(&self, path: &RelPath) -> Result<()> {
        let full = self.full(path);
        match std::fs::remove_file(&full) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    fn finalize(&self) -> Result<()> {
        Ok(())
    }

    fn supports_empty_dirs(&self) -> bool {
        true
    }

    fn snapshot_dirs(&self) -> Result<BTreeSet<RelPath>> {
        walk_empty_dirs(&self.base)
    }

    fn create_dir(&self, path: &RelPath) -> Result<()> {
        std::fs::create_dir_all(self.full(path))?;
        Ok(())
    }

    fn remove_dir(&self, path: &RelPath) -> Result<()> {
        match std::fs::remove_dir(self.full(path)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}
