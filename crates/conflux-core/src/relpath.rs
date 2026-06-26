//! A normalized, forward-slash relative path used as the shared key for a file
//! in both the local tree (under `root`) and the remote tree (under `remote_path`).

use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};

/// A relative path with `/` separators, no leading slash, and no `.`/`..` parts.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RelPath(String);

impl RelPath {
    /// Build a `RelPath` from `full` interpreted as relative to `root`.
    ///
    /// Returns `None` if `full` is not under `root` or escapes it via `..`.
    pub fn from_base(root: &Path, full: &Path) -> Option<Self> {
        let rel = full.strip_prefix(root).ok()?;
        Self::from_relative(rel)
    }

    /// Build a `RelPath` from an already-relative path, normalizing separators.
    pub fn from_relative(rel: &Path) -> Option<Self> {
        let mut parts = Vec::new();
        for comp in rel.components() {
            match comp {
                Component::Normal(p) => parts.push(p.to_str()?.to_string()),
                Component::CurDir => {}
                // Anything that escapes the root (or is absolute) is rejected.
                _ => return None,
            }
        }
        if parts.is_empty() {
            return None;
        }
        Some(RelPath(parts.join("/")))
    }

    /// The path as a `/`-separated string.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Resolve this path against a local root directory.
    pub fn to_local(&self, root: &Path) -> PathBuf {
        let mut p = root.to_path_buf();
        for part in self.0.split('/') {
            p.push(part);
        }
        p
    }
}

impl std::fmt::Display for RelPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_root_and_normalizes() {
        let root = Path::new("/home/me/.config");
        let full = Path::new("/home/me/.config/nvim/init.lua");
        let rel = RelPath::from_base(root, full).unwrap();
        assert_eq!(rel.as_str(), "nvim/init.lua");
        assert_eq!(rel.to_local(root), full);
    }

    #[test]
    fn rejects_paths_outside_root() {
        assert!(RelPath::from_relative(Path::new("../escape")).is_none());
        assert!(RelPath::from_base(Path::new("/a"), Path::new("/b/c")).is_none());
    }
}
