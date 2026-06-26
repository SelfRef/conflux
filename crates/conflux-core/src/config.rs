//! TOML configuration schema, loading, normalization, and validation.

use crate::error::{Error, Result};
use crate::model::{Direction, PullScope, RemoteKind, Trigger};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Glob force-added to every group's excludes so conflict copies never re-sync.
pub const CONFLICT_GLOB: &str = "*.conflux-conflict-*";

/// Default watch debounce when none is configured at any level.
pub const DEFAULT_DEBOUNCE: Duration = Duration::from_secs(2);

/// Default timer interval when a `timer` sync omits `interval`.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(60 * 60);

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
    /// Active profile for this host; `None`/`"default"` means "run every sync".
    #[serde(default)]
    pub active_profile: Option<String>,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        DaemonConfig {
            log_level: default_log_level(),
            debounce: DEFAULT_DEBOUNCE,
            active_profile: None,
        }
    }
}

/// A remote connection.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Remote {
    /// Unique name referenced by sync groups.
    pub name: String,
    /// Backend kind.
    #[serde(rename = "type")]
    pub kind: RemoteKind,
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
    /// Remote-level default watch debounce.
    #[serde(default, deserialize_with = "de::opt_duration")]
    pub debounce: Option<Duration>,
}

/// A sync group: one local root mapped to a remote path.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Sync {
    /// Name of the remote this group syncs with.
    pub remote: String,
    /// Single local root directory (tilde/env expanded on load).
    pub root: PathBuf,
    /// Path under the remote that mirrors `root`.
    pub remote_path: String,
    /// Sync direction: bidirectional (`sync`, default) or `pull` only.
    #[serde(default)]
    pub direction: Direction,
    /// Glob paths (relative to `root`) eligible for push; empty means "all".
    #[serde(default)]
    pub include: Vec<String>,
    /// Whether pulls cover the whole remote tree or only `include`.
    #[serde(default)]
    pub pull_scope: PullScope,
    /// How this group is triggered.
    pub trigger: Trigger,
    /// Timer interval; defaults to one hour when `trigger = "timer"`.
    #[serde(default, deserialize_with = "de::opt_duration")]
    pub interval: Option<Duration>,
    /// Watch debounce override.
    #[serde(default, deserialize_with = "de::opt_duration")]
    pub debounce: Option<Duration>,
    /// Profiles this group belongs to (in addition to the implicit `default`).
    #[serde(default)]
    pub profiles: Vec<String>,
    /// Glob paths (relative to `root`) excluded from sync.
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

    /// Expand `~`/env vars in local roots in place.
    fn normalize(&mut self) {
        for sync in &mut self.syncs {
            sync.root = expand_path(&sync.root);
        }
    }

    /// Check cross-field invariants. Returns the first problem found.
    pub fn validate(&self) -> Result<()> {
        let mut names = HashSet::new();
        for remote in &self.remotes {
            if !names.insert(remote.name.as_str()) {
                return Err(Error::Validation(format!(
                    "duplicate remote name `{}`",
                    remote.name
                )));
            }
        }

        for sync in &self.syncs {
            if !names.contains(sync.remote.as_str()) {
                return Err(Error::Validation(format!(
                    "sync for `{}` references unknown remote `{}`",
                    sync.root.display(),
                    sync.remote
                )));
            }
            for pattern in sync.include.iter().chain(&sync.exclude) {
                globset::Glob::new(pattern)
                    .map_err(|e| Error::Validation(format!("invalid glob `{pattern}`: {e}")))?;
            }
        }

        Ok(())
    }

    /// Look up a remote by name.
    pub fn remote(&self, name: &str) -> Option<&Remote> {
        self.remotes.iter().find(|r| r.name == name)
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
    /// Effective timer interval (configured value or the default).
    pub fn effective_interval(&self) -> Duration {
        self.interval.unwrap_or(DEFAULT_INTERVAL)
    }

    /// Effective excludes: the configured list plus the forced conflict-copy glob.
    pub fn effective_excludes(&self) -> Vec<String> {
        let mut out = self.exclude.clone();
        if !out.iter().any(|p| p == CONFLICT_GLOB) {
            out.push(CONFLICT_GLOB.to_string());
        }
        out
    }

    /// Whether `include` selects everything under `root`.
    pub fn includes_all(&self) -> bool {
        self.include.is_empty()
    }
}

fn default_log_level() -> String {
    "info".to_string()
}

fn default_debounce() -> Duration {
    DEFAULT_DEBOUNCE
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
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = r#"
        [daemon]
        log_level = "debug"
        debounce = "5s"

        [[remote]]
        name = "nc"
        type = "webdav"
        url = "https://example.com/dav/"
        username = "me"
        password = "secret"

        [[sync]]
        remote = "nc"
        root = "/tmp/cfg"
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
        assert_eq!(cfg.remotes[0].kind, RemoteKind::Webdav);
        let sync = &cfg.syncs[0];
        assert_eq!(sync.trigger, Trigger::Watch);
        assert_eq!(sync.debounce, Some(Duration::from_secs(3)));
        assert_eq!(sync.pull_scope, PullScope::All);
        assert_eq!(sync.direction, Direction::Sync);
        assert!(!sync.includes_all());
    }

    #[test]
    fn parses_pull_direction() {
        let cfg = Config::from_toml_str(
            r#"
            [[remote]]
            name = "r"
            type = "local"
            url = "/tmp/mirror"

            [[sync]]
            remote = "r"
            root = "/tmp/cfg"
            remote_path = "x"
            trigger = "timer"
            direction = "pull"
        "#,
        )
        .unwrap();
        assert_eq!(cfg.syncs[0].direction, Direction::Pull);
    }

    #[test]
    fn applies_defaults() {
        let cfg = Config::from_toml_str(
            r#"
            [[remote]]
            name = "r"
            type = "local"
            url = "/tmp/mirror"

            [[sync]]
            remote = "r"
            root = "/tmp/cfg"
            remote_path = "x"
            trigger = "timer"
        "#,
        )
        .unwrap();
        assert_eq!(cfg.daemon.debounce, DEFAULT_DEBOUNCE);
        let sync = &cfg.syncs[0];
        assert!(sync.includes_all());
        assert_eq!(sync.effective_interval(), DEFAULT_INTERVAL);
        assert!(sync.effective_excludes().iter().any(|e| e == CONFLICT_GLOB));
    }

    #[test]
    fn rejects_unknown_remote() {
        let err = Config::from_toml_str(
            r#"
            [[sync]]
            remote = "ghost"
            root = "/tmp/cfg"
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
            name = "dup"
            type = "local"
            url = "/a"
            [[remote]]
            name = "dup"
            type = "local"
            url = "/b"
        "#,
        )
        .unwrap_err();
        assert!(matches!(err, Error::Validation(_)));
    }
}
