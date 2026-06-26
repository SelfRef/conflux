//! Error and result types for `conflux-core`.

use std::path::PathBuf;
use thiserror::Error;

/// Errors produced by the core library.
#[derive(Debug, Error)]
pub enum Error {
    /// The config file could not be read from disk.
    #[error("failed to read config at {path}: {source}")]
    ConfigRead {
        /// Path that was attempted.
        path: PathBuf,
        /// Underlying I/O error.
        source: std::io::Error,
    },

    /// The config file was not valid TOML or did not match the schema.
    #[error("failed to parse config: {0}")]
    ConfigParse(#[from] toml::de::Error),

    /// The config parsed but failed semantic validation.
    #[error("invalid config: {0}")]
    Validation(String),

    /// A required base directory could not be determined.
    #[error("could not determine {0} directory")]
    MissingDir(&'static str),

    /// A backend (local/webdav/git) operation failed.
    #[error("backend error: {0}")]
    Backend(String),

    /// The on-disk index could not be (de)serialized.
    #[error("index error: {0}")]
    Index(#[from] serde_json::Error),

    /// A generic I/O error.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Convenience result alias for the core library.
pub type Result<T> = std::result::Result<T, Error>;
