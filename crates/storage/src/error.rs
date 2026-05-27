//! Error types for the storage crate.

use std::path::PathBuf;

/// The error type for storage operations.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// An I/O error occurred.
    #[error("io error: {0}")]
    Io(#[from] runtime::Error),

    /// Data corruption was detected.
    #[error("corruption: {0}")]
    Corruption(String),

    /// A key was not found.
    #[error("key not found")]
    KeyNotFound,

    /// The memtable is full.
    #[error("memtable full")]
    MemtableFull,

    /// Invalid data format.
    #[error("invalid format: {0}")]
    InvalidFormat(String),

    /// A file was not found.
    #[error("file not found: {0}")]
    FileNotFound(PathBuf),

    /// Invalid argument.
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
}

/// A specialized Result type for storage operations.
pub type Result<T> = std::result::Result<T, Error>;
