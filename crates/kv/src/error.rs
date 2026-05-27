//! Error types for the kv crate.
//!
//! # Design Decision (ADR-022)
//!
//! SSI conflicts are NOT errors. They surface as `Ok(CommitOutcome { aborted_for_ssi: true })`
//! because retry semantics are structurally different from error semantics.

use mvcc::SSIError;
use std::io;
use thiserror::Error;

/// Errors that can occur during database operations.
///
/// These represent actual problems, not SSI conflicts. SSI conflicts tell the user
/// "retry the transaction" which is fundamentally different from "something went wrong,
/// the database may be in trouble."
#[derive(Debug, Error)]
pub enum Error {
    /// I/O error from the storage layer.
    #[error("io error: {0}")]
    Io(#[from] io::Error),

    /// Data on disk is corrupted.
    #[error("corruption: {0}")]
    Corruption(String),

    /// A database for this path is already open in this process.
    #[error("database already open")]
    AlreadyOpen,

    /// The database path does not exist and create was not requested.
    #[error("database not found")]
    NotFound,

    /// Invalid argument provided to a method.
    #[error("invalid argument: {0}")]
    InvalidArgument(String),

    /// Storage engine error.
    #[error("storage error: {0}")]
    Storage(#[from] storage::Error),

    /// Feature not yet implemented.
    #[error("not implemented: {0}")]
    Unimplemented(String),
}

impl From<SSIError> for Error {
    fn from(e: SSIError) -> Self {
        // Map SSIError variants to appropriate kv::Error variants.
        // SerializationConflict and WriteWriteConflict are handled explicitly
        // in Txn::commit and should not normally reach this impl during commit.
        // If they do (e.g., from get/put/delete), they map to InvalidArgument.
        match e {
            SSIError::WriteWriteConflict => {
                Error::InvalidArgument("write-write conflict on key".to_string())
            }
            SSIError::SerializationConflict => Error::InvalidArgument(
                "serialization conflict: dangerous structure detected".to_string(),
            ),
            SSIError::AlreadyFinished => {
                Error::InvalidArgument("transaction already finished".to_string())
            }
            SSIError::Aborted => Error::InvalidArgument("transaction aborted".to_string()),
            SSIError::StorageError(storage_err) => {
                // Storage errors propagate through to preserve error kind
                Error::Storage(storage_err)
            }
        }
    }
}

/// Result type for kv operations.
pub type Result<T> = std::result::Result<T, Error>;
