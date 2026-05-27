//! Transaction management.
//!
//! Handles transaction lifecycle: begin, read, write, commit, abort.
//!
//! # TLA+ Spec Reference
//!
//! This module implements the actions from `specs/MVCC.tla`:
//! - `Begin(t)`: Start a new transaction with a begin timestamp
//! - `Read(t, k)`: Read a key at the transaction's snapshot
//! - `Write(t, k, v)`: Buffer a write in the transaction's write set
//! - `Commit(t)`: Attempt to commit (check for write-write conflicts)
//! - `Abort(t)`: Abort and discard buffered writes
//!
//! # Deterministic Timestamps
//!
//! Timestamps are assigned from a `Mutex<u64>` counter, NOT from the wall clock.
//! This ensures deterministic behavior in simulation.

use bytes::Bytes;
use parking_lot::Mutex;
use runtime::Env;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use thiserror::Error;

use crate::version::VersionStore;

/// Transaction ID type.
pub type TxnId = u64;

/// Errors that can occur during transaction operations.
#[derive(Debug, Error)]
pub enum TxnError {
    /// Write-write conflict: another transaction committed to the same key
    /// after this transaction's begin timestamp.
    #[error("write-write conflict on key")]
    WriteWriteConflict,

    /// Transaction was already committed or aborted.
    #[error("transaction already finished")]
    AlreadyFinished,

    /// Transaction was aborted (e.g., due to conflict detection).
    #[error("transaction aborted")]
    Aborted,

    /// Storage layer error.
    #[error("storage error: {0}")]
    StorageError(#[from] storage::Error),
}

// Manual PartialEq impl because storage::Error doesn't implement PartialEq
impl PartialEq for TxnError {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (TxnError::WriteWriteConflict, TxnError::WriteWriteConflict) => true,
            (TxnError::AlreadyFinished, TxnError::AlreadyFinished) => true,
            (TxnError::Aborted, TxnError::Aborted) => true,
            // StorageErrors are considered equal if they're both StorageErrors
            // (we can't compare the inner error)
            (TxnError::StorageError(_), TxnError::StorageError(_)) => true,
            _ => false,
        }
    }
}

impl Eq for TxnError {}

/// Result type for transaction operations.
pub type TxnResult<T> = Result<T, TxnError>;

/// Transaction status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxnStatus {
    /// Transaction is running.
    Active,
    /// Transaction has committed.
    Committed,
    /// Transaction has aborted.
    Aborted,
}

/// A transaction in the MVCC system.
///
/// Transactions provide snapshot isolation:
/// - Reads see a consistent snapshot at `begin_ts`
/// - Writes are buffered until commit
/// - Commit fails if another transaction modified the same key
#[derive(Debug)]
pub struct Transaction {
    /// Unique transaction ID.
    pub id: TxnId,
    /// Begin timestamp (snapshot point).
    pub begin_ts: u64,
    /// Commit timestamp (assigned at commit time, 0 if not committed).
    pub commit_ts: u64,
    /// Current status.
    pub status: TxnStatus,
    /// Keys read by this transaction (for SSI tracking).
    pub read_set: BTreeSet<Bytes>,
    /// Buffered writes: key -> value (None = deletion).
    pub write_set: BTreeMap<Bytes, Option<Bytes>>,
}

impl Transaction {
    /// Create a new active transaction.
    fn new(id: TxnId, begin_ts: u64) -> Self {
        Self {
            id,
            begin_ts,
            commit_ts: 0,
            status: TxnStatus::Active,
            read_set: BTreeSet::new(),
            write_set: BTreeMap::new(),
        }
    }

    /// Check if the transaction is still active.
    pub fn is_active(&self) -> bool {
        self.status == TxnStatus::Active
    }

    /// Check if the transaction has committed.
    pub fn is_committed(&self) -> bool {
        self.status == TxnStatus::Committed
    }

    /// Check if the transaction has aborted.
    pub fn is_aborted(&self) -> bool {
        self.status == TxnStatus::Aborted
    }
}

/// Transaction manager for MVCC.
///
/// Manages transaction lifecycle, timestamp assignment, and conflict detection.
///
/// # Thread Safety
///
/// The manager is thread-safe. Multiple transactions can run concurrently.
/// Conflict detection happens at commit time using the version store.
#[derive(Debug)]
pub struct TransactionManager<E: Env + Clone + 'static> {
    /// Timestamp counter for deterministic timestamp assignment.
    /// Uses Mutex<u64> to ensure sequential timestamp assignment.
    next_ts: Mutex<u64>,
    /// Transaction ID counter.
    next_txn_id: AtomicU64,
    /// Version store for reading and writing versions (engine-backed).
    versions: Arc<VersionStore<E>>,
}

impl<E: Env + Clone + 'static> TransactionManager<E> {
    /// Create a new transaction manager.
    ///
    /// Initializes next_ts from the engine's recovery values.
    pub fn new(versions: Arc<VersionStore<E>>) -> Self {
        let recovered_ts = versions.max_commit_ts();
        Self {
            next_ts: Mutex::new(if recovered_ts > 0 {
                recovered_ts + 1
            } else {
                1
            }),
            next_txn_id: AtomicU64::new(1),
            versions,
        }
    }

    /// Begin a new transaction.
    ///
    /// Assigns a begin timestamp for snapshot reads.
    ///
    /// # TLA+ Spec Reference
    ///
    /// Implements `Begin(t)` from MVCC.tla:
    /// ```tla
    /// /\ txnStatus' = [txnStatus EXCEPT ![t] = TXN_ACTIVE]
    /// /\ beginTs' = [beginTs EXCEPT ![t] = nextTs]
    /// /\ nextTs' = nextTs + 1
    /// ```
    pub fn begin(&self) -> Transaction {
        let id = self.next_txn_id.fetch_add(1, Ordering::Relaxed);
        let begin_ts = {
            let mut ts = self.next_ts.lock();
            let current = *ts;
            *ts += 1;
            current
        };
        Transaction::new(id, begin_ts)
    }

    /// Read a key at the transaction's snapshot.
    ///
    /// Records the key in the read set (for SSI tracking in Stage 6).
    /// Returns the value visible at the transaction's begin timestamp.
    ///
    /// # TLA+ Spec Reference
    ///
    /// Implements `Read(t, k)` from MVCC.tla:
    /// ```tla
    /// /\ readSet' = [readSet EXCEPT ![t] = readSet[t] \union {k}]
    /// ```
    pub fn read(&self, txn: &mut Transaction, key: &[u8]) -> TxnResult<Option<Bytes>> {
        if !txn.is_active() {
            return Err(TxnError::AlreadyFinished);
        }

        // Record in read set (for SSI)
        txn.read_set.insert(Bytes::copy_from_slice(key));

        // Check write set first (read-your-writes)
        if let Some(value) = txn.write_set.get(key) {
            return Ok(value.clone());
        }

        // Read from version store at begin timestamp
        Ok(self.versions.read_at(key, txn.begin_ts)?)
    }

    /// Buffer a write in the transaction's write set.
    ///
    /// The write is not visible until commit.
    ///
    /// # TLA+ Spec Reference
    ///
    /// Implements `Write(t, k, v)` from MVCC.tla:
    /// ```tla
    /// /\ writeSet' = [writeSet EXCEPT ![t] = ...]
    /// ```
    pub fn write(&self, txn: &mut Transaction, key: &[u8], value: &[u8]) -> TxnResult<()> {
        if !txn.is_active() {
            return Err(TxnError::AlreadyFinished);
        }

        txn.write_set.insert(
            Bytes::copy_from_slice(key),
            Some(Bytes::from(value.to_vec())),
        );
        Ok(())
    }

    /// Buffer a deletion in the transaction's write set.
    ///
    /// The deletion is not visible until commit.
    pub fn delete(&self, txn: &mut Transaction, key: &[u8]) -> TxnResult<()> {
        if !txn.is_active() {
            return Err(TxnError::AlreadyFinished);
        }

        txn.write_set.insert(Bytes::copy_from_slice(key), None);
        Ok(())
    }

    /// Attempt to commit the transaction.
    ///
    /// Checks for write-write conflicts and installs writes atomically.
    ///
    /// # TLA+ Spec Reference
    ///
    /// Implements `Commit(t)` from MVCC.tla:
    /// ```tla
    /// /\ \A k \in DOMAIN writeSet[t]: ~HasConflictingWrite(k, beginTs[t])
    /// /\ commitTs' = [commitTs EXCEPT ![t] = nextTs]
    /// /\ versions' = [k \in Keys |->
    ///      IF k \in DOMAIN writeSet[t]
    ///      THEN Append(versions[k], <<nextTs, writeSet[t][k]>>)
    ///      ELSE versions[k]]
    /// /\ nextTs' = nextTs + 1
    /// ```
    pub fn commit(&self, txn: &mut Transaction) -> TxnResult<u64> {
        if !txn.is_active() {
            return Err(TxnError::AlreadyFinished);
        }

        // Check for write-write conflicts
        for key in txn.write_set.keys() {
            if self.versions.has_write_after(key, txn.begin_ts)? {
                txn.status = TxnStatus::Aborted;
                return Err(TxnError::WriteWriteConflict);
            }
        }

        // Assign commit timestamp
        let commit_ts = {
            let mut ts = self.next_ts.lock();
            let current = *ts;
            *ts += 1;
            current
        };

        // Install writes atomically - now persists to engine
        let writes: Vec<_> = txn
            .write_set
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        // Handle storage errors
        if let Err(e) = self.versions.install_writes(commit_ts, writes) {
            txn.status = TxnStatus::Aborted;
            return Err(TxnError::StorageError(e));
        }

        txn.commit_ts = commit_ts;
        txn.status = TxnStatus::Committed;

        Ok(commit_ts)
    }

    /// Abort the transaction.
    ///
    /// Discards all buffered writes.
    ///
    /// # TLA+ Spec Reference
    ///
    /// Implements `Abort(t)` from MVCC.tla:
    /// ```tla
    /// /\ txnStatus' = [txnStatus EXCEPT ![t] = TXN_ABORTED]
    /// ```
    pub fn abort(&self, txn: &mut Transaction) -> TxnResult<()> {
        if !txn.is_active() {
            return Err(TxnError::AlreadyFinished);
        }

        txn.status = TxnStatus::Aborted;
        // Write set is implicitly discarded (not installed to versions)
        Ok(())
    }

    /// Get the current timestamp (for testing/debugging).
    pub fn current_ts(&self) -> u64 {
        *self.next_ts.lock()
    }

    /// Get a reference to the version store.
    pub fn versions(&self) -> &Arc<VersionStore<E>> {
        &self.versions
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use runtime::{SimEnv, SimEnvConfig};
    use storage::{Engine, EngineConfig};

    fn make_manager() -> TransactionManager<SimEnv> {
        let env = SimEnv::new(SimEnvConfig::with_seed(42));
        env.create_dir_all(runtime::Path::new("/db")).unwrap();
        let engine = Arc::new(
            Engine::open(env, runtime::Path::new("/db"), EngineConfig::default()).unwrap(),
        );
        let versions = Arc::new(VersionStore::new(engine));
        TransactionManager::new(versions)
    }

    #[allow(dead_code)]
    fn make_manager_with_seed(seed: u64) -> TransactionManager<SimEnv> {
        let env = SimEnv::new(SimEnvConfig::with_seed(seed));
        let path_str = format!("/db_{}", seed);
        let path = runtime::Path::new(&path_str);
        env.create_dir_all(path).unwrap();
        let engine = Arc::new(Engine::open(env, path, EngineConfig::default()).unwrap());
        let versions = Arc::new(VersionStore::new(engine));
        TransactionManager::new(versions)
    }

    #[test]
    fn begin_transaction() {
        let mgr = make_manager();

        let t1 = mgr.begin();
        assert_eq!(t1.id, 1);
        assert_eq!(t1.begin_ts, 1);
        assert!(t1.is_active());

        let t2 = mgr.begin();
        assert_eq!(t2.id, 2);
        assert_eq!(t2.begin_ts, 2);
    }

    #[test]
    fn read_nonexistent_key() {
        let mgr = make_manager();
        let mut txn = mgr.begin();

        let result = mgr.read(&mut txn, b"missing").unwrap();
        assert!(result.is_none());
        assert!(txn.read_set.contains(&Bytes::from("missing")));
    }

    #[test]
    fn write_and_read_your_writes() {
        let mgr = make_manager();
        let mut txn = mgr.begin();

        mgr.write(&mut txn, b"k1", b"v1").unwrap();
        let result = mgr.read(&mut txn, b"k1").unwrap();
        assert_eq!(result, Some(Bytes::from("v1")));
    }

    #[test]
    fn commit_empty_transaction() {
        let mgr = make_manager();
        let mut txn = mgr.begin();
        let begin_ts = txn.begin_ts;

        let commit_ts = mgr.commit(&mut txn).unwrap();
        assert!(commit_ts > begin_ts);
        assert!(txn.is_committed());
        assert_eq!(txn.commit_ts, commit_ts);
    }

    #[test]
    fn commit_with_writes() {
        let mgr = make_manager();

        let mut t1 = mgr.begin();
        mgr.write(&mut t1, b"k1", b"v1").unwrap();
        mgr.commit(&mut t1).unwrap();

        // New transaction should see the committed value
        let mut t2 = mgr.begin();
        let result = mgr.read(&mut t2, b"k1").unwrap();
        assert_eq!(result, Some(Bytes::from("v1")));
    }

    #[test]
    fn snapshot_isolation() {
        let mgr = make_manager();

        // T1 writes k1=v1 and commits
        let mut t1 = mgr.begin();
        mgr.write(&mut t1, b"k1", b"v1").unwrap();
        mgr.commit(&mut t1).unwrap();

        // T2 starts and sees k1=v1
        let mut t2 = mgr.begin();
        assert_eq!(mgr.read(&mut t2, b"k1").unwrap(), Some(Bytes::from("v1")));

        // T3 writes k1=v2 and commits
        let mut t3 = mgr.begin();
        mgr.write(&mut t3, b"k1", b"v2").unwrap();
        mgr.commit(&mut t3).unwrap();

        // T2 still sees k1=v1 (its snapshot)
        assert_eq!(mgr.read(&mut t2, b"k1").unwrap(), Some(Bytes::from("v1")));

        // T4 starts and sees k1=v2
        let mut t4 = mgr.begin();
        assert_eq!(mgr.read(&mut t4, b"k1").unwrap(), Some(Bytes::from("v2")));
    }

    #[test]
    fn write_write_conflict() {
        let mgr = make_manager();

        // T1 starts
        let mut t1 = mgr.begin();

        // T2 writes k1 and commits
        let mut t2 = mgr.begin();
        mgr.write(&mut t2, b"k1", b"v2").unwrap();
        mgr.commit(&mut t2).unwrap();

        // T1 tries to write k1 and commit -> conflict
        mgr.write(&mut t1, b"k1", b"v1").unwrap();
        let result = mgr.commit(&mut t1);
        assert_eq!(result, Err(TxnError::WriteWriteConflict));
        assert!(t1.is_aborted());
    }

    #[test]
    fn no_conflict_on_different_keys() {
        let mgr = make_manager();

        let mut t1 = mgr.begin();
        let mut t2 = mgr.begin();

        mgr.write(&mut t1, b"k1", b"v1").unwrap();
        mgr.write(&mut t2, b"k2", b"v2").unwrap();

        mgr.commit(&mut t1).unwrap();
        mgr.commit(&mut t2).unwrap(); // Should succeed

        assert!(t1.is_committed());
        assert!(t2.is_committed());
    }

    #[test]
    fn abort_transaction() {
        let mgr = make_manager();

        let mut txn = mgr.begin();
        mgr.write(&mut txn, b"k1", b"v1").unwrap();
        mgr.abort(&mut txn).unwrap();

        assert!(txn.is_aborted());

        // Write should not be visible
        let mut t2 = mgr.begin();
        assert!(mgr.read(&mut t2, b"k1").unwrap().is_none());
    }

    #[test]
    fn delete_key() {
        let mgr = make_manager();

        // Write and commit
        let mut t1 = mgr.begin();
        mgr.write(&mut t1, b"k1", b"v1").unwrap();
        mgr.commit(&mut t1).unwrap();

        // Delete and commit
        let mut t2 = mgr.begin();
        mgr.delete(&mut t2, b"k1").unwrap();
        mgr.commit(&mut t2).unwrap();

        // Should be deleted
        let mut t3 = mgr.begin();
        assert!(mgr.read(&mut t3, b"k1").unwrap().is_none());
    }

    #[test]
    fn operations_on_finished_txn() {
        let mgr = make_manager();

        let mut txn = mgr.begin();
        mgr.commit(&mut txn).unwrap();

        assert_eq!(mgr.read(&mut txn, b"k1"), Err(TxnError::AlreadyFinished));
        assert_eq!(
            mgr.write(&mut txn, b"k1", b"v1"),
            Err(TxnError::AlreadyFinished)
        );
        assert_eq!(mgr.commit(&mut txn), Err(TxnError::AlreadyFinished));
        assert_eq!(mgr.abort(&mut txn), Err(TxnError::AlreadyFinished));
    }

    #[test]
    fn timestamp_monotonicity() {
        let mgr = make_manager();

        let t1 = mgr.begin();
        let t2 = mgr.begin();
        let t3 = mgr.begin();

        assert!(t1.begin_ts < t2.begin_ts);
        assert!(t2.begin_ts < t3.begin_ts);
    }

    #[test]
    fn commit_ts_after_begin_ts() {
        let mgr = make_manager();

        let mut txn = mgr.begin();
        let begin_ts = txn.begin_ts;
        let commit_ts = mgr.commit(&mut txn).unwrap();

        assert!(commit_ts > begin_ts);
    }
}
