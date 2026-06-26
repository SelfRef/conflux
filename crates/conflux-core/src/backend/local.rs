//! A backend that mirrors to another local directory.
//!
//! Doubles as the engine's offline test harness and as a real "local mirror"
//! feature. The remote change id is the blake3 content hash, so change
//! detection is exact.

use super::{walk_snapshot, Backend, RemoteSnapshot};
use crate::config::{Remote, Sync};
use crate::error::Result;
use crate::hash::hash_bytes;
use crate::model::RemoteMeta;
use crate::relpath::RelPath;
use std::path::PathBuf;

/// Mirrors a group to `<remote.url>/<sync.remote_path>`.
pub struct LocalBackend {
    base: PathBuf,
}

impl LocalBackend {
    /// Build the backend; the base directory is the remote url joined with the
    /// group's remote path.
    pub fn new(remote: &Remote, sync: &Sync) -> Self {
        let mut base = PathBuf::from(&remote.url);
        for part in sync.remote_path.split('/').filter(|s| !s.is_empty()) {
            base.push(part);
        }
        LocalBackend { base }
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

    fn write(&self, path: &RelPath, data: &[u8]) -> Result<RemoteMeta> {
        let full = self.full(path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&full, data)?;
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
}
