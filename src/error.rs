use std::io;

use thiserror::Error;

/// Errors returned while configuring, binding, or supervising a server.
#[derive(Debug, Error)]
pub enum Error {
    #[error("invalid server configuration: {0}")]
    InvalidConfig(&'static str),
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("connection task failed: {0}")]
    Task(String),
}

/// Result returned by server lifecycle operations.
pub type Result<T> = std::result::Result<T, Error>;
