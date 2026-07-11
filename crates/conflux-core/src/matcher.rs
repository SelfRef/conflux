//! Glob matching for `include` (what a group syncs) and `exclude` patterns.
//!
//! Patterns are matched with `*` bound to a single path segment and `**`
//! spanning across separators (standard gitignore-style semantics):
//!   * `*.conf`     matches top-level `.conf` files only
//!   * `**/*.conf`  matches `.conf` files at any depth
//!   * `fish/*`     matches the immediate children of `fish`
//!   * `fish/**`    matches the whole `fish` subtree
//!
//! A *literal* directory pattern like `nvim` (no glob metacharacters) is
//! additionally expanded to match everything beneath it (`nvim/**`), so simply
//! listing a directory name includes its whole subtree. Patterns that already
//! contain wildcards are used verbatim, so `*` and `**` mean exactly what they
//! say.

use crate::error::{Error, Result};
use crate::relpath::RelPath;
use globset::{Glob, GlobBuilder, GlobSet, GlobSetBuilder};

/// A compiled set of glob patterns.
pub struct Matcher {
    set: GlobSet,
    /// When the pattern list is empty, `is_match` returns this.
    empty_default: bool,
}

impl Matcher {
    /// Build a matcher. When `patterns` is empty, `is_match` returns `empty_default`:
    /// callers pass `false` to match nothing on an empty list (as `include` and
    /// `exclude` both do) or `true` to match everything.
    pub fn new(patterns: &[String], empty_default: bool) -> Result<Self> {
        let mut builder = GlobSetBuilder::new();
        for pattern in patterns {
            builder.add(compile(pattern)?);
            // A literal directory name (no wildcards) also matches its whole
            // subtree, so `nvim` covers `nvim/init.lua`. Wildcard patterns are
            // left as written so `*` stays one segment and `**` stays recursive.
            if is_literal(pattern) {
                builder.add(compile(&format!("{}/**", pattern.trim_end_matches('/')))?);
            }
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

/// Whether a pattern contains no glob metacharacters (so it names a literal path).
fn is_literal(pattern: &str) -> bool {
    !pattern.contains(['*', '?', '[', ']', '{', '}'])
}

fn compile(pattern: &str) -> Result<Glob> {
    // `literal_separator(true)` binds `*`/`?` to a single path segment so they
    // do not cross `/`; only `**` spans directories.
    GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .map_err(|e| Error::Validation(format!("invalid glob `{pattern}`: {e}")))
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
    fn single_star_matches_one_segment_only() {
        let m = Matcher::new(&["fish/*".to_string()], true).unwrap();
        assert!(m.is_match(&rel("fish/config.fish")));
        // `*` does not cross a separator, so nested files are not matched.
        assert!(!m.is_match(&rel("fish/functions/greet.fish")));
    }

    #[test]
    fn double_star_matches_across_segments() {
        let m = Matcher::new(&["fish/**".to_string()], true).unwrap();
        assert!(m.is_match(&rel("fish/config.fish")));
        assert!(m.is_match(&rel("fish/functions/greet.fish")));
    }

    #[test]
    fn leading_double_star_matches_any_depth() {
        let m = Matcher::new(&["**/*.conf".to_string()], true).unwrap();
        assert!(m.is_match(&rel("kitty.conf")));
        assert!(m.is_match(&rel("kitty/kitty.conf")));
        assert!(m.is_match(&rel("a/b/c.conf")));
        assert!(!m.is_match(&rel("kitty/theme.toml")));
    }

    #[test]
    fn bare_star_matches_top_level_only() {
        let m = Matcher::new(&["*.conf".to_string()], true).unwrap();
        assert!(m.is_match(&rel("kitty.conf")));
        assert!(!m.is_match(&rel("kitty/kitty.conf")));
    }

    #[test]
    fn conflict_glob_is_excluded_at_any_depth() {
        let m = Matcher::new(&["**/*.conflux-conflict-*".to_string()], false).unwrap();
        assert!(m.is_match(&rel("init.lua.conflux-conflict-1700000000")));
        assert!(m.is_match(&rel("nvim/init.lua.conflux-conflict-1700000000")));
        assert!(!m.is_match(&rel("init.lua")));
    }
}
