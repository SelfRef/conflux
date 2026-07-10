//! Per-group baseline state: what each file looked like (local hash + remote id)
//! at the end of the last successful sync. The three-way reconciliation in the
//! engine diffs the current local and remote views against this baseline.

use crate::config::Sync;
use crate::error::Result;
use crate::relpath::RelPath;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The baseline recorded for a single file at the last sync.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexEntry {
    /// Local content hash (blake3 hex) at last sync.
    pub local_hash: String,
    /// Remote change id at last sync.
    pub remote_id: String,
}

/// The baseline index for one sync group, keyed by `RelPath` string.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Index {
    /// Per-file baseline entries.
    pub entries: BTreeMap<String, IndexEntry>,
    /// Empty directories known to be synced at the last run (only populated in
    /// `empty_dirs = mirror` mode; used to propagate empty-dir deletions).
    #[serde(default)]
    pub dirs: std::collections::BTreeSet<String>,
}

impl Index {
    /// Compute the on-disk path of a group's index inside `state_dir`.
    pub fn path_for(state_dir: &Path, sync: &Sync) -> PathBuf {
        let id = group_id(sync);
        state_dir.join("index").join(format!("{id}.json"))
    }

    /// Load an index from disk, returning an empty index if the file is absent.
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read(path) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Index::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// Atomically write the index to disk (write temp file, then rename).
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(self)?;
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Look up the baseline for a path.
    pub fn get(&self, path: &RelPath) -> Option<&IndexEntry> {
        self.entries.get(path.as_str())
    }

    /// Record/replace the baseline for a path.
    pub fn set(&mut self, path: &RelPath, local_hash: String, remote_id: String) {
        self.entries.insert(
            path.as_str().to_string(),
            IndexEntry {
                local_hash,
                remote_id,
            },
        );
    }

    /// Remove the baseline for a path (after a delete propagates).
    pub fn remove(&mut self, path: &RelPath) {
        self.entries.remove(path.as_str());
    }
}

/// Derive a filesystem-safe id for a sync group from its remote + remote path.
fn group_id(sync: &Sync) -> String {
    let raw = format!("{}__{}", sync.remote, sync.remote_path);
    raw.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}
