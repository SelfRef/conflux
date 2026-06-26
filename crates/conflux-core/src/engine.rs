//! The sync engine: a three-way reconciliation between the current local tree,
//! the current remote snapshot, and the baseline index from the last sync.
//!
//! For each path we classify how the local and remote sides changed since the
//! baseline, decide an action, then apply it. Bidirectional groups resolve true
//! conflicts by "newer wins", preserving the losing version as a conflict copy.

use crate::backend::Backend;
use crate::config::{Remote, Sync};
use crate::error::Result;
use crate::hash::hash_bytes;
use crate::index::Index;
use crate::matcher::Matcher;
use crate::model::{Direction, FileMeta, PullScope};
use crate::relpath::RelPath;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Which side won a conflict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Winner {
    /// Local file was kept; remote version saved as a conflict copy.
    Local,
    /// Remote file was kept; local version saved as a conflict copy.
    Remote,
}

/// A recorded conflict and where the losing version was preserved.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConflictRecord {
    /// The conflicted path.
    pub path: RelPath,
    /// Which side won.
    pub winner: Winner,
    /// Local path of the preserved (losing) copy.
    pub conflict_copy: PathBuf,
}

/// Summary of what a sync did.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct SyncReport {
    /// Paths uploaded to the remote.
    pub pushed: Vec<RelPath>,
    /// Paths downloaded from the remote.
    pub pulled: Vec<RelPath>,
    /// Paths deleted locally.
    pub deleted_local: Vec<RelPath>,
    /// Paths deleted on the remote.
    pub deleted_remote: Vec<RelPath>,
    /// Conflicts that were resolved.
    pub conflicts: Vec<ConflictRecord>,
}

impl SyncReport {
    /// Whether the sync changed anything at all.
    pub fn is_empty(&self) -> bool {
        self.pushed.is_empty()
            && self.pulled.is_empty()
            && self.deleted_local.is_empty()
            && self.deleted_remote.is_empty()
            && self.conflicts.is_empty()
    }
}

/// Compact counts for a sync run, suitable for status display and IPC.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct SyncSummary {
    /// Number of files pushed.
    pub pushed: usize,
    /// Number of files pulled.
    pub pulled: usize,
    /// Number of files deleted locally.
    pub deleted_local: usize,
    /// Number of files deleted on the remote.
    pub deleted_remote: usize,
    /// Number of conflicts resolved.
    pub conflicts: usize,
}

impl From<&SyncReport> for SyncSummary {
    fn from(r: &SyncReport) -> Self {
        SyncSummary {
            pushed: r.pushed.len(),
            pulled: r.pulled.len(),
            deleted_local: r.deleted_local.len(),
            deleted_remote: r.deleted_remote.len(),
            conflicts: r.conflicts.len(),
        }
    }
}

/// A stable, human-readable label for a sync group (`remote:remote_path`).
pub fn group_label(sync: &Sync) -> String {
    format!("{}:{}", sync.remote, sync.remote_path)
}

/// Convenience driver: build the backend, load the baseline index for `sync`
/// from `state_dir`, run a sync, and persist the updated index.
pub fn run(sync: &Sync, remote: &Remote, state_dir: &Path) -> Result<SyncReport> {
    let backend = crate::backend::build(remote, sync, state_dir)?;
    let index_path = Index::path_for(state_dir, sync);
    let mut index = Index::load(&index_path)?;
    let report = sync_group(sync, backend.as_ref(), &mut index)?;
    index.save(&index_path)?;
    Ok(report)
}

/// How a side changed relative to the baseline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Change {
    /// Present and identical to the baseline.
    Same,
    /// Present but differs from the baseline.
    Modified,
    /// Present with no baseline (newly appeared).
    Created,
    /// Absent but present in the baseline (deleted).
    Removed,
    /// Absent with no baseline.
    Gone,
}

/// The action chosen for a single path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Nothing,
    DropIndex,
    Push,
    Pull,
    DeleteLocal,
    DeleteRemote,
    /// Both sides modified: resolve by newer-wins.
    Conflict,
    /// Local modified, remote deleted: keep local (re-upload).
    ResurrectPush,
    /// Local deleted, remote modified: keep remote (restore locally).
    ResurrectPull,
    /// Pull-only path whose local copy diverged: overwrite, preserving local.
    PullPreserveLocal,
}

/// Reconcile and apply one sync group against `backend`, updating `index`.
pub fn sync_group(sync: &Sync, backend: &dyn Backend, index: &mut Index) -> Result<SyncReport> {
    let exclude = Matcher::new(&sync.effective_excludes(), false)?;
    let include = Matcher::new(&sync.include, true)?;

    let local = scan_local(&sync.root, &exclude)?;
    let mut remote = backend.snapshot()?;
    remote.retain(|path, _| !exclude.is_match(path));

    let mut report = SyncReport::default();

    for key in union_keys(&local, &remote, index) {
        let included = include.is_match(&key);
        if sync.pull_scope == PullScope::Include && !included {
            continue;
        }
        let pull_only = sync.direction == Direction::Pull || !included;

        let base = index.get(&key).cloned();
        let local_change = classify(
            local.get(&key).map(|m| m.hash.as_str()),
            base.as_ref().map(|b| b.local_hash.as_str()),
        );
        let remote_change = classify(
            remote.get(&key).map(|m| m.id.as_str()),
            base.as_ref().map(|b| b.remote_id.as_str()),
        );

        let action = if pull_only {
            decide_pull_only(local_change, remote_change, local.contains_key(&key))
        } else {
            decide_bidir(local_change, remote_change)
        };

        apply(
            sync,
            backend,
            index,
            &mut report,
            &key,
            action,
            &local,
            &remote,
        )?;
    }

    backend.finalize()?;
    Ok(report)
}

#[allow(clippy::too_many_arguments)]
fn apply(
    sync: &Sync,
    backend: &dyn Backend,
    index: &mut Index,
    report: &mut SyncReport,
    key: &RelPath,
    action: Action,
    local: &BTreeMap<RelPath, FileMeta>,
    remote: &crate::backend::RemoteSnapshot,
) -> Result<()> {
    let root = &sync.root;
    match action {
        Action::Nothing => {}
        Action::DropIndex => index.remove(key),

        Action::Push | Action::ResurrectPush => {
            let meta = local.get(key).expect("push requires a local file");
            let data = std::fs::read(key.to_local(root))?;
            let remote_meta = backend.write(key, &data)?;
            index.set(key, meta.hash.clone(), remote_meta.id);
            report.pushed.push(key.clone());
        }

        Action::Pull | Action::ResurrectPull => {
            let remote_meta = remote.get(key).expect("pull requires a remote file");
            let data = backend.read(key)?;
            write_local(root, key, &data)?;
            index.set(key, hash_bytes(&data), remote_meta.id.clone());
            report.pulled.push(key.clone());
        }

        Action::DeleteLocal => {
            remove_local(root, key)?;
            index.remove(key);
            report.deleted_local.push(key.clone());
        }

        Action::DeleteRemote => {
            backend.remove(key)?;
            index.remove(key);
            report.deleted_remote.push(key.clone());
        }

        Action::Conflict => {
            let local_meta = local.get(key).expect("conflict requires a local file");
            let remote_meta = remote.get(key).expect("conflict requires a remote file");
            let remote_data = backend.read(key)?;
            let remote_hash = hash_bytes(&remote_data);

            if remote_hash == local_meta.hash {
                // Both sides reached the same content independently — not a real conflict.
                index.set(key, local_meta.hash.clone(), remote_meta.id.clone());
            } else if newer_is_local(local_meta, remote_meta) {
                let copy = write_conflict_copy(root, key, &remote_data)?;
                let local_data = std::fs::read(key.to_local(root))?;
                let written = backend.write(key, &local_data)?;
                index.set(key, local_meta.hash.clone(), written.id);
                report.conflicts.push(ConflictRecord {
                    path: key.clone(),
                    winner: Winner::Local,
                    conflict_copy: copy,
                });
            } else {
                let local_data = std::fs::read(key.to_local(root))?;
                let copy = write_conflict_copy(root, key, &local_data)?;
                write_local(root, key, &remote_data)?;
                index.set(key, remote_hash, remote_meta.id.clone());
                report.conflicts.push(ConflictRecord {
                    path: key.clone(),
                    winner: Winner::Remote,
                    conflict_copy: copy,
                });
            }
        }

        Action::PullPreserveLocal => {
            let remote_meta = remote.get(key).expect("pull requires a remote file");
            let remote_data = backend.read(key)?;
            let remote_hash = hash_bytes(&remote_data);
            let full = key.to_local(root);
            if let Ok(local_data) = std::fs::read(&full) {
                if hash_bytes(&local_data) != remote_hash {
                    let copy = write_conflict_copy(root, key, &local_data)?;
                    report.conflicts.push(ConflictRecord {
                        path: key.clone(),
                        winner: Winner::Remote,
                        conflict_copy: copy,
                    });
                }
            }
            write_local(root, key, &remote_data)?;
            index.set(key, remote_hash, remote_meta.id.clone());
            report.pulled.push(key.clone());
        }
    }
    Ok(())
}

fn union_keys(
    local: &BTreeMap<RelPath, FileMeta>,
    remote: &crate::backend::RemoteSnapshot,
    index: &Index,
) -> BTreeSet<RelPath> {
    let mut keys: BTreeSet<RelPath> = BTreeSet::new();
    keys.extend(local.keys().cloned());
    keys.extend(remote.keys().cloned());
    for key in index.entries.keys() {
        if let Some(rel) = RelPath::from_relative(Path::new(key)) {
            keys.insert(rel);
        }
    }
    keys
}

fn classify(current: Option<&str>, baseline: Option<&str>) -> Change {
    match (current, baseline) {
        (Some(c), Some(b)) if c == b => Change::Same,
        (Some(_), Some(_)) => Change::Modified,
        (Some(_), None) => Change::Created,
        (None, Some(_)) => Change::Removed,
        (None, None) => Change::Gone,
    }
}

fn decide_bidir(local: Change, remote: Change) -> Action {
    use Change::*;
    match (local, remote) {
        (Same, Same) => Action::Nothing,
        (Created, Gone) | (Modified, Same) => Action::Push,
        (Removed, Same) => Action::DeleteRemote,
        (Gone, Created) | (Same, Modified) => Action::Pull,
        (Same, Removed) => Action::DeleteLocal,
        (Removed, Removed) => Action::DropIndex,
        (Modified, Removed) => Action::ResurrectPush,
        (Removed, Modified) => Action::ResurrectPull,
        // Both sides moved (or an inconsistent baseline): resolve as a conflict.
        (Created, Created) | (Modified, Modified) | (Created, Modified) | (Modified, Created) => {
            Action::Conflict
        }
        _ => Action::Nothing,
    }
}

fn decide_pull_only(local: Change, remote: Change, local_present: bool) -> Action {
    use Change::*;
    match remote {
        Modified | Created => {
            if local_present && matches!(local, Modified | Created) {
                Action::PullPreserveLocal
            } else {
                Action::Pull
            }
        }
        Removed => {
            if local == Same {
                Action::DeleteLocal
            } else {
                Action::Nothing
            }
        }
        // Remote unchanged: restore a tracked file the user deleted, else leave local alone.
        Same => {
            if local == Removed {
                Action::Pull
            } else {
                Action::Nothing
            }
        }
        Gone => Action::Nothing,
    }
}

/// Newer-wins: local wins ties and cases where the remote has no known mtime.
fn newer_is_local(local: &FileMeta, remote: &crate::model::RemoteMeta) -> bool {
    let remote_mtime = remote.mtime.unwrap_or(UNIX_EPOCH);
    local.mtime >= remote_mtime
}

fn scan_local(root: &Path, exclude: &Matcher) -> Result<BTreeMap<RelPath, FileMeta>> {
    let mut map = BTreeMap::new();
    if !root.exists() {
        return Ok(map);
    }
    for entry in walkdir::WalkDir::new(root).follow_links(false) {
        let entry = entry.map_err(std::io::Error::from)?;
        if !entry.file_type().is_file() {
            continue;
        }
        let Some(rel) = RelPath::from_base(root, entry.path()) else {
            continue;
        };
        if exclude.is_match(&rel) {
            continue;
        }
        let data = std::fs::read(entry.path())?;
        let meta = entry.metadata().map_err(std::io::Error::from)?;
        map.insert(
            rel,
            FileMeta {
                hash: hash_bytes(&data),
                mtime: meta.modified().unwrap_or(UNIX_EPOCH),
                size: meta.len(),
            },
        );
    }
    Ok(map)
}

fn write_local(root: &Path, key: &RelPath, data: &[u8]) -> Result<()> {
    let full = key.to_local(root);
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&full, data)?;
    Ok(())
}

fn remove_local(root: &Path, key: &RelPath) -> Result<()> {
    let full = key.to_local(root);
    match std::fs::remove_file(&full) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Write `data` next to the original file as `<stem>.conflux-conflict-<epoch><.ext>`.
fn write_conflict_copy(root: &Path, key: &RelPath, data: &[u8]) -> Result<PathBuf> {
    let full = key.to_local(root);
    let dir = full.parent().map(Path::to_path_buf).unwrap_or_default();
    let stem = full.file_stem().and_then(|s| s.to_str()).unwrap_or("file");
    let ts = now_secs();
    let name = match full.extension().and_then(|e| e.to_str()) {
        Some(ext) => format!("{stem}.conflux-conflict-{ts}.{ext}"),
        None => format!("{stem}.conflux-conflict-{ts}"),
    };
    let path = dir.join(name);
    std::fs::create_dir_all(&dir)?;
    std::fs::write(&path, data)?;
    Ok(path)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
