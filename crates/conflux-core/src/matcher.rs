//! Glob matching for `include` (push scope) and `exclude` patterns.
//!
//! A bare directory pattern like `nvim` is expanded to also match everything
//! beneath it (`nvim/**`), so listing a directory includes its whole subtree.

use crate::error::{Error, Result};
use crate::relpath::RelPath;
use globset::{Glob, GlobSet, GlobSetBuilder};

/// A compiled set of glob patterns.
pub struct Matcher {
    set: GlobSet,
    /// When the pattern list is empty, `is_match` returns this.
    empty_default: bool,
}

impl Matcher {
    /// Build a matcher. When `patterns` is empty, `is_match` returns `empty_default`
    /// (used so that an empty `include` means "everything" but an empty `exclude`
    /// means "nothing").
    pub fn new(patterns: &[String], empty_default: bool) -> Result<Self> {
        let mut builder = GlobSetBuilder::new();
        for pattern in patterns {
            builder.add(compile(pattern)?);
            // Also match the subtree under a directory-style pattern.
            builder.add(compile(&format!("{}/**", pattern.trim_end_matches('/')))?);
        }
        let set = builder
            .build()
            .map_err(|e| Error::Validation(format!("invalid glob set: {e}")))?;
        Ok(Matcher {
            set,
            empty_default: patterns.is_empty() && empty_default,
        })
    }

    /// Whether `path` matches any pattern (or the empty-default).
    pub fn is_match(&self, path: &RelPath) -> bool {
        if self.set.is_empty() {
            return self.empty_default;
        }
        self.set.is_match(path.as_str())
    }
}

fn compile(pattern: &str) -> Result<Glob> {
    Glob::new(pattern).map_err(|e| Error::Validation(format!("invalid glob `{pattern}`: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn rel(s: &str) -> RelPath {
        RelPath::from_relative(Path::new(s)).unwrap()
    }

    #[test]
    fn directory_pattern_matches_subtree() {
        let m = Matcher::new(&["nvim".to_string()], true).unwrap();
        assert!(m.is_match(&rel("nvim")));
        assert!(m.is_match(&rel("nvim/init.lua")));
        assert!(!m.is_match(&rel("fish/config.fish")));
    }

    #[test]
    fn empty_include_matches_all_but_empty_exclude_matches_none() {
        let include = Matcher::new(&[], true).unwrap();
        let exclude = Matcher::new(&[], false).unwrap();
        assert!(include.is_match(&rel("anything")));
        assert!(!exclude.is_match(&rel("anything")));
    }

    #[test]
    fn conflict_glob_is_excluded() {
        let m = Matcher::new(&["*.conflux-conflict-*".to_string()], false).unwrap();
        assert!(m.is_match(&rel("init.lua.conflux-conflict-1700000000")));
        assert!(!m.is_match(&rel("init.lua")));
    }
}
