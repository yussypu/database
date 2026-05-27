//! Error types for the runtime crate.

use std::path::PathBuf;

/// The error type for runtime operations.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// An IO error occurred.
    #[error("io error at {path:?}: {message}")]
    Io {
        path: Option<PathBuf>,
        message: String,
        #[source]
        source: Option<std::io::Error>,
    },

    /// The file or directory was not found.
    #[error("not found: {0}")]
    NotFound(PathBuf),

    /// The file or directory already exists.
    #[error("already exists: {0}")]
    AlreadyExists(PathBuf),

    /// Permission denied.
    #[error("permission denied: {0}")]
    PermissionDenied(PathBuf),

    /// The disk is full (simulated or real).
    #[error("disk full")]
    DiskFull,

    /// A task was cancelled or panicked.
    #[error("task failed: {0}")]
    TaskFailed(String),

    /// Invalid argument.
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
}

impl Error {
    /// Creates an IO error from a std::io::Error.
    pub fn from_io(err: std::io::Error, path: Option<PathBuf>) -> Self {
        use std::io::ErrorKind;
        match err.kind() {
            ErrorKind::NotFound => {
                Error::NotFound(path.unwrap_or_else(|| PathBuf::from("<unknown>")))
            }
            ErrorKind::AlreadyExists => {
                Error::AlreadyExists(path.unwrap_or_else(|| PathBuf::from("<unknown>")))
            }
            ErrorKind::PermissionDenied => {
                Error::PermissionDenied(path.unwrap_or_else(|| PathBuf::from("<unknown>")))
            }
            _ => Error::Io {
                path,
                message: err.to_string(),
                source: Some(err),
            },
        }
    }
}

/// A specialized Result type for runtime operations.
pub type Result<T> = std::result::Result<T, Error>;
