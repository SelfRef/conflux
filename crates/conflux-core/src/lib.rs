//! Core library for conflux: configuration, domain model, and (later) the sync
//! engine and backends. Both the daemon and the CLI are thin shells over this crate.

#![warn(missing_docs)]

pub mod backend;
pub mod config;
pub mod engine;
pub mod error;
pub mod hash;
pub mod index;
pub mod ipc;
pub mod matcher;
pub mod model;
pub mod paths;
pub mod relpath;
pub mod timefmt;

pub use config::Config;
pub use error::{Error, Result};
pub use paths::{Paths, RunMode};
pub use relpath::RelPath;
