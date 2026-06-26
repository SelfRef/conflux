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

impl Paths {
    /// Resolve paths for the given mode. `$CONFLUX_CONFIG` overrides the config path.
    pub fn resolve(mode: RunMode) -> Result<Self> {
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

        if let Some(p) = std::env::var_os("CONFLUX_CONFIG") {
            paths.config = PathBuf::from(p);
        }

        Ok(paths)
    }
}
