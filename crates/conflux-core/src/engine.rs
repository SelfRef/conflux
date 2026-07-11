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
use crate::model::{EmptyDirMode, FileMeta, Scope};
use crate::relpath::RelPath;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::debug;

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

/// What a sync did, classified by outcome (not by direction). Files and empty
/// directories are counted together: a newly-mirrored empty dir is `added`, a
/// pruned one is `removed`.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct SyncReport {
    /// Files/dirs that newly appeared (no prior baseline) on either side.
    pub added: Vec<RelPath>,
    /// Files/dirs that were deleted on either side.
    pub removed: Vec<RelPath>,
    /// Existing files whose content changed on either side.
    pub modified: Vec<RelPath>,
    /// Conflicts that were resolved (kept as records so the preserved copy can
    /// be logged).
    pub conflicts: Vec<ConflictRecord>,
}

impl SyncReport {
    /// Whether the sync changed anything at all.
    pub fn is_empty(&self) -> bool {
        self.added.is_empty()
            && self.removed.is_empty()
            && self.modified.is_empty()
            && self.conflicts.is_empty()
    }

    /// Record a written file/dir as `added` (no baseline) or `modified`.
    fn record_write(&mut self, key: &RelPath, had_baseline: bool) {
        if had_baseline {
            self.modified.push(key.clone());
        } else {
            self.added.push(key.clone());
        }
    }
}

/// Compact counts for a sync run, suitable for status display and IPC. Rendered
/// as `+added -removed *modified ~conflicted`.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct SyncSummary {
    /// Files/dirs added.
    pub added: usize,
    /// Files/dirs removed.
    pub removed: usize,
    /// Files modified.
    pub modified: usize,
    /// Conflicts resolved.
    pub conflicted: usize,
}

impl std::fmt::Display for SyncSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "+{} -{} *{} ~{}",
            self.added, self.removed, self.modified, self.conflicted
        )
    }
}

impl From<&SyncReport> for SyncSummary {
    fn from(r: &SyncReport) -> Self {
        SyncSummary {
            added: r.added.len(),
            removed: r.removed.len(),
            modified: r.modified.len(),
            conflicted: r.conflicts.len(),
        }
    }
}

/// A stable, human-readable label for a sync group (`remote:remote_path`, or
/// just `remote` when the group maps to the remote's root).
pub fn group_label(sync: &Sync) -> String {
    if sync.remote_path.is_empty() {
        sync.remote.clone()
    } else {
        format!("{}:{}", sync.remote, sync.remote_path)
    }
}

/// Convenience driver: build the backend, load the baseline index for `sync`
/// from `state_dir`, run a sync, and persist the updated index.
///
/// `remote_refresh` marks a periodic `pull_interval` poll: for a bidirectional
/// group it applies only remote-originated changes and leaves local-side changes
/// for the normal trigger, so it never fights the bidirectional reconcile.
pub fn run(
    sync: &Sync,
    remote: &Remote,
    state_dir: &Path,
    remote_refresh: bool,
    empty_dirs: EmptyDirMode,
    max_file_size: u64,
    exclude_defaults: &[String],
) -> Result<SyncReport> {
    let backend = crate::backend::build(remote, sync, state_dir)?;
    let index_path = Index::path_for(state_dir, sync);
    let mut index = Index::load(&index_path)?;
    let report = sync_group(
        sync,
        backend.as_ref(),
        &mut index,
        remote_refresh,
        empty_dirs,
        sync.scope,
        max_file_size,
        exclude_defaults,
    )?;
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
    /// Push-only path whose remote copy diverged: overwrite, preserving the
    /// remote version as a local conflict copy.
    PushPreserveRemote,
}

/// How a single path is synced, derived from the group's [`Scope`] and the
/// path's `include` membership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyMode {
    /// Ignore the path entirely (never push, pull, or delete).
    Skip,
    /// Reconcile both sides (creates, edits, deletes, conflicts).
    Bidir,
    /// Only bring remote changes down; local edits never reach the remote.
    PullOnly,
    /// Only push local changes up; remote edits never reach local.
    PushOnly,
}

/// Resolve how one path is synced from the group's scope and the path's
/// `include` membership. `mirror` ignores `include` (everything is
/// bidirectional); the other scopes make included paths bidirectional and treat
/// the rest per scope: `include` skips them, `remote` pulls them, `local`
/// pushes them.
fn key_mode(scope: Scope, included: bool) -> KeyMode {
    match scope {
        Scope::Mirror => KeyMode::Bidir,
        _ if included => KeyMode::Bidir,
        Scope::Include => KeyMode::Skip,
        Scope::Remote => KeyMode::PullOnly,
        Scope::Local => KeyMode::PushOnly,
    }
}

/// If `action` would transfer a file whose size exceeds `limit`, return that
/// size (so the caller can skip and log it). `limit == 0` means no cap. Actions
/// that move no bytes (deletes, index-only, nothing) are never capped. A
/// conflict transfers both ways, so the larger side is checked.
fn oversize(
    action: Action,
    local: &BTreeMap<RelPath, FileMeta>,
    remote: &crate::backend::RemoteSnapshot,
    key: &RelPath,
    limit: u64,
) -> Option<u64> {
    if limit == 0 {
        return None;
    }
    let size = match action {
        Action::Push | Action::ResurrectPush | Action::PushPreserveRemote => {
            local.get(key).map(|m| m.size)?
        }
        Action::Pull | Action::ResurrectPull | Action::PullPreserveLocal => {
            remote.get(key).map(|m| m.size)?
        }
        Action::Conflict => {
            let l = local.get(key).map_or(0, |m| m.size);
            let r = remote.get(key).map_or(0, |m| m.size);
            l.max(r)
        }
        Action::Nothing | Action::DropIndex | Action::DeleteLocal | Action::DeleteRemote => {
            return None
        }
    };
    (size > limit).then_some(size)
}

/// Reconcile and apply one sync group against `backend`, updating `index`.
///
/// When `remote_refresh` is set (a periodic `pull_interval` poll), bidirectional
/// files only follow remote-originated changes; see [`decide_remote_refresh`].
#[allow(clippy::too_many_arguments)]
pub fn sync_group(
    sync: &Sync,
    backend: &dyn Backend,
    index: &mut Index,
    remote_refresh: bool,
    empty_dirs: EmptyDirMode,
    scope: Scope,
    max_file_size: u64,
    exclude_defaults: &[String],
) -> Result<SyncReport> {
    let exclude = Matcher::new(&sync.effective_excludes(exclude_defaults), false)?;
    // Empty `include` matches nothing; `["**"]` matches everything.
    let include = Matcher::new(&sync.include, false)?;

    let local = scan_local(&sync.local, &exclude)?;
    let mut remote = backend.snapshot()?;
    remote.retain(|path, _| !exclude.is_match(path));

    let mut report = SyncReport::default();

    for key in union_keys(&local, &remote, index) {
        let mode = key_mode(scope, include.is_match(&key));
        if mode == KeyMode::Skip {
            continue;
        }

        let base = index.get(&key).cloned();
        let local_change = classify(
            local.get(&key).map(|m| m.hash.as_str()),
            base.as_ref().map(|b| b.local_hash.as_str()),
        );
        let remote_change = classify(
            remote.get(&key).map(|m| m.id.as_str()),
            base.as_ref().map(|b| b.remote_id.as_str()),
        );

        let action = match mode {
            // Bidirectional file during a background pull: apply only remote-side
            // changes; leave local edits/deletes to the bidirectional trigger.
            KeyMode::Bidir if remote_refresh => decide_remote_refresh(local_change, remote_change),
            KeyMode::Bidir => decide_bidir(local_change, remote_change),
            KeyMode::PullOnly => {
                decide_pull_only(local_change, remote_change, local.contains_key(&key))
            }
            // A background remote poll never pushes, so a push-only file is left
            // for its normal (local-driven) trigger.
            KeyMode::PushOnly if remote_refresh => Action::Nothing,
            KeyMode::PushOnly => {
                decide_push_only(local_change, remote_change, remote.contains_key(&key))
            }
            KeyMode::Skip => unreachable!("skipped above"),
        };

        // Skip files whose transfer would exceed the configured size cap, in
        // either direction. The index is left untouched, so the file simply
        // stays unsynced until it shrinks or the cap is raised.
        if let Some(size) = oversize(action, &local, &remote, &key, max_file_size) {
            debug!(
                group = %group_label(sync),
                path = %key,
                size,
                limit = max_file_size,
                "skipping file larger than max_file_size"
            );
            continue;
        }

        apply(
            sync,
            backend,
            index,
            &mut report,
            &key,
            action,
            &local,
            &remote,
            base.is_some(),
        )?;
    }

    // Empty-directory handling runs after files are applied (so emptiness
    // reflects the just-synced tree) and only when the backend supports it.
    if empty_dirs != EmptyDirMode::Ignore && backend.supports_empty_dirs() {
        reconcile_dirs(sync, backend, index, &mut report, empty_dirs)?;
    }

    backend.finalize()?;
    Ok(report)
}

/// Handle empty directories after the file reconciliation, per `empty_dirs` mode.
fn reconcile_dirs(
    sync: &Sync,
    backend: &dyn Backend,
    index: &mut Index,
    report: &mut SyncReport,
    mode: EmptyDirMode,
) -> Result<()> {
    match mode {
        EmptyDirMode::Ignore => Ok(()),
        EmptyDirMode::Mirror => mirror_dirs(sync, backend, index, report),
        EmptyDirMode::Prune => prune_dirs(sync, backend, report),
    }
}

/// `mirror`: mirror empty directories to both sides and propagate their creation
/// and deletion, using `index.dirs` as the baseline of previously-synced dirs.
fn mirror_dirs(
    sync: &Sync,
    backend: &dyn Backend,
    index: &mut Index,
    report: &mut SyncReport,
) -> Result<()> {
    let local = crate::backend::walk_empty_dirs(&sync.local)?;
    let remote = backend.snapshot_dirs()?;

    let mut union: BTreeSet<RelPath> = BTreeSet::new();
    union.extend(local.iter().cloned());
    union.extend(remote.iter().cloned());
    for d in &index.dirs {
        if let Some(rel) = RelPath::from_relative(Path::new(d)) {
            union.insert(rel);
        }
    }

    for dir in union {
        let lp = local.contains(&dir);
        let rp = remote.contains(&dir);
        let base = index.dirs.contains(dir.as_str());
        match (lp, rp, base) {
            // Present on both sides: ensure it stays tracked.
            (true, true, _) => {
                index.dirs.insert(dir.as_str().to_string());
            }
            // Newly created on one side: create on the other.
            (true, false, false) => {
                backend.create_dir(&dir)?;
                index.dirs.insert(dir.as_str().to_string());
                report.added.push(dir);
            }
            (false, true, false) => {
                write_local_dir(&sync.local, &dir)?;
                index.dirs.insert(dir.as_str().to_string());
                report.added.push(dir);
            }
            // Deleted on one side (was synced): delete on the other.
            (true, false, true) => {
                remove_local_dir(&sync.local, &dir)?;
                index.dirs.remove(dir.as_str());
                report.removed.push(dir);
            }
            (false, true, true) => {
                backend.remove_dir(&dir)?;
                index.dirs.remove(dir.as_str());
                report.removed.push(dir);
            }
            // Gone from both sides.
            (false, false, _) => {
                index.dirs.remove(dir.as_str());
            }
        }
    }
    Ok(())
}

/// `remove`: prune empty directories from both sides. Repeats until stable so a
/// parent that becomes empty once its empty child is removed is pruned too.
fn prune_dirs(sync: &Sync, backend: &dyn Backend, report: &mut SyncReport) -> Result<()> {
    // Bound the passes by nesting depth; each pass removes at least the deepest
    // empties, so a fixpoint is reached well within this many rounds.
    for _ in 0..64 {
        let mut removed_any = false;

        for dir in crate::backend::walk_empty_dirs(&sync.local)? {
            remove_local_dir(&sync.local, &dir)?;
            report.removed.push(dir);
            removed_any = true;
        }
        for dir in backend.snapshot_dirs()? {
            backend.remove_dir(&dir)?;
            report.removed.push(dir);
            removed_any = true;
        }

        if !removed_any {
            break;
        }
    }
    Ok(())
}

fn write_local_dir(root: &Path, key: &RelPath) -> Result<()> {
    std::fs::create_dir_all(key.to_local(root))?;
    Ok(())
}

fn remove_local_dir(root: &Path, key: &RelPath) -> Result<()> {
    match std::fs::remove_dir(key.to_local(root)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
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
    had_baseline: bool,
) -> Result<()> {
    let root = &sync.local;
    match action {
        Action::Nothing => {}
        Action::DropIndex => index.remove(key),

        Action::Push | Action::ResurrectPush => {
            let meta = local.get(key).expect("push requires a local file");
            let data = std::fs::read(key.to_local(root))?;
            let remote_meta = backend.write(key, &data, Some(meta.mtime))?;
            index.set(key, meta.hash.clone(), remote_meta.id);
            report.record_write(key, had_baseline);
        }

        Action::Pull | Action::ResurrectPull => {
            let remote_meta = remote.get(key).expect("pull requires a remote file");
            let data = backend.read(key)?;
            write_local(root, key, &data, remote_meta.mtime)?;
            index.set(key, hash_bytes(&data), remote_meta.id.clone());
            report.record_write(key, had_baseline);
        }

        Action::DeleteLocal => {
            remove_local(root, key)?;
            index.remove(key);
            report.removed.push(key.clone());
        }

        Action::DeleteRemote => {
            backend.remove(key)?;
            index.remove(key);
            report.removed.push(key.clone());
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
                let written = backend.write(key, &local_data, Some(local_meta.mtime))?;
                index.set(key, local_meta.hash.clone(), written.id);
                report.conflicts.push(ConflictRecord {
                    path: key.clone(),
                    winner: Winner::Local,
                    conflict_copy: copy,
                });
            } else {
                let local_data = std::fs::read(key.to_local(root))?;
                let copy = write_conflict_copy(root, key, &local_data)?;
                write_local(root, key, &remote_data, remote_meta.mtime)?;
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
            let mut conflicted = false;
            if let Ok(local_data) = std::fs::read(&full) {
                if hash_bytes(&local_data) != remote_hash {
                    let copy = write_conflict_copy(root, key, &local_data)?;
                    report.conflicts.push(ConflictRecord {
                        path: key.clone(),
                        winner: Winner::Remote,
                        conflict_copy: copy,
                    });
                    conflicted = true;
                }
            }
            write_local(root, key, &remote_data, remote_meta.mtime)?;
            index.set(key, remote_hash, remote_meta.id.clone());
            // If a divergent local copy was preserved it's a conflict; otherwise
            // it's a plain pull counted as added/modified.
            if !conflicted {
                report.record_write(key, had_baseline);
            }
        }

        Action::PushPreserveRemote => {
            let local_meta = local.get(key).expect("push requires a local file");
            let local_data = std::fs::read(key.to_local(root))?;
            let mut conflicted = false;
            // If the remote diverged, preserve its version as a local conflict
            // copy before overwriting the remote with the local content.
            if remote.get(key).is_some() {
                let remote_data = backend.read(key)?;
                if hash_bytes(&remote_data) != local_meta.hash {
                    let copy = write_conflict_copy(root, key, &remote_data)?;
                    report.conflicts.push(ConflictRecord {
                        path: key.clone(),
                        winner: Winner::Local,
                        conflict_copy: copy,
                    });
                    conflicted = true;
                }
            }
            let written = backend.write(key, &local_data, Some(local_meta.mtime))?;
            index.set(key, local_meta.hash.clone(), written.id);
            if !conflicted {
                report.record_write(key, had_baseline);
            }
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

/// Decide the action for a **remote-refresh** run: a periodic `pull_interval`
/// poll of a bidirectional group. It applies only remote-originated changes and
/// defers every local-side change to the normal bidirectional trigger — so a
/// background pull can never resurrect a file the user just deleted locally
/// (the bug where the first local delete was undone by a racing pull).
fn decide_remote_refresh(local: Change, remote: Change) -> Action {
    use Change::*;
    match (local, remote) {
        // Remote changed while local stayed put: bring the remote change down.
        (Same, Modified) | (Gone, Created) => Action::Pull,
        (Same, Removed) => Action::DeleteLocal,
        // Local-side changes, or both sides changed: leave it to the
        // bidirectional trigger (which pushes, deletes, or conflict-resolves).
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

/// The mirror of [`decide_pull_only`] for a push-only path (`scope = "local"`):
/// local changes flow up, remote changes never come down. Keyed on the local
/// change, with the remote in the passenger seat.
fn decide_push_only(local: Change, remote: Change, remote_present: bool) -> Action {
    use Change::*;
    match local {
        Modified | Created => {
            if remote_present && matches!(remote, Modified | Created) {
                Action::PushPreserveRemote
            } else {
                Action::Push
            }
        }
        Removed => {
            if remote == Same {
                Action::DeleteRemote
            } else {
                Action::Nothing
            }
        }
        // Local unchanged: restore a tracked file deleted on the remote, else leave it.
        Same => {
            if remote == Removed {
                Action::Push
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

fn write_local(root: &Path, key: &RelPath, data: &[u8], mtime: Option<SystemTime>) -> Result<()> {
    let full = key.to_local(root);
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&full, data)?;
    // Preserve the remote's modification time so the copy matches 1:1.
    if let Some(mtime) = mtime {
        if let Ok(f) = std::fs::File::options().write(true).open(&full) {
            let _ = f.set_modified(mtime);
        }
    }
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

/// Write `data` next to the original file as
/// `<stem>.conflux-conflict-YYYY-MM-DD_HH-MM-SS<.ext>`.
fn write_conflict_copy(root: &Path, key: &RelPath, data: &[u8]) -> Result<PathBuf> {
    let full = key.to_local(root);
    let dir = full.parent().map(Path::to_path_buf).unwrap_or_default();
    let stem = full.file_stem().and_then(|s| s.to_str()).unwrap_or("file");
    let ts = crate::timefmt::stamp(now_secs());
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

#[cfg(test)]
mod tests {
    use super::Change::*;
    use super::*;

    #[test]
    fn bidirectional_local_delete_propagates_to_remote() {
        // The real (watch/timer/manual) trigger deletes a locally-removed file
        // from the remote instead of resurrecting it.
        assert_eq!(decide_bidir(Removed, Same), Action::DeleteRemote);
    }

    #[test]
    fn remote_refresh_never_resurrects_a_local_delete() {
        // Regression: a background `pull_interval` poll used to run pull-only,
        // so (local Removed, remote Same) resolved to Pull and restored the file
        // the user just deleted — undoing the pending bidirectional delete.
        assert_eq!(decide_remote_refresh(Removed, Same), Action::Nothing);
        // It also leaves other local-only changes alone.
        assert_eq!(decide_remote_refresh(Modified, Same), Action::Nothing);
        assert_eq!(decide_remote_refresh(Created, Gone), Action::Nothing);
        // ...and defers both-sides-changed to the bidirectional trigger.
        assert_eq!(decide_remote_refresh(Modified, Modified), Action::Nothing);
    }

    #[test]
    fn remote_refresh_still_applies_remote_side_changes() {
        assert_eq!(decide_remote_refresh(Same, Modified), Action::Pull);
        assert_eq!(decide_remote_refresh(Gone, Created), Action::Pull);
        assert_eq!(decide_remote_refresh(Same, Removed), Action::DeleteLocal);
    }

    #[test]
    fn pull_only_group_still_restores_a_local_delete() {
        // Unchanged behavior for genuinely pull-only groups: local is a mirror,
        // so a deleted file is restored from the remote on the next pull.
        assert_eq!(decide_pull_only(Removed, Same, false), Action::Pull);
    }

    #[test]
    fn push_only_mirrors_pull_only_with_the_sides_swapped() {
        // Local changes flow up; remote changes never come down.
        assert_eq!(decide_push_only(Created, Gone, false), Action::Push);
        assert_eq!(decide_push_only(Modified, Same, true), Action::Push);
        // Both sides diverged: local wins, remote kept as a local conflict copy.
        assert_eq!(
            decide_push_only(Modified, Modified, true),
            Action::PushPreserveRemote
        );
        // A tracked file deleted on the remote is restored by re-pushing.
        assert_eq!(decide_push_only(Same, Removed, false), Action::Push);
        // A local delete propagates to the remote.
        assert_eq!(decide_push_only(Removed, Same, true), Action::DeleteRemote);
        // Remote-only changes are ignored.
        assert_eq!(decide_push_only(Same, Modified, true), Action::Nothing);
    }

    #[test]
    fn key_mode_include_scope_skips_non_included_paths() {
        use Scope::*;
        // Included paths sync both ways; the rest are ignored entirely.
        assert_eq!(key_mode(Include, true), KeyMode::Bidir);
        assert_eq!(key_mode(Include, false), KeyMode::Skip);
    }

    #[test]
    fn key_mode_remote_scope_pulls_non_included_paths() {
        use Scope::*;
        assert_eq!(key_mode(Remote, true), KeyMode::Bidir);
        assert_eq!(key_mode(Remote, false), KeyMode::PullOnly);
    }

    #[test]
    fn key_mode_local_scope_pushes_non_included_paths() {
        use Scope::*;
        assert_eq!(key_mode(Local, true), KeyMode::Bidir);
        assert_eq!(key_mode(Local, false), KeyMode::PushOnly);
    }

    #[test]
    fn key_mode_mirror_scope_ignores_include() {
        use Scope::*;
        assert_eq!(key_mode(Mirror, false), KeyMode::Bidir);
        assert_eq!(key_mode(Mirror, true), KeyMode::Bidir);
    }
}
