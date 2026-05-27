//! Database backend trait and adapters.
//!
//! This module defines the `Backend` trait that all database adapters implement,
//! allowing the benchmark harness to run workloads against any supported database.

pub mod crackeddb;
pub mod lmdb_adapter;
pub mod rocksdb_adapter;
pub mod sled_adapter;

use std::io;
use std::ops::Bound;
use std::path::Path;

pub use self::crackeddb::CrackeddbBackend;
pub use self::lmdb_adapter::LmdbBackend;
pub use self::rocksdb_adapter::RocksdbBackend;
pub use self::sled_adapter::SledBackend;

/// Error type for backend operations.
#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    Other(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "I/O error: {}", e),
            Error::Other(s) => write!(f, "{}", s),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            Error::Other(_) => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// Outcome of a transaction commit.
#[derive(Debug, Clone)]
pub struct CommitOutcome {
    /// Whether the commit succeeded.
    pub success: bool,
    /// Whether the transaction was aborted due to a conflict (SSI abort or equivalent).
    pub aborted_for_conflict: bool,
}

/// Trait for database backends.
///
/// Each backend must be thread-safe (Send + Sync) to allow concurrent benchmarks.
pub trait Backend: Send + Sync + Sized {
    /// The transaction type for this backend.
    type Txn<'a>: BackendTxn<'a>
    where
        Self: 'a;

    /// Opens a database at the given path.
    fn open(path: &Path) -> Result<Self>;

    /// Begins a new transaction.
    fn begin(&self) -> Result<Self::Txn<'_>>;

    /// Closes the database, releasing resources.
    fn close(self) -> Result<()>;

    /// Returns the total size of the database on disk in bytes.
    fn disk_size_bytes(&self) -> Result<u64>;
}

/// Trait for database transactions.
pub trait BackendTxn<'a> {
    /// Gets a value by key.
    fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>>;

    /// Puts a key-value pair.
    fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()>;

    /// Deletes a key.
    fn delete(&mut self, key: &[u8]) -> Result<()>;

    /// Scans a range of keys.
    fn scan(&mut self, start: Bound<&[u8]>, end: Bound<&[u8]>) -> Result<Vec<(Vec<u8>, Vec<u8>)>>;

    /// Commits the transaction.
    fn commit(self) -> Result<CommitOutcome>;

    /// Rolls back the transaction.
    fn rollback(self) -> Result<()>;
}

/// Calculate directory size recursively.
pub fn dir_size(path: &Path) -> Result<u64> {
    let mut total = 0u64;
    if path.is_dir() {
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                total += dir_size(&path)?;
            } else {
                total += entry.metadata()?.len();
            }
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Generic smoke test for any backend.
    fn run_smoke<B: Backend>() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("db");

        // Open, write, commit
        {
            let db = B::open(&path).unwrap();
            let mut txn = db.begin().unwrap();
            txn.put(b"key1", b"value1").unwrap();
            txn.put(b"key2", b"value2").unwrap();
            let outcome = txn.commit().unwrap();
            assert!(outcome.success);
            db.close().unwrap();
        }

        // Reopen, read back
        {
            let db = B::open(&path).unwrap();
            let mut txn = db.begin().unwrap();
            let v1 = txn.get(b"key1").unwrap();
            let v2 = txn.get(b"key2").unwrap();
            assert_eq!(v1, Some(b"value1".to_vec()));
            assert_eq!(v2, Some(b"value2".to_vec()));
            txn.rollback().unwrap();
            db.close().unwrap();
        }
    }

    /// Smoke test for crackeddb backend.
    #[test]
    fn smoke_crackeddb() {
        run_smoke::<CrackeddbBackend>();
    }

    /// Smoke test for RocksDB backend.
    #[test]
    fn smoke_rocksdb() {
        run_smoke::<RocksdbBackend>();
    }

    /// Smoke test for LMDB backend.
    #[test]
    fn smoke_lmdb() {
        run_smoke::<LmdbBackend>();
    }

    /// Smoke test for sled backend.
    #[test]
    fn smoke_sled() {
        run_smoke::<SledBackend>();
    }
}
