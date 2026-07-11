//! Remote backends and the trait the sync engine drives them through.
//!
//! Backends are **synchronous**: the daemon runs each sync job on a blocking
//! thread (`tokio::task::spawn_blocking`), so backend implementations can use
//! ordinary blocking I/O (`std::fs`, `reqwest::blocking`, `git2`).

pub mod git;
pub mod local;
pub mod webdav;

use crate::config::{Remote, Sync};
use crate::error::Result;
use crate::hash::hash_bytes;
use crate::model::RemoteMeta;
use crate::relpath::RelPath;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::SystemTime;

/// A snapshot of every file under a group's remote path: path -> metadata.
pub type RemoteSnapshot = BTreeMap<RelPath, RemoteMeta>;

/// Operations the engine needs from a remote.
///
/// `read`/`write`/`remove` address files by `RelPath` relative to the group's
/// remote path. `finalize` flushes a batch of writes (git commits + pushes;
/// other backends treat it as a no-op).
pub trait Backend: Send {
    /// Metadata for every file under the remote path.
    fn snapshot(&self) -> Result<RemoteSnapshot>;
    /// Read a file's bytes.
    fn read(&self, path: &RelPath) -> Result<Vec<u8>>;
    /// Create or overwrite a file, returning its new metadata. `mtime`, when
    /// given, is the source file's modification time; backends that can preserve
    /// it (the local mirror) should, for a 1:1 copy. Others may ignore it.
    fn write(&self, path: &RelPath, data: &[u8], mtime: Option<SystemTime>) -> Result<RemoteMeta>;
    /// Delete a file.
    fn remove(&self, path: &RelPath) -> Result<()>;
    /// Commit/flush the batch of writes performed since the last finalize.
    fn finalize(&self) -> Result<()>;

    /// Whether this backend can represent empty directories. Git cannot (it only
    /// tracks files), so it returns `false` and the engine skips `empty_dirs`.
    fn supports_empty_dirs(&self) -> bool {
        false
    }

    /// The set of *empty* directories under the remote path. Only meaningful when
    /// [`Backend::supports_empty_dirs`] is true.
    fn snapshot_dirs(&self) -> Result<BTreeSet<RelPath>> {
        Ok(BTreeSet::new())
    }

    /// Create an empty directory (and any missing parents) at `path`.
    fn create_dir(&self, _path: &RelPath) -> Result<()> {
        Ok(())
    }

    /// Remove the (expected-empty) directory at `path`.
    fn remove_dir(&self, _path: &RelPath) -> Result<()> {
        Ok(())
    }
}

/// Walk `base` and return every *empty* subdirectory as a `RelPath` (the base
/// itself is never included). A directory is empty when it contains no entries.
pub(crate) fn walk_empty_dirs(base: &Path) -> Result<BTreeSet<RelPath>> {
    let mut dirs = BTreeSet::new();
    if !base.exists() {
        return Ok(dirs);
    }
    for entry in walkdir::WalkDir::new(base).follow_links(false) {
        let entry = entry.map_err(std::io::Error::from)?;
        if !entry.file_type().is_dir() || entry.path() == base {
            continue;
        }
        if entry.path().components().any(|c| c.as_os_str() == ".git") {
            continue;
        }
        let is_empty = std::fs::read_dir(entry.path())?.next().is_none();
        if !is_empty {
            continue;
        }
        if let Some(rel) = RelPath::from_base(base, entry.path()) {
            dirs.insert(rel);
        }
    }
    Ok(dirs)
}

/// Construct a backend for `sync` against its `remote`. `state_dir` is where
/// backends that need local working state (git clones) keep it.
pub fn build(remote: &Remote, sync: &Sync, state_dir: &Path) -> Result<Box<dyn Backend>> {
    use crate::model::RemoteKind;
    match remote.backend {
        RemoteKind::Filesystem => Ok(Box::new(local::LocalBackend::new(remote, sync))),
        RemoteKind::Webdav => Ok(Box::new(webdav::WebdavBackend::new(remote, sync)?)),
        RemoteKind::Git => Ok(Box::new(git::GitBackend::new(remote, sync, state_dir)?)),
    }
}

/// Walk every file under `base` into a [`RemoteSnapshot`], using the blake3
/// content hash as the change id. When `mtime_override` is set, every entry
/// gets that mtime (git: the HEAD commit time); otherwise each file's own mtime.
pub(crate) fn walk_snapshot(
    base: &Path,
    mtime_override: Option<SystemTime>,
) -> Result<RemoteSnapshot> {
    let mut snapshot = RemoteSnapshot::new();
    if !base.exists() {
        return Ok(snapshot);
    }
    for entry in walkdir::WalkDir::new(base).follow_links(false) {
        let entry = entry.map_err(std::io::Error::from)?;
        if !entry.file_type().is_file() {
            continue;
        }
        // Never sync the git metadata directory.
        if entry.path().components().any(|c| c.as_os_str() == ".git") {
            continue;
        }
        let Some(rel) = RelPath::from_base(base, entry.path()) else {
            continue;
        };
        let data = std::fs::read(entry.path())?;
        let mtime = match mtime_override {
            Some(t) => Some(t),
            None => entry.metadata().ok().and_then(|m| m.modified().ok()),
        };
        snapshot.insert(
            rel,
            RemoteMeta {
                id: hash_bytes(&data),
                mtime,
                size: data.len() as u64,
            },
        );
    }
    Ok(snapshot)
}
