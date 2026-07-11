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

/// Whether a sync group is allowed to delete files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Deletions {
    /// Never delete a file on either side, regardless of `scope` (default). A
    /// file removed on one side is left in place on the other and the deletion
    /// is not propagated, guarding against accidental data loss.
    #[default]
    Deny,
    /// Propagate deletions in both directions: a file removed on one side is
    /// removed on the other.
    Allow,
}

/// What a sync group covers, relative to its `include` globs. `exclude` always
/// applies on top, in every variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    /// Sync only the paths matched by `include`, bidirectionally (creates,
    /// modifications, and deletions). Anything outside `include` is ignored
    /// entirely — never pushed, pulled, or deleted (default).
    #[default]
    Include,
    /// Like `include`, but additionally pull every other remote file down. Those
    /// non-included files are pull-only: the remote overrides local (a conflict
    /// copy is still kept locally when the local copy diverged), so listing them
    /// in `include` is what promotes them to a two-way sync. Good for dotfiles
    /// you want mirrored read-only unless opted into full sync.
    Remote,
    /// The reverse of `remote`: like `include`, but additionally push every other
    /// local file up. Those non-included files are push-only: local overrides the
    /// remote (a conflict copy of the losing remote version is still kept
    /// locally).
    Local,
    /// Sync the whole tree 1:1 in both directions, ignoring `include`. Deletions
    /// are propagated only when the group's [`Deletions`] policy is `allow`
    /// (with `allow` this can delete files en masse on either side, so use with
    /// care); the default `deny` mirrors creates and edits but never deletes.
    Mirror,
}
