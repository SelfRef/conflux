//! Filesystem path resolution for user vs system deployment modes.

use crate::error::{Error, Result};
use std::path::PathBuf;

/// Whether conflux runs per-user or system-wide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunMode {
    /// Per-user (`systemd --user`), rooted in `$XDG_*` / `$HOME`.
    User,
    /// System-wide, rooted in `/etc`, `/var/lib`, `/run`.
    System,
}

impl RunMode {
    /// Pick a mode: `--system` forces system; otherwise root (uid 0) implies system.
    pub fn detect(force_system: bool) -> Self {
        if force_system || is_root() {
            RunMode::System
        } else {
            RunMode::User
        }
    }
}

fn is_root() -> bool {
    // SAFETY: `geteuid` takes no arguments and is always safe to call.
    unsafe { libc::geteuid() == 0 }
}

/// Resolved locations for the config file, state dir, and control socket.
#[derive(Debug, Clone)]
pub struct Paths {
    /// Path to the TOML config file.
    pub config: PathBuf,
    /// Directory holding per-group index/state.
    pub state: PathBuf,
    /// Unix domain socket used for CLI <-> daemon IPC.
    pub socket: PathBuf,
}

/// The profile whose instance uses the un-suffixed base paths.
pub const DEFAULT_PROFILE: &str = "default";

impl Paths {
    /// Resolve paths for the given mode and profile. `$CONFLUX_CONFIG` overrides
    /// the config path (which is always shared across profiles).
    ///
    /// The config file is never namespaced — one file can drive every profile.
    /// The state directory and control socket *are* namespaced by profile so
    /// several instances (`conflux@desktop`, `conflux@laptop`, …) can run on one
    /// host without colliding. The `"default"` profile keeps the base paths for
    /// backward compatibility; any other profile is suffixed.
    pub fn resolve(mode: RunMode, profile: Option<&str>) -> Result<Self> {
        let mut paths = match mode {
            RunMode::System => Paths {
                config: PathBuf::from("/etc/conflux/config.toml"),
                state: PathBuf::from("/var/lib/conflux"),
                socket: PathBuf::from("/run/conflux/conflux.sock"),
            },
            RunMode::User => {
                let base = directories::BaseDirs::new().ok_or(Error::MissingDir("home"))?;
                let config = base.config_dir().join("conflux").join("config.toml");
                let state = base
                    .state_dir()
                    .ok_or(Error::MissingDir("state"))?
                    .join("conflux");
                let socket = base
                    .runtime_dir()
                    .map(|d| d.join("conflux.sock"))
                    .unwrap_or_else(|| state.join("conflux.sock"));
                Paths {
                    config,
                    state,
                    socket,
                }
            }
        };

        // Namespace state + socket for non-default profiles so instances coexist.
        // State goes in a per-profile subdirectory of the (systemd-owned) state
        // dir; the socket keeps the same directory with a profile-suffixed name.
        if let Some(profile) = profile.filter(|p| !p.is_empty() && *p != DEFAULT_PROFILE) {
            paths.state = paths.state.join(profile);
            let name = format!("conflux-{profile}.sock");
            paths.socket = paths.socket.with_file_name(name);
        }

        if let Some(p) = std::env::var_os("CONFLUX_CONFIG") {
            paths.config = PathBuf::from(p);
        }

        Ok(paths)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // System mode uses fixed paths, so it is deterministic to assert on.
    #[test]
    fn default_profile_uses_base_paths() {
        for profile in [None, Some("default")] {
            let p = Paths::resolve(RunMode::System, profile).unwrap();
            assert_eq!(p.state, PathBuf::from("/var/lib/conflux"));
            assert_eq!(p.socket, PathBuf::from("/run/conflux/conflux.sock"));
        }
    }

    #[test]
    fn named_profile_namespaces_state_and_socket_but_not_config() {
        let base = Paths::resolve(RunMode::System, None).unwrap();
        let desktop = Paths::resolve(RunMode::System, Some("desktop")).unwrap();
        assert_eq!(desktop.state, PathBuf::from("/var/lib/conflux/desktop"));
        assert_eq!(
            desktop.socket,
            PathBuf::from("/run/conflux/conflux-desktop.sock")
        );
        // Config stays shared across profiles.
        assert_eq!(desktop.config, base.config);
    }

    #[test]
    fn distinct_profiles_get_distinct_sockets() {
        let a = Paths::resolve(RunMode::System, Some("laptop")).unwrap();
        let b = Paths::resolve(RunMode::System, Some("desktop")).unwrap();
        assert_ne!(a.socket, b.socket);
        assert_ne!(a.state, b.state);
    }
}
