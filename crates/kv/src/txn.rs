//! Transaction types.
//!
//! # Design Decisions (ADR-022)
//!
//! - `Txn` is `Send` but not `Sync`: it can move between threads but cannot be shared.
//! - `commit()` returns `Result<CommitOutcome, Error>`. SSI conflicts surface as
//!   `Ok(CommitOutcome { aborted_for_ssi: true })`, NOT as errors.
//! - Drop on uncommitted `Txn` rolls back with a tracing warning (no panic).

use crate::db::DbInner;
use crate::error::{Error, Result};
use bytes::Bytes;
use mvcc::{SSIError, SSITransaction};
use runtime::Env;
use std::marker::PhantomData;
use std::ops::{Bound, RangeBounds};
use std::sync::Arc;

/// The outcome of committing a transaction.
///
/// SSI conflicts are NOT errors. If `aborted_for_ssi` is true, the transaction
/// was rolled back due to a serialization conflict. The caller should retry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommitOutcome {
    /// The commit timestamp assigned to this transaction.
    /// Zero if the transaction was aborted for SSI.
    pub commit_ts: u64,

    /// True if the transaction was aborted due to an SSI conflict.
    /// When true, no data was written and the caller should retry.
    pub aborted_for_ssi: bool,
}

/// A database transaction.
///
/// Transactions provide snapshot isolation: all reads see a consistent snapshot
/// as of the transaction's begin timestamp. Writes are buffered until commit.
///
/// `Txn` is `Send` but not `Sync`: it can move between threads but cannot be
/// shared between threads simultaneously.
///
/// # Commit vs Rollback
///
/// - `commit()` attempts to commit. If SSI detects a dangerous structure,
///   it returns `Ok(CommitOutcome { aborted_for_ssi: true })`.
/// - `rollback()` explicitly aborts.
/// - Dropping an uncommitted transaction rolls back with a tracing warning.
pub struct Txn<E: Env + Clone> {
    inner: Arc<DbInner<E>>,
    ssi_txn: Option<SSITransaction>,
    committed: bool,
    // Make Txn !Sync
    _marker: PhantomData<*const ()>,
}

// Txn is Send (can move between threads) but not Sync (cannot be shared)
unsafe impl<E: Env + Clone> Send for Txn<E> {}

impl<E: Env + Clone> Txn<E> {
    pub(crate) fn new(inner: Arc<DbInner<E>>, ssi_txn: SSITransaction) -> Self {
        Self {
            inner,
            ssi_txn: Some(ssi_txn),
            committed: false,
            _marker: PhantomData,
        }
    }

    /// Returns the transaction's begin timestamp.
    ///
    /// This is the snapshot timestamp used for MVCC reads.
    /// Used primarily for testing crash recovery behavior.
    pub fn begin_ts(&self) -> u64 {
        self.ssi_txn.as_ref().map(|txn| txn.begin_ts).unwrap_or(0)
    }

    /// Gets a value by key.
    ///
    /// Returns `None` if the key does not exist in this transaction's snapshot.
    ///
    /// # Errors
    ///
    /// - `Error::InvalidArgument` if the key is empty or transaction finished.
    /// - `Error::Storage` on storage failure (I/O, corruption, etc.).
    pub fn get(&mut self, key: &[u8]) -> Result<Option<Bytes>> {
        if key.is_empty() {
            return Err(Error::InvalidArgument("key cannot be empty".to_string()));
        }

        let txn = self
            .ssi_txn
            .as_mut()
            .ok_or_else(|| Error::InvalidArgument("transaction already finished".to_string()))?;

        // Read through SSI manager (tracks read set, propagates errors)
        Ok(self.inner.ssi_manager.read(txn, key)?)
    }

    /// Writes a key-value pair.
    ///
    /// The write is buffered until commit. If the transaction is rolled back,
    /// the write is discarded.
    ///
    /// # Errors
    ///
    /// - `Error::InvalidArgument` if the key is empty.
    pub fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        if key.is_empty() {
            return Err(Error::InvalidArgument("key cannot be empty".to_string()));
        }

        let txn = self
            .ssi_txn
            .as_mut()
            .ok_or_else(|| Error::InvalidArgument("transaction already finished".to_string()))?;

        self.inner.ssi_manager.write(txn, key, value)?;

        Ok(())
    }

    /// Deletes a key.
    ///
    /// The deletion is buffered until commit. If the transaction is rolled back,
    /// the key remains unchanged.
    ///
    /// # Errors
    ///
    /// - `Error::InvalidArgument` if the key is empty.
    pub fn delete(&mut self, key: &[u8]) -> Result<()> {
        if key.is_empty() {
            return Err(Error::InvalidArgument("key cannot be empty".to_string()));
        }

        let txn = self
            .ssi_txn
            .as_mut()
            .ok_or_else(|| Error::InvalidArgument("transaction already finished".to_string()))?;

        self.inner.ssi_manager.delete(txn, key)?;

        Ok(())
    }

    /// Scans a range of keys.
    ///
    /// Returns an iterator over key-value pairs in the given range, ordered by
    /// key. Each key returns the newest version visible at this transaction's
    /// begin timestamp. Tombstones are filtered out (deleted keys don't appear).
    ///
    /// All returned keys are added to the SSI read set for conflict tracking.
    ///
    /// # Phantom Write Limitation
    ///
    /// This implementation does NOT detect phantom writes. A concurrent
    /// transaction may insert a new key in the scanned range and commit without
    /// causing a conflict. This matches PostgreSQL and CockroachDB behavior.
    /// See ADR-028 for details.
    ///
    /// # Errors
    ///
    /// - `Error::InvalidArgument` if the transaction is already finished.
    /// - `Error::Storage` on storage failure (I/O, corruption, etc.).
    pub fn scan<R>(&mut self, range: R) -> Result<Scan<E>>
    where
        R: RangeBounds<Vec<u8>>,
    {
        let txn = self
            .ssi_txn
            .as_mut()
            .ok_or_else(|| Error::InvalidArgument("transaction already finished".to_string()))?;

        // Convert RangeBounds<Vec<u8>> to Bound<&[u8]>
        let start: Bound<&[u8]> = match range.start_bound() {
            Bound::Included(k) => Bound::Included(k.as_slice()),
            Bound::Excluded(k) => Bound::Excluded(k.as_slice()),
            Bound::Unbounded => Bound::Unbounded,
        };
        let end: Bound<&[u8]> = match range.end_bound() {
            Bound::Included(k) => Bound::Included(k.as_slice()),
            Bound::Excluded(k) => Bound::Excluded(k.as_slice()),
            Bound::Unbounded => Bound::Unbounded,
        };

        // SSI scan returns Vec<(Bytes, Bytes)> with read tracking
        let results = self.inner.ssi_manager.scan(txn, start, end)?;

        Ok(Scan::new(results))
    }

    /// Attempts to commit the transaction.
    ///
    /// # Returns
    ///
    /// - `Ok(CommitOutcome { aborted_for_ssi: false, commit_ts })` on success.
    /// - `Ok(CommitOutcome { aborted_for_ssi: true, commit_ts: 0 })` if SSI
    ///   detected a serialization conflict. Retry the transaction.
    /// - `Err(...)` on actual errors (I/O failure, corruption, etc.). Do not
    ///   retry blindly.
    ///
    /// # Design Note
    ///
    /// SSI conflicts are NOT errors. They tell the caller "retry" which is
    /// fundamentally different from "the database is broken." This distinction
    /// is per ADR-022.
    pub fn commit(mut self) -> Result<CommitOutcome> {
        let mut txn = self
            .ssi_txn
            .take()
            .ok_or_else(|| Error::InvalidArgument("transaction already finished".to_string()))?;

        // Try SSI commit - writes are persisted via VersionStore::install_writes
        // (Stage 5b: engine-backed VersionStore handles all persistence)
        match self.inner.ssi_manager.commit(&mut txn) {
            Ok(commit_ts) => {
                // SSI commit succeeded - writes are already persisted to engine
                // via VersionStore::install_writes. No additional engine.put needed.
                self.committed = true;
                Ok(CommitOutcome {
                    commit_ts,
                    aborted_for_ssi: false,
                })
            }
            Err(SSIError::SerializationConflict) | Err(SSIError::WriteWriteConflict) => {
                // SSI conflict - not an error, just a retry signal
                self.committed = true; // Prevent Drop warning
                Ok(CommitOutcome {
                    commit_ts: 0,
                    aborted_for_ssi: true,
                })
            }
            Err(SSIError::AlreadyFinished) | Err(SSIError::Aborted) => {
                self.committed = true;
                Err(Error::InvalidArgument(
                    "transaction already finished".to_string(),
                ))
            }
            Err(SSIError::StorageError(e)) => {
                // Storage errors are actual errors, not SSI conflicts
                self.committed = true;
                Err(Error::Storage(e))
            }
        }
    }

    /// Explicitly rolls back the transaction.
    ///
    /// All buffered writes are discarded. This is equivalent to dropping the
    /// transaction without committing, but without the warning.
    pub fn rollback(mut self) {
        if let Some(mut txn) = self.ssi_txn.take() {
            let _ = self.inner.ssi_manager.abort(&mut txn);
        }
        self.committed = true; // Prevent Drop warning
    }
}

impl<E: Env + Clone> Drop for Txn<E> {
    fn drop(&mut self) {
        if !self.committed {
            if let Some(mut txn) = self.ssi_txn.take() {
                tracing::warn!(
                    txn_id = txn.id,
                    "transaction dropped without commit or rollback, rolling back"
                );
                let _ = self.inner.ssi_manager.abort(&mut txn);
            }
        }
    }
}

/// An iterator over key-value pairs from a scan.
///
/// Holds pre-collected results from SSI scan. The scan was performed at
/// the transaction's begin timestamp with read tracking.
pub struct Scan<E: Env + Clone> {
    results: std::vec::IntoIter<(Bytes, Bytes)>,
    _marker: PhantomData<E>,
}

impl<E: Env + Clone> Scan<E> {
    fn new(results: Vec<(Bytes, Bytes)>) -> Self {
        Self {
            results: results.into_iter(),
            _marker: PhantomData,
        }
    }
}

impl<E: Env + Clone> Iterator for Scan<E> {
    type Item = Result<(Bytes, Bytes)>;

    fn next(&mut self) -> Option<Self::Item> {
        self.results.next().map(Ok)
    }
}
