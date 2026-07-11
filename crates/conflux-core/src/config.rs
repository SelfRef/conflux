//! TOML configuration schema, loading, normalization, and validation.

use crate::error::{Error, Result};
use crate::model::{EmptyDirMode, RemoteKind, Scope, Trigger};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Glob force-added to every group's excludes so conflict copies never re-sync.
/// The leading `**/` matches conflict copies at any depth, not just the root.
pub const CONFLICT_GLOB: &str = "**/*.conflux-conflict-*";

/// Well-known paths excluded by default (the `[daemon] exclude` default): VCS
/// metadata and common OS/editor cruft that should almost never be synced.
pub const DEFAULT_EXCLUDES: &[&str] = &[
    "**/.git/**",
    "**/.svn/**",
    "**/.hg/**",
    "**/.DS_Store",
    "**/Thumbs.db",
    "**/*.swp",
];

/// Default watch debounce when none is configured at any level.
pub const DEFAULT_DEBOUNCE: Duration = Duration::from_secs(5);

/// Default timer interval when a `timer` sync omits `interval`.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// Default `max_file_size` when none is configured at any level: 100 MB.
pub const DEFAULT_MAX_FILE_SIZE: u64 = 100 * 1000 * 1000;

/// Top-level parsed configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Daemon-wide settings.
    #[serde(default)]
    pub daemon: DaemonConfig,
    /// Remote connections (`[[remote]]`).
    #[serde(default, rename = "remote")]
    pub remotes: Vec<Remote>,
    /// Sync groups (`[[sync]]`).
    #[serde(default, rename = "sync")]
    pub syncs: Vec<Sync>,
}

/// Daemon-wide settings.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonConfig {
    /// Tracing log level (e.g. `info`, `debug`).
    #[serde(default = "default_log_level")]
    pub log_level: String,
    /// Default watch debounce, overridable per remote/sync.
    #[serde(
        default = "default_debounce",
        deserialize_with = "de::duration",
        serialize_with = "se::duration"
    )]
    pub debounce: Duration,
    /// Default timer interval, overridable per sync.
    #[serde(
        default = "default_interval",
        deserialize_with = "de::duration",
        serialize_with = "se::duration"
    )]
    pub interval: Duration,
    /// Default interval for periodic remote pulls, overridable per remote/sync.
    /// `None` (unset) disables background pulling; `0` also disables it (handy to
    /// opt a single group out when a broader level enables it).
    #[serde(default, deserialize_with = "de::opt_duration")]
    pub pull_interval: Option<Duration>,
    /// Default empty-directory handling, overridable per remote/sync.
    #[serde(default)]
    pub empty_dirs: EmptyDirMode,
    /// Default maximum size, in bytes, of a file synced in either direction;
    /// larger files are skipped. Accepts a byte count or a size string like
    /// `"100MB"`/`"512KiB"`. `0` means unlimited. Overridable per remote/sync.
    #[serde(
        default = "default_max_file_size",
        deserialize_with = "de::byte_size",
        serialize_with = "se::byte_size"
    )]
    pub max_file_size: u64,
    /// Globs excluded from every sync (in addition to each sync's own `exclude`
    /// and the always-forced conflict-copy glob). Defaults to well-known cruft
    /// like `.git`; set to `[]` to sync everything.
    #[serde(default = "default_exclude")]
    pub exclude: Vec<String>,
    /// Active profile for this host; defaults to `"default"` when unset. Only
    /// syncs whose `profiles` include this value run, so the default runs the
    /// syncs that omitted the `profiles` setting. A programmatic `None` runs
    /// every sync (not reachable from config).
    #[serde(default = "default_profile")]
    pub profile: Option<String>,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        DaemonConfig {
            log_level: default_log_level(),
            debounce: DEFAULT_DEBOUNCE,
            interval: DEFAULT_INTERVAL,
            pull_interval: None,
            empty_dirs: EmptyDirMode::default(),
            max_file_size: default_max_file_size(),
            exclude: default_exclude(),
            profile: default_profile(),
        }
    }
}

/// A remote connection.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Remote {
    /// Unique id referenced by sync groups (each sync's `remote` field).
    pub id: String,
    /// Optional human-friendly label for UIs; not used by the daemon.
    #[serde(default)]
    pub label: Option<String>,
    /// Backend kind.
    pub backend: RemoteKind,
    /// Backend URL (WebDAV base / git remote / local path).
    pub url: String,
    /// Optional username for auth.
    #[serde(default)]
    pub username: Option<String>,
    /// Optional plaintext password.
    #[serde(default)]
    pub password: Option<String>,
    /// Optional command whose stdout yields the password.
    #[serde(default)]
    pub password_command: Option<String>,
    /// Git branch; defaults to the remote's default branch when omitted.
    #[serde(default)]
    pub branch: Option<String>,
    /// Git only (optional): command whose stdout is used as the commit message
    /// (run via `sh -c` in the clone directory). Defaults to `"conflux sync"`.
    #[serde(default)]
    pub commit_msg_command: Option<String>,
    /// Remote-level default for periodic remote pulls (overrides the daemon
    /// default; overridden per sync). See [`DaemonConfig::pull_interval`].
    #[serde(default, deserialize_with = "de::opt_duration")]
    pub pull_interval: Option<Duration>,
    /// Remote-level empty-directory handling (overrides the daemon default;
    /// overridden per sync). Ignored for git remotes, which cannot store them.
    #[serde(default)]
    pub empty_dirs: Option<EmptyDirMode>,
    /// Remote-level maximum synced file size in bytes (overrides the daemon
    /// default; overridden per sync). `0` means unlimited. See
    /// [`DaemonConfig::max_file_size`].
    #[serde(default, deserialize_with = "de::opt_byte_size")]
    pub max_file_size: Option<u64>,
}

/// A sync group: one local root mapped to a remote path.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Sync {
    /// Optional short id for this group, used to target it from the CLI
    /// (`conflux sync <id>`). Must be unique across syncs when set. The
    /// `remote:remote_path` label works as a target too.
    #[serde(default)]
    pub id: Option<String>,
    /// Optional human-friendly label for UIs; not used by the daemon.
    #[serde(default)]
    pub label: Option<String>,
    /// Id of the remote this group syncs with (matches a `[[remote]]` `id`).
    pub remote: String,
    /// Single local root directory (tilde/env expanded on load).
    pub local: PathBuf,
    /// Path under the remote that mirrors `local`. Optional; when omitted (or
    /// empty) the group maps to the remote's root. Normalized to drop
    /// surrounding slashes on load.
    #[serde(default)]
    pub remote_path: String,
    /// Glob paths (relative to `local`) selecting what this group syncs. An empty
    /// list (the default) matches nothing; use `["**"]` to match everything. How
    /// paths outside `include` are treated is governed by [`Sync::scope`].
    #[serde(default)]
    pub include: Vec<String>,
    /// What this group covers relative to `include`: `include` (default),
    /// `remote`, `local`, or `mirror`. See [`Scope`]. Sync-level only.
    #[serde(default)]
    pub scope: Scope,
    /// How this group is triggered.
    pub trigger: Trigger,
    /// Timer interval; defaults to one hour when `trigger = "timer"`.
    #[serde(default, deserialize_with = "de::opt_duration")]
    pub interval: Option<Duration>,
    /// Watch debounce override.
    #[serde(default, deserialize_with = "de::opt_duration")]
    pub debounce: Option<Duration>,
    /// Periodic remote-pull interval for this group (overrides the remote/daemon
    /// default). Independent of `trigger`: it adds a pull-only run every interval
    /// so remote-side changes are picked up even for `watch`/`manual` groups.
    /// See [`DaemonConfig::pull_interval`].
    #[serde(default, deserialize_with = "de::opt_duration")]
    pub pull_interval: Option<Duration>,
    /// Empty-directory handling for this group (overrides the remote/daemon
    /// default): `ignore` (default), `prune`, or `keep`. Ignored for git.
    #[serde(default)]
    pub empty_dirs: Option<EmptyDirMode>,
    /// Maximum synced file size in bytes for this group (overrides the
    /// remote/daemon default). `0` means unlimited. See
    /// [`DaemonConfig::max_file_size`].
    #[serde(default, deserialize_with = "de::opt_byte_size")]
    pub max_file_size: Option<u64>,
    /// Profiles this group belongs to. When the setting is omitted it defaults
    /// to the implicit `["default"]`; an explicit list (even empty) is used
    /// verbatim, without any implicit `default` membership.
    #[serde(default = "default_profiles")]
    pub profiles: Vec<String>,
    /// Glob paths (relative to `local`) excluded from sync, *added to* the
    /// `[daemon] exclude` defaults (they are not replaced).
    #[serde(default)]
    pub exclude: Vec<String>,
}

impl Config {
    /// Read, parse, normalize, and validate a config file.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|source| Error::ConfigRead {
            path: path.to_path_buf(),
            source,
        })?;
        let mut config: Config = toml::from_str(&text)?;
        config.normalize();
        config.validate()?;
        Ok(config)
    }

    /// Parse a config from a TOML string (used by tests); also validates.
    pub fn from_toml_str(text: &str) -> Result<Self> {
        let mut config: Config = toml::from_str(text)?;
        config.normalize();
        config.validate()?;
        Ok(config)
    }

    /// Expand `~`/env vars in local roots and trim slashes off remote paths.
    fn normalize(&mut self) {
        for sync in &mut self.syncs {
            sync.local = expand_path(&sync.local);
            // An empty / all-slashes `remote_path` means "the remote root".
            let trimmed = sync.remote_path.trim_matches('/');
            if trimmed.len() != sync.remote_path.len() {
                sync.remote_path = trimmed.to_string();
            }
        }
    }

    /// Check cross-field invariants. Returns the first problem found.
    pub fn validate(&self) -> Result<()> {
        let mut ids = HashSet::new();
        for remote in &self.remotes {
            if !ids.insert(remote.id.as_str()) {
                return Err(Error::Validation(format!(
                    "duplicate remote id `{}`",
                    remote.id
                )));
            }
            if remote.commit_msg_command.is_some() && remote.backend != RemoteKind::Git {
                return Err(Error::Validation(format!(
                    "remote `{}` sets `commit_msg_command`, which only applies to a `git` \
                     backend, but its backend is `{}`",
                    remote.id,
                    remote.backend.as_str(),
                )));
            }
        }

        let mut labels = HashSet::new();
        let mut sync_ids = HashSet::new();
        for sync in &self.syncs {
            if !ids.contains(sync.remote.as_str()) {
                return Err(Error::Validation(format!(
                    "sync for `{}` references unknown remote `{}`",
                    sync.local.display(),
                    sync.remote
                )));
            }
            // Two groups sharing a (remote, remote_path) collide on their state
            // index; catch it here (easy to hit now that `remote_path` is optional).
            let label = crate::engine::group_label(sync);
            if !labels.insert(label.clone()) {
                return Err(Error::Validation(format!(
                    "duplicate sync group `{label}` (same remote and remote_path)"
                )));
            }
            // `id` is used to target a single group from the CLI, so it must be
            // unambiguous.
            if let Some(id) = &sync.id {
                if !sync_ids.insert(id.as_str()) {
                    return Err(Error::Validation(format!("duplicate sync id `{id}`")));
                }
            }
            // `watch-both` watches the remote's filesystem path, which only a
            // `local` backend has — reject it up front rather than silently
            // degrading to a local-only watch.
            if sync.trigger == Trigger::WatchBoth {
                if let Some(remote) = self.remote(&sync.remote) {
                    if remote.backend != RemoteKind::Local {
                        return Err(Error::Validation(format!(
                            "sync `{label}` uses `trigger = \"watch-both\"`, which requires a \
                             `local` backend, but remote `{}` is `{}`; use `trigger = \"watch\"` \
                             (optionally with `pull_interval`) instead",
                            sync.remote,
                            remote.backend.as_str(),
                        )));
                    }
                }
            }
            for pattern in sync.include.iter().chain(&sync.exclude) {
                globset::Glob::new(pattern)
                    .map_err(|e| Error::Validation(format!("invalid glob `{pattern}`: {e}")))?;
            }
        }

        Ok(())
    }

    /// Non-fatal configuration lints: conditions that parse and validate fine
    /// but are almost certainly mistakes. Returned as human-readable messages
    /// for the caller to surface (the daemon logs them at startup; `conflux
    /// config validate` prints them).
    pub fn warnings(&self) -> Vec<String> {
        let mut out = Vec::new();
        for sync in &self.syncs {
            // `scope = "include"` with no `include` globs matches nothing, so the
            // group is inert — likely an oversight rather than intent.
            if sync.scope == Scope::Include && sync.include.is_empty() {
                out.push(format!(
                    "sync group `{}` has scope = \"include\" but an empty `include`, so it \
                     syncs nothing; add `include` globs or pick a different `scope`",
                    crate::engine::group_label(sync)
                ));
            }
        }
        out
    }

    /// Look up a remote by its id.
    pub fn remote(&self, id: &str) -> Option<&Remote> {
        self.remotes.iter().find(|r| r.id == id)
    }
}

impl Remote {
    /// Resolve the password: run `password_command` if set, else use `password`.
    pub fn resolve_password(&self) -> Result<Option<String>> {
        if let Some(cmd) = &self.password_command {
            let output = std::process::Command::new("sh")
                .arg("-c")
                .arg(cmd)
                .output()
                .map_err(|e| Error::Backend(format!("password_command failed to run: {e}")))?;
            if !output.status.success() {
                return Err(Error::Backend(format!(
                    "password_command exited with {}: {}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr).trim()
                )));
            }
            let pw = String::from_utf8_lossy(&output.stdout)
                .trim_end_matches(['\n', '\r'])
                .to_string();
            Ok(Some(pw))
        } else {
            Ok(self.password.clone())
        }
    }
}

impl Sync {
    /// Effective timer interval: the per-sync value, else the daemon default.
    pub fn effective_interval(&self, daemon_default: Duration) -> Duration {
        self.interval.unwrap_or(daemon_default)
    }

    /// Effective excludes: the `[daemon]`-level `defaults`, this group's own
    /// `exclude`, and the always-forced conflict-copy glob, unioned (deduped).
    pub fn effective_excludes(&self, defaults: &[String]) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for pat in defaults.iter().chain(&self.exclude) {
            if !out.iter().any(|p| p == pat) {
                out.push(pat.clone());
            }
        }
        if !out.iter().any(|p| p == CONFLICT_GLOB) {
            out.push(CONFLICT_GLOB.to_string());
        }
        out
    }
}

fn default_log_level() -> String {
    "info".to_string()
}

fn default_exclude() -> Vec<String> {
    DEFAULT_EXCLUDES.iter().map(|s| s.to_string()).collect()
}

fn default_debounce() -> Duration {
    DEFAULT_DEBOUNCE
}

fn default_interval() -> Duration {
    DEFAULT_INTERVAL
}

fn default_max_file_size() -> u64 {
    DEFAULT_MAX_FILE_SIZE
}

/// Parse a byte size: a bare number (bytes) or a number with a unit suffix.
/// Decimal units (`KB`, `MB`, `GB`, `TB`) are powers of 1000; binary units
/// (`KiB`, `MiB`, `GiB`, `TiB`) are powers of 1024. Case-insensitive.
fn parse_byte_size(s: &str) -> std::result::Result<u64, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty size".to_string());
    }
    // Split the leading numeric part from an optional unit suffix.
    let split = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let value: f64 = num
        .parse()
        .map_err(|_| format!("invalid size number in `{s}`"))?;
    let mult: f64 = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1.0,
        "k" | "kb" => 1e3,
        "m" | "mb" => 1e6,
        "g" | "gb" => 1e9,
        "t" | "tb" => 1e12,
        "kib" => 1024.0,
        "mib" => 1024f64.powi(2),
        "gib" => 1024f64.powi(3),
        "tib" => 1024f64.powi(4),
        other => return Err(format!("unknown size unit `{other}` in `{s}`")),
    };
    Ok((value * mult) as u64)
}

/// Format a byte count back to a compact decimal size string for `config show`,
/// preferring the largest unit that divides it evenly (e.g. `100000000` →
/// `"100MB"`); falls back to a bare byte count (which also covers `0`).
fn format_byte_size(n: u64) -> String {
    const UNITS: &[(&str, u64)] = &[("TB", 1_000_000_000_000), ("GB", 1_000_000_000), ("MB", 1_000_000), ("KB", 1_000)];
    for (name, factor) in UNITS {
        if n >= *factor && n % factor == 0 {
            return format!("{}{name}", n / factor);
        }
    }
    n.to_string()
}

/// Active profile when the `[daemon] profile` setting is omitted.
fn default_profile() -> Option<String> {
    Some("default".to_string())
}

/// Implicit profile membership when the `profiles` setting is omitted.
fn default_profiles() -> Vec<String> {
    vec!["default".to_string()]
}

/// Expand a leading `~` and `$VAR` references; falls back to the input on error.
fn expand_path(p: &Path) -> PathBuf {
    match p.to_str() {
        Some(s) => match shellexpand::full(s) {
            Ok(expanded) => PathBuf::from(expanded.into_owned()),
            Err(_) => p.to_path_buf(),
        },
        None => p.to_path_buf(),
    }
}

mod de {
    use serde::{Deserialize, Deserializer};
    use std::time::Duration;

    pub fn duration<'de, D>(d: D) -> Result<Duration, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(d)?;
        humantime::parse_duration(&s).map_err(serde::de::Error::custom)
    }

    pub fn opt_duration<'de, D>(d: D) -> Result<Option<Duration>, D::Error>
    where
        D: Deserializer<'de>,
    {
        match Option::<String>::deserialize(d)? {
            Some(s) => humantime::parse_duration(&s)
                .map(Some)
                .map_err(serde::de::Error::custom),
            None => Ok(None),
        }
    }

    /// A byte size given either as an integer (bytes) or a size string.
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum ByteSize {
        Int(u64),
        Str(String),
    }

    impl ByteSize {
        fn into_bytes<E: serde::de::Error>(self) -> Result<u64, E> {
            match self {
                ByteSize::Int(n) => Ok(n),
                ByteSize::Str(s) => super::parse_byte_size(&s).map_err(E::custom),
            }
        }
    }

    pub fn byte_size<'de, D>(d: D) -> Result<u64, D::Error>
    where
        D: Deserializer<'de>,
    {
        ByteSize::deserialize(d)?.into_bytes()
    }

    pub fn opt_byte_size<'de, D>(d: D) -> Result<Option<u64>, D::Error>
    where
        D: Deserializer<'de>,
    {
        match Option::<ByteSize>::deserialize(d)? {
            Some(bs) => bs.into_bytes().map(Some),
            None => Ok(None),
        }
    }
}

mod se {
    use serde::Serializer;
    use std::time::Duration;

    pub fn duration<S>(d: &Duration, s: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        s.serialize_str(&humantime::format_duration(*d).to_string())
    }

    pub fn byte_size<S>(n: &u64, s: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        s.serialize_str(&super::format_byte_size(*n))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = r#"
        [daemon]
        log_level = "debug"
        debounce = "5s"

        [[remote]]
        id = "nc"
        backend = "webdav"
        url = "https://example.com/dav/"
        username = "me"
        password = "secret"

        [[sync]]
        remote = "nc"
        local = "/tmp/cfg"
        remote_path = "config"
        include = ["nvim", "fish/*"]
        trigger = "watch"
        debounce = "3s"
    "#;

    #[test]
    fn parses_and_validates_good_config() {
        let cfg = Config::from_toml_str(GOOD).expect("should parse");
        assert_eq!(cfg.daemon.debounce, Duration::from_secs(5));
        assert_eq!(cfg.remotes.len(), 1);
        assert_eq!(cfg.remotes[0].backend, RemoteKind::Webdav);
        let sync = &cfg.syncs[0];
        assert_eq!(sync.trigger, Trigger::Watch);
        assert_eq!(sync.debounce, Some(Duration::from_secs(3)));
        assert_eq!(sync.scope, Scope::Include); // unset => the safe default
        assert_eq!(sync.include, vec!["nvim".to_string(), "fish/*".to_string()]);
    }

    #[test]
    fn shipped_example_config_parses() {
        // Guards against config.example.toml drifting from the schema. The
        // example keeps only `[daemon]` uncommented (with every option at its
        // default), so the remotes/syncs lists are empty.
        let text = include_str!("../../../config.example.toml");
        let cfg = Config::from_toml_str(text).expect("example config should parse");
        assert!(cfg.remotes.is_empty());
        assert!(cfg.syncs.is_empty());
        // The `[daemon]` block spells out the built-in defaults verbatim.
        let d = &cfg.daemon;
        assert_eq!(d.log_level, default_log_level());
        assert_eq!(d.debounce, DEFAULT_DEBOUNCE);
        assert_eq!(d.interval, DEFAULT_INTERVAL);
        assert_eq!(d.empty_dirs, EmptyDirMode::Ignore);
        assert_eq!(d.max_file_size, DEFAULT_MAX_FILE_SIZE);
        assert_eq!(d.profile.as_deref(), Some("default"));
        // pull_interval is shown as "0s" (the representable "disabled" default).
        assert!(d.pull_interval.is_none_or(|dur| dur.is_zero()));
    }

    #[test]
    fn parses_max_file_size_as_bytes_units_and_zero() {
        // Size strings and bare byte counts both parse; `0` means unlimited.
        assert_eq!(parse_byte_size("100MB"), Ok(100_000_000));
        assert_eq!(parse_byte_size("512KiB"), Ok(512 * 1024));
        assert_eq!(parse_byte_size("2gb"), Ok(2_000_000_000));
        assert_eq!(parse_byte_size("1048576"), Ok(1_048_576));
        assert!(parse_byte_size("10QB").is_err());
        // Round-trips through the serializer for `config show`.
        assert_eq!(format_byte_size(100_000_000), "100MB");
        assert_eq!(format_byte_size(0), "0");

        let cfg = Config::from_toml_str(
            r#"
            [daemon]
            max_file_size = "250MB"

            [[remote]]
            id = "r"
            backend = "local"
            url = "/tmp/mirror"
            max_file_size = 0

            [[sync]]
            remote = "r"
            local = "/tmp/cfg"
            remote_path = "x"
            trigger = "timer"
            max_file_size = "10MiB"
        "#,
        )
        .unwrap();
        assert_eq!(cfg.daemon.max_file_size, 250_000_000);
        assert_eq!(cfg.remotes[0].max_file_size, Some(0));
        assert_eq!(cfg.syncs[0].max_file_size, Some(10 * 1024 * 1024));
    }

    #[test]
    fn parses_scope() {
        let cfg = Config::from_toml_str(
            r#"
            [[remote]]
            id = "r"
            backend = "local"
            url = "/tmp/mirror"

            [[sync]]
            remote = "r"
            local = "/tmp/cfg"
            remote_path = "x"
            trigger = "timer"
            scope = "remote"
        "#,
        )
        .unwrap();
        assert_eq!(cfg.syncs[0].scope, Scope::Remote);
    }

    #[test]
    fn parses_optional_sync_id_and_rejects_duplicates() {
        let base = r#"
            [[remote]]
            id = "r"
            backend = "local"
            url = "/tmp/mirror"

            [[sync]]
            id = "docs"
            remote = "r"
            local = "/tmp/a"
            remote_path = "documents"
            trigger = "manual"
        "#;
        let cfg = Config::from_toml_str(base).unwrap();
        assert_eq!(cfg.syncs[0].id.as_deref(), Some("docs"));

        // A second group reusing the same id is rejected.
        let err = Config::from_toml_str(&format!(
            "{base}\n[[sync]]\nid = \"docs\"\nremote = \"r\"\nlocal = \"/tmp/b\"\n\
             remote_path = \"other\"\ntrigger = \"manual\"\n"
        ))
        .unwrap_err();
        assert!(matches!(err, Error::Validation(_)));
    }

    #[test]
    fn warns_when_include_scope_has_empty_include() {
        let base = r#"
            [[remote]]
            id = "r"
            backend = "local"
            url = "/tmp/mirror"

            [[sync]]
            remote = "r"
            local = "/tmp/cfg"
            remote_path = "x"
            trigger = "manual"
        "#;

        // Default scope = "include" with no `include` globs: inert, so warn.
        let cfg = Config::from_toml_str(base).unwrap();
        let warnings = cfg.warnings();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("syncs nothing"));

        // Giving it an `include` clears the warning.
        let cfg = Config::from_toml_str(&format!("{base}\ninclude = [\"nvim\"]")).unwrap();
        assert!(cfg.warnings().is_empty());

        // So does a scope that reaches beyond `include`.
        let cfg = Config::from_toml_str(&format!("{base}\nscope = \"mirror\"")).unwrap();
        assert!(cfg.warnings().is_empty());
    }

    #[test]
    fn applies_defaults() {
        let cfg = Config::from_toml_str(
            r#"
            [[remote]]
            id = "r"
            backend = "local"
            url = "/tmp/mirror"

            [[sync]]
            remote = "r"
            local = "/tmp/cfg"
            remote_path = "x"
            trigger = "timer"
        "#,
        )
        .unwrap();
        assert_eq!(cfg.daemon.debounce, DEFAULT_DEBOUNCE);
        assert_eq!(cfg.daemon.interval, DEFAULT_INTERVAL);
        // `profile` defaults to "default" when the setting is omitted.
        assert_eq!(cfg.daemon.profile.as_deref(), Some("default"));
        // `[daemon] exclude` defaults to the well-known list.
        assert_eq!(cfg.daemon.exclude, default_exclude());
        assert!(cfg.daemon.exclude.iter().any(|e| e == "**/.git/**"));
        let sync = &cfg.syncs[0];
        // Unset `scope`/`include` => the safe default: `include` scope, empty
        // include list (nothing synced until the user opts paths in).
        assert_eq!(sync.scope, Scope::Include);
        assert!(sync.include.is_empty());
        // No per-sync interval => falls back to the daemon default.
        assert_eq!(sync.interval, None);
        assert_eq!(sync.effective_interval(cfg.daemon.interval), DEFAULT_INTERVAL);
        // effective_excludes merges the daemon defaults, the sync's own, and the
        // forced conflict glob.
        let eff = sync.effective_excludes(&cfg.daemon.exclude);
        assert!(eff.iter().any(|e| e == CONFLICT_GLOB));
        assert!(eff.iter().any(|e| e == "**/.git/**"));
    }

    #[test]
    fn exclude_merges_daemon_defaults_with_sync_and_can_be_overridden() {
        // Sync-level excludes ADD to the daemon defaults; both plus the forced
        // conflict glob show up.
        let defaults = vec!["**/.git/**".to_string()];
        let sync = Sync {
            id: None,
            label: None,
            remote: "r".into(),
            local: "/tmp/x".into(),
            remote_path: String::new(),
            include: vec![],
            scope: Scope::default(),
            trigger: Trigger::Manual,
            interval: None,
            debounce: None,
            pull_interval: None,
            empty_dirs: None,
            max_file_size: None,
            profiles: default_profiles(),
            exclude: vec!["*.log".to_string()],
        };
        let eff = sync.effective_excludes(&defaults);
        assert!(eff.iter().any(|e| e == "**/.git/**"));
        assert!(eff.iter().any(|e| e == "*.log"));
        assert!(eff.iter().any(|e| e == CONFLICT_GLOB));

        // Empty daemon defaults (user opted out) => only the sync's own + forced.
        let eff = sync.effective_excludes(&[]);
        assert!(!eff.iter().any(|e| e == "**/.git/**"));
        assert!(eff.iter().any(|e| e == CONFLICT_GLOB));
    }

    #[test]
    fn remote_path_is_optional_and_maps_to_remote_root() {
        let cfg = Config::from_toml_str(
            r#"
            [[remote]]
            id = "mirror"
            backend = "local"
            url = "/tmp/mirror"

            [[sync]]
            remote = "mirror"
            local = "/tmp/a"
            trigger = "manual"

            [[sync]]
            remote = "mirror"
            local = "/tmp/b"
            remote_path = "/sub/dir/"
            trigger = "manual"
        "#,
        )
        .unwrap();
        // Omitted => empty (remote root); label is just the remote name.
        assert_eq!(cfg.syncs[0].remote_path, "");
        assert_eq!(crate::engine::group_label(&cfg.syncs[0]), "mirror");
        // Surrounding slashes are trimmed on load.
        assert_eq!(cfg.syncs[1].remote_path, "sub/dir");
        assert_eq!(crate::engine::group_label(&cfg.syncs[1]), "mirror:sub/dir");
    }

    #[test]
    fn rejects_commit_msg_command_on_non_git_backend() {
        let err = Config::from_toml_str(
            r#"
            [[remote]]
            id = "m"
            backend = "local"
            url = "/tmp/mirror"
            commit_msg_command = "printf hi"
        "#,
        )
        .unwrap_err();
        assert!(matches!(err, Error::Validation(_)));
    }

    #[test]
    fn rejects_watch_both_on_non_local_backend() {
        let err = Config::from_toml_str(
            r#"
            [[remote]]
            id = "nc"
            backend = "webdav"
            url = "https://example.com/dav/"

            [[sync]]
            remote = "nc"
            local = "/tmp/x"
            trigger = "watch-both"
        "#,
        )
        .unwrap_err();
        assert!(matches!(err, Error::Validation(_)));

        // A local backend with watch-both is fine.
        Config::from_toml_str(
            r#"
            [[remote]]
            id = "m"
            backend = "local"
            url = "/tmp/mirror"

            [[sync]]
            remote = "m"
            local = "/tmp/x"
            trigger = "watch-both"
        "#,
        )
        .expect("watch-both on a local backend should be valid");
    }

    #[test]
    fn rejects_duplicate_group_labels() {
        // Both default remote_path to the root => same label => index collision.
        let err = Config::from_toml_str(
            r#"
            [[remote]]
            id = "mirror"
            backend = "local"
            url = "/tmp/mirror"

            [[sync]]
            remote = "mirror"
            local = "/tmp/a"
            trigger = "manual"

            [[sync]]
            remote = "mirror"
            local = "/tmp/b"
            trigger = "manual"
        "#,
        )
        .unwrap_err();
        assert!(matches!(err, Error::Validation(_)));
    }

    #[test]
    fn daemon_interval_overrides_default_and_syncs_inherit_it() {
        let cfg = Config::from_toml_str(
            r#"
            [daemon]
            interval = "15m"

            [[remote]]
            id = "r"
            backend = "local"
            url = "/tmp/mirror"

            [[sync]]
            remote = "r"
            local = "/tmp/a"
            remote_path = "a"
            trigger = "timer"

            [[sync]]
            remote = "r"
            local = "/tmp/b"
            remote_path = "b"
            trigger = "timer"
            interval = "5m"
        "#,
        )
        .unwrap();
        assert_eq!(cfg.daemon.interval, Duration::from_secs(15 * 60));
        // Sync without its own interval inherits the daemon-level value.
        assert_eq!(
            cfg.syncs[0].effective_interval(cfg.daemon.interval),
            Duration::from_secs(15 * 60)
        );
        // Per-sync interval still wins.
        assert_eq!(
            cfg.syncs[1].effective_interval(cfg.daemon.interval),
            Duration::from_secs(5 * 60)
        );
    }

    #[test]
    fn profiles_default_to_implicit_default_only_when_omitted() {
        let cfg = Config::from_toml_str(
            r#"
            [[remote]]
            id = "r"
            backend = "local"
            url = "/tmp/mirror"

            [[sync]]
            remote = "r"
            local = "/tmp/a"
            remote_path = "a"
            trigger = "manual"

            [[sync]]
            remote = "r"
            local = "/tmp/b"
            remote_path = "b"
            trigger = "manual"
            profiles = ["desktop"]

            [[sync]]
            remote = "r"
            local = "/tmp/c"
            remote_path = "c"
            trigger = "manual"
            profiles = []
        "#,
        )
        .unwrap();
        // Omitted => implicit ["default"].
        assert_eq!(cfg.syncs[0].profiles, vec!["default".to_string()]);
        // Explicit list => used verbatim, no implicit "default".
        assert_eq!(cfg.syncs[1].profiles, vec!["desktop".to_string()]);
        // Explicit empty => member of no profile.
        assert!(cfg.syncs[2].profiles.is_empty());
    }

    #[test]
    fn rejects_unknown_remote() {
        let err = Config::from_toml_str(
            r#"
            [[sync]]
            remote = "ghost"
            local = "/tmp/cfg"
            remote_path = "x"
            trigger = "manual"
        "#,
        )
        .unwrap_err();
        assert!(matches!(err, Error::Validation(_)));
    }

    #[test]
    fn rejects_duplicate_remote_names() {
        let err = Config::from_toml_str(
            r#"
            [[remote]]
            id = "dup"
            backend = "local"
            url = "/a"
            [[remote]]
            id = "dup"
            backend = "local"
            url = "/b"
        "#,
        )
        .unwrap_err();
        assert!(matches!(err, Error::Validation(_)));
    }
}
