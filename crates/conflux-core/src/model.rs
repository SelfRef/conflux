//! Core domain types shared across config, the sync engine, and backends.

use serde::{Deserialize, Serialize};
use std::time::SystemTime;

/// Metadata for a local file: content hash, modification time, and size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileMeta {
    /// Blake3 content hash, hex-encoded.
    pub hash: String,
    /// Last modification time.
    pub mtime: SystemTime,
    /// Size in bytes.
    pub size: u64,
}

/// Metadata for a remote file: a backend-defined change id, optional mtime, size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteMeta {
    /// Change token: WebDAV ETag, git blob OID, or local content hash.
    pub id: String,
    /// Last modification time, if the backend exposes one.
    pub mtime: Option<SystemTime>,
    /// Size in bytes.
    pub size: u64,
}

/// Which kind of remote backend a connection uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RemoteKind {
    /// WebDAV (e.g. Nextcloud).
    Webdav,
    /// A git repository.
    Git,
    /// A local directory (mirror / test backend).
    Local,
}

impl RemoteKind {
    /// The lowercase name used in config (`"webdav"`, `"git"`, `"local"`).
    pub fn as_str(&self) -> &'static str {
        match self {
            RemoteKind::Webdav => "webdav",
            RemoteKind::Git => "git",
            RemoteKind::Local => "local",
        }
    }
}

/// How a sync group is triggered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Trigger {
    /// Only synced on explicit request (`conflux sync`).
    Manual,
    /// Synced on a fixed interval.
    Timer,
    /// Synced when watched local files change.
    Watch,
    /// Like `watch`, but also watches the remote side. Only a `local` backend
    /// exposes a watchable filesystem path, so for other backends this behaves
    /// like `watch` (local-only) and logs a warning.
    #[serde(rename = "watch-both")]
    WatchBoth,
}

/// Direction of synchronization for a sync group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    /// Bidirectional sync with newer-wins conflict resolution (default).
    #[default]
    Sync,
    /// Pull-only: download remote changes; never upload local ones.
    Pull,
}

/// How empty directories are handled during a sync.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EmptyDirMode {
    /// Sync files only; empty directories are neither created nor removed (default).
    #[default]
    Ignore,
    /// Prune empty directories from both sides after a sync.
    Prune,
    /// Treat empty directories as first-class: mirror them to both sides and
    /// propagate their creation and deletion (tracked in the index).
    Mirror,
}

/// How much of the remote tree a sync pulls down.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PullScope {
    /// Pull every file under the remote path (default).
    #[default]
    All,
    /// Pull only the paths listed in `include`.
    Include,
}
