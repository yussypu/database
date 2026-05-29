//! Version chain management for MVCC - Engine-backed implementation.
//!
//! This module provides the interface between MVCC transactions and the
//! storage engine. All versioned data is stored directly in the LSM engine,
//! not in an in-memory cache.
//!
//! # TLA+ Spec Reference
//!
//! This corresponds to the `versions` state variable in `specs/MVCC.tla`.
//! The `ReadAtTimestamp` operator maps to `VersionStore::read_at()`.
//!
//! # Stage 5b Integration
//!
//! ADR-025 documents the redo of MVCC-storage integration. The previous
//! VersionStore was purely in-memory (BTreeMap). This version routes all
//! operations through the LSM Engine for proper durability.

use bytes::Bytes;
use runtime::Env;
use std::ops::Bound;
use std::sync::Arc;
use storage::{Engine, EngineScan, Result as StorageResult};

/// Engine-backed version store for MVCC.
///
/// All reads and writes go directly to the storage engine.
/// No in-memory version chains are maintained.
pub struct VersionStore<E: Env + Clone + 'static> {
    engine: Arc<Engine<E>>,
}

impl<E: Env + Clone + 'static> std::fmt::Debug for VersionStore<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VersionStore")
            .field("engine", &"<Engine>")
            .finish()
    }
}

impl<E: Env + Clone + 'static> VersionStore<E> {
    /// Create a new version store backed by the given engine.
    pub fn new(engine: Arc<Engine<E>>) -> Self {
        Self { engine }
    }

    /// Read the value visible at the given timestamp.
    ///
    /// Returns the value from the most recent version with `commit_ts <= ts`.
    /// Returns `Ok(None)` if no version exists at or before the timestamp,
    /// or if the most recent version is a tombstone.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying engine read fails (I/O error,
    /// corruption, etc.). Errors are NOT silently swallowed.
    ///
    /// # TLA+ Spec Reference
    ///
    /// This implements `ReadAtTimestamp(k, ts)` from MVCC.tla.
    pub fn read_at(&self, key: &[u8], ts: u64) -> StorageResult<Option<Bytes>> {
        // Route through engine's MVCC read
        self.engine.get_at(key, ts)
    }

    /// Check if any version exists with commit_ts > ts.
    ///
    /// Used for write-write conflict detection: if another transaction
    /// committed a version after our begin timestamp, we have a conflict.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying engine read fails. This is a
    /// correctness-critical operation: defaulting to "no conflict" on error
    /// would allow commits that violate serializability.
    ///
    /// # TLA+ Spec Reference
    ///
    /// This implements `HasConflictingWrite(k, ts)` from MVCC.tla.
    pub fn has_write_after(&self, key: &[u8], ts: u64) -> StorageResult<bool> {
        self.engine.has_write_after(key, ts)
    }

    /// Install a batch of writes atomically.
    ///
    /// All writes are committed with the same timestamp.
    /// Writes are persisted to the WAL and memtable.
    /// Caller must ensure no conflicting writes exist (via `has_write_after`).
    ///
    /// This method uses group commit to batch the writes into a single WAL
    /// record and share fsync across concurrent transactions.
    pub fn install_writes(
        &self,
        commit_ts: u64,
        writes: impl IntoIterator<Item = (Bytes, Option<Bytes>)>,
    ) -> StorageResult<()> {
        // Use batched commit which:
        // 1. Encodes all writes into a single WAL record
        // 2. Commits via group commit (shares fsync with other transactions)
        // 3. Applies to memtable after fsync completes
        self.engine.put_versioned_batch(commit_ts, writes)
    }

    /// Get a reference to the underlying engine.
    pub fn engine(&self) -> &Arc<Engine<E>> {
        &self.engine
    }

    /// Returns the maximum commit timestamp seen during recovery.
    ///
    /// Used to initialize SSI's next_ts after crash recovery.
    pub fn max_commit_ts(&self) -> u64 {
        self.engine.max_commit_ts()
    }

    /// Returns the maximum transaction ID seen during recovery.
    ///
    /// Used to initialize SSI's next_txn_id after crash recovery.
    pub fn max_txn_id(&self) -> u64 {
        self.engine.max_txn_id()
    }

    /// Scan all keys in a range visible at the given timestamp.
    ///
    /// Returns an iterator over key-value pairs where each key is returned
    /// with its newest version that has `commit_ts <= ts`. Tombstones are
    /// filtered out (deleted keys do not appear in results).
    ///
    /// # Arguments
    ///
    /// * `start` - Start bound of the range
    /// * `end` - End bound of the range
    /// * `ts` - Snapshot timestamp; only versions with commit_ts <= ts are visible
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying engine scan fails.
    ///
    /// # TLA+ Spec Reference
    ///
    /// This extends `ReadAtTimestamp` semantics to range queries.
    pub fn scan_at(
        &self,
        start: Bound<&[u8]>,
        end: Bound<&[u8]>,
        ts: u64,
    ) -> StorageResult<EngineScan<'_, E>> {
        self.engine.scan_at_snapshot(start, end, ts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use runtime::{SimEnv, SimEnvConfig};
    use storage::EngineConfig;

    fn test_env() -> SimEnv {
        SimEnv::new(SimEnvConfig::with_seed(42))
    }

    #[test]
    fn version_store_basic_read_write() {
        let env = test_env();
        env.create_dir_all(runtime::Path::new("/db")).unwrap();

        let engine = Arc::new(
            Engine::open(env, runtime::Path::new("/db"), EngineConfig::default()).unwrap(),
        );
        let store = VersionStore::new(engine);

        // Initially empty
        assert!(store.read_at(b"k1", 100).unwrap().is_none());

        // Install a write at timestamp 10
        store
            .install_writes(10, vec![(Bytes::from("k1"), Some(Bytes::from("v1")))])
            .unwrap();

        // Read at different timestamps
        assert!(store.read_at(b"k1", 5).unwrap().is_none()); // Before write
        assert_eq!(store.read_at(b"k1", 10).unwrap(), Some(Bytes::from("v1")));
        assert_eq!(store.read_at(b"k1", 100).unwrap(), Some(Bytes::from("v1")));
    }

    #[test]
    fn version_store_multiple_versions() {
        let env = test_env();
        env.create_dir_all(runtime::Path::new("/db")).unwrap();

        let engine = Arc::new(
            Engine::open(env, runtime::Path::new("/db"), EngineConfig::default()).unwrap(),
        );
        let store = VersionStore::new(engine);

        // Write multiple versions
        store
            .install_writes(10, vec![(Bytes::from("k1"), Some(Bytes::from("v1")))])
            .unwrap();
        store
            .install_writes(20, vec![(Bytes::from("k1"), Some(Bytes::from("v2")))])
            .unwrap();
        store
            .install_writes(30, vec![(Bytes::from("k1"), Some(Bytes::from("v3")))])
            .unwrap();

        // Read at different timestamps
        assert!(store.read_at(b"k1", 5).unwrap().is_none());
        assert_eq!(store.read_at(b"k1", 10).unwrap(), Some(Bytes::from("v1")));
        assert_eq!(store.read_at(b"k1", 15).unwrap(), Some(Bytes::from("v1")));
        assert_eq!(store.read_at(b"k1", 20).unwrap(), Some(Bytes::from("v2")));
        assert_eq!(store.read_at(b"k1", 25).unwrap(), Some(Bytes::from("v2")));
        assert_eq!(store.read_at(b"k1", 30).unwrap(), Some(Bytes::from("v3")));
        assert_eq!(store.read_at(b"k1", 100).unwrap(), Some(Bytes::from("v3")));
    }

    #[test]
    fn version_store_conflict_detection() {
        let env = test_env();
        env.create_dir_all(runtime::Path::new("/db")).unwrap();

        let engine = Arc::new(
            Engine::open(env, runtime::Path::new("/db"), EngineConfig::default()).unwrap(),
        );
        let store = VersionStore::new(engine);

        store
            .install_writes(10, vec![(Bytes::from("k1"), Some(Bytes::from("v1")))])
            .unwrap();

        assert!(store.has_write_after(b"k1", 5).unwrap()); // Write at 10 > 5
        assert!(!store.has_write_after(b"k1", 10).unwrap()); // No write > 10
        assert!(!store.has_write_after(b"k2", 5).unwrap()); // Key doesn't exist
    }

    #[test]
    fn version_store_survives_crash() {
        let env = test_env();
        env.create_dir_all(runtime::Path::new("/db")).unwrap();

        // Write data
        {
            let engine = Arc::new(
                Engine::open(
                    env.clone(),
                    runtime::Path::new("/db"),
                    EngineConfig::default(),
                )
                .unwrap(),
            );
            let store = VersionStore::new(engine);

            store
                .install_writes(10, vec![(Bytes::from("k1"), Some(Bytes::from("v1")))])
                .unwrap();
            store
                .install_writes(20, vec![(Bytes::from("k2"), Some(Bytes::from("v2")))])
                .unwrap();
        }

        // Simulate crash
        env.simulate_crash();

        // Reopen and verify data survived
        {
            let engine = Arc::new(
                Engine::open(env, runtime::Path::new("/db"), EngineConfig::default()).unwrap(),
            );
            let store = VersionStore::new(engine);

            assert_eq!(store.read_at(b"k1", 100).unwrap(), Some(Bytes::from("v1")));
            assert_eq!(store.read_at(b"k2", 100).unwrap(), Some(Bytes::from("v2")));
        }
    }

    /// Test that read_at propagates engine errors instead of swallowing them.
    ///
    /// This test is marked #[ignore] because SimEnv fault injection does not
    /// currently reach engine.get_at() - faults are injected at the file I/O
    /// level but get_at() catches them internally. To fully test error
    /// propagation, we would need:
    /// 1. A mock Engine that can be configured to return errors, or
    /// 2. Fault injection that reaches the Engine::get_at() call site.
    ///
    /// The change from `.ok().flatten()` to direct propagation guarantees that
    /// when get_at() does return Err, read_at() returns Err (not Ok(None)).
    #[test]
    #[ignore]
    fn read_at_propagates_engine_errors() {
        // When fault injection is extended to reach Engine::get_at():
        // 1. Create engine with fault injection enabled
        // 2. Configure a read fault
        // 3. Call store.read_at()
        // 4. Assert that it returns Err, not Ok(None)
        //
        // Until then, the type signature change (-> StorageResult<Option<Bytes>>)
        // is the compile-time guarantee that errors are propagated.
    }

    #[test]
    fn version_store_scan_at_basic() {
        let env = test_env();
        env.create_dir_all(runtime::Path::new("/db")).unwrap();

        let engine = Arc::new(
            Engine::open(env, runtime::Path::new("/db"), EngineConfig::default()).unwrap(),
        );
        let store = VersionStore::new(engine);

        // Write some data
        store
            .install_writes(
                10,
                vec![
                    (Bytes::from("a"), Some(Bytes::from("a1"))),
                    (Bytes::from("b"), Some(Bytes::from("b1"))),
                    (Bytes::from("c"), Some(Bytes::from("c1"))),
                ],
            )
            .unwrap();

        // Scan all at ts=20
        let results: Vec<_> = store
            .scan_at(Bound::Unbounded, Bound::Unbounded, 20)
            .unwrap()
            .collect::<storage::Result<Vec<_>>>()
            .unwrap();

        assert_eq!(results.len(), 3);
        assert_eq!(results[0], (Bytes::from("a"), Bytes::from("a1")));
        assert_eq!(results[1], (Bytes::from("b"), Bytes::from("b1")));
        assert_eq!(results[2], (Bytes::from("c"), Bytes::from("c1")));
    }

    #[test]
    fn version_store_scan_at_respects_timestamp() {
        let env = test_env();
        env.create_dir_all(runtime::Path::new("/db")).unwrap();

        let engine = Arc::new(
            Engine::open(env, runtime::Path::new("/db"), EngineConfig::default()).unwrap(),
        );
        let store = VersionStore::new(engine);

        // Write versions at different timestamps
        store
            .install_writes(10, vec![(Bytes::from("a"), Some(Bytes::from("a1")))])
            .unwrap();
        store
            .install_writes(20, vec![(Bytes::from("b"), Some(Bytes::from("b1")))])
            .unwrap();
        store
            .install_writes(30, vec![(Bytes::from("a"), Some(Bytes::from("a2")))])
            .unwrap();

        // At ts=15: only a@10 visible
        let results: Vec<_> = store
            .scan_at(Bound::Unbounded, Bound::Unbounded, 15)
            .unwrap()
            .collect::<storage::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0], (Bytes::from("a"), Bytes::from("a1")));

        // At ts=25: a@10 and b@20 visible
        let results: Vec<_> = store
            .scan_at(Bound::Unbounded, Bound::Unbounded, 25)
            .unwrap()
            .collect::<storage::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0], (Bytes::from("a"), Bytes::from("a1")));
        assert_eq!(results[1], (Bytes::from("b"), Bytes::from("b1")));

        // At ts=35: a@30 (newer version) and b@20 visible
        let results: Vec<_> = store
            .scan_at(Bound::Unbounded, Bound::Unbounded, 35)
            .unwrap()
            .collect::<storage::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0], (Bytes::from("a"), Bytes::from("a2")));
        assert_eq!(results[1], (Bytes::from("b"), Bytes::from("b1")));
    }

    #[test]
    fn version_store_scan_at_with_tombstones() {
        let env = test_env();
        env.create_dir_all(runtime::Path::new("/db")).unwrap();

        let engine = Arc::new(
            Engine::open(env, runtime::Path::new("/db"), EngineConfig::default()).unwrap(),
        );
        let store = VersionStore::new(engine);

        // Write data then delete
        store
            .install_writes(
                10,
                vec![
                    (Bytes::from("a"), Some(Bytes::from("a1"))),
                    (Bytes::from("b"), Some(Bytes::from("b1"))),
                ],
            )
            .unwrap();
        store
            .install_writes(20, vec![(Bytes::from("a"), None)]) // Tombstone
            .unwrap();

        // At ts=15: both visible
        let results: Vec<_> = store
            .scan_at(Bound::Unbounded, Bound::Unbounded, 15)
            .unwrap()
            .collect::<storage::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(results.len(), 2);

        // At ts=25: only b visible (a deleted)
        let results: Vec<_> = store
            .scan_at(Bound::Unbounded, Bound::Unbounded, 25)
            .unwrap()
            .collect::<storage::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0], (Bytes::from("b"), Bytes::from("b1")));
    }
}
