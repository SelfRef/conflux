//! Wire types for CLI <-> daemon IPC over the Unix socket.
//!
//! Messages are newline-delimited JSON: the client writes one [`Request`] line
//! and reads one [`Response`] line. The framing helpers live with the client
//! (sync, in the CLI) and server (async, in the daemon); only the types are
//! shared here.

use crate::engine::SyncSummary;
use serde::{Deserialize, Serialize};

/// A command sent from the CLI to the daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    /// Ask for the status of every active group.
    Status,
    /// Run a sync now for the given target.
    Sync(SyncTarget),
    /// Re-read and apply the configuration file.
    Reload,
}

/// Which groups a [`Request::Sync`] applies to. Profile selection happens at the
/// daemon level (each daemon serves one profile), so this only distinguishes
/// "everything this daemon runs" from a single named group.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SyncTarget {
    /// Every active group.
    All,
    /// A single group by its label (`remote:remote_path`).
    Group(String),
}

/// The daemon's reply.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Response {
    /// Status of each active group.
    Status(Vec<GroupStatus>),
    /// Outcome of a sync request, one entry per group run.
    Synced(Vec<GroupOutcome>),
    /// Result of a reload.
    Reloaded {
        /// Whether the reload succeeded.
        ok: bool,
        /// Human-readable detail.
        message: String,
    },
    /// The request could not be served.
    Error(String),
}

/// A snapshot of one group's runtime state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupStatus {
    /// Group label (`remote:remote_path`).
    pub label: String,
    /// Remote name.
    pub remote: String,
    /// Local root.
    pub root: String,
    /// Trigger kind (`manual`/`timer`/`watch`).
    pub trigger: String,
    /// Epoch seconds of the last run, if any.
    pub last_run: Option<u64>,
    /// Compact summary of the last successful run.
    pub last_summary: Option<SyncSummary>,
    /// Last error message, if the last run failed.
    pub last_error: Option<String>,
}

/// The outcome of running a single group.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupOutcome {
    /// Group label (`remote:remote_path`).
    pub label: String,
    /// Summary on success.
    pub summary: Option<SyncSummary>,
    /// Error message on failure.
    pub error: Option<String>,
}
