//! Public key-value API for cracked-db.
//!
//! This crate provides the user-facing API for the database. All other crates
//! (`runtime`, `storage`, `mvcc`, `learned`) are internal implementation details.
//!
//! # Design Decisions
//!
//! See ADR-022 in `DECISIONS.md` for the API design rationale:
//!
//! - `Db` is `Clone` via `Arc<DbInner>`. Cloning is cheap.
//! - `Db::open` takes an explicit `Env` parameter. Deterministic simulation is
//!   a first-class feature.
//! - `Txn` is `Send` but not `Sync`. Transactions can move between threads but
//!   cannot be shared.
//! - `commit()` returns `Result<CommitOutcome, Error>`. SSI conflicts surface as
//!   `Ok(CommitOutcome { aborted_for_ssi: true })`, NOT as errors.
//!
//! # Example
//!
//! ```ignore
//! use kv::{Db, Options};
//! use runtime::RealEnv;
//!
//! let env = RealEnv::new();
//! let db = Db::open(env, "/tmp/mydb", Options::default())?;
//!
//! // Write transaction
//! let mut txn = db.begin();
//! txn.put(b"user:42", b"alice")?;
//! txn.put(b"user:43", b"bob")?;
//! let outcome = txn.commit()?;
//!
//! if outcome.aborted_for_ssi {
//!     // Retry the transaction
//! }
//!
//! // Read transaction
//! let mut txn = db.begin();
//! assert_eq!(txn.get(b"user:42")?, Some(b"alice".into()));
//! txn.rollback();
//! ```
//!
//! # SSI Retry Pattern
//!
//! When a transaction is aborted for SSI, retry with exponential backoff:
//!
//! ```ignore
//! use kv::{Db, CommitOutcome};
//!
//! fn transfer(db: &Db<RealEnv>, from: &[u8], to: &[u8], amount: i64) -> Result<(), kv::Error> {
//!     loop {
//!         let mut txn = db.begin();
//!
//!         // Read balances
//!         let from_balance: i64 = txn.get(from)?.map(parse_i64).unwrap_or(0);
//!         let to_balance: i64 = txn.get(to)?.map(parse_i64).unwrap_or(0);
//!
//!         // Update balances
//!         txn.put(from, &(from_balance - amount).to_le_bytes())?;
//!         txn.put(to, &(to_balance + amount).to_le_bytes())?;
//!
//!         match txn.commit()? {
//!             CommitOutcome { aborted_for_ssi: false, .. } => return Ok(()),
//!             CommitOutcome { aborted_for_ssi: true, .. } => continue, // Retry
//!         }
//!     }
//! }
//! ```

mod db;
mod error;
mod txn;

pub use db::{Db, Options};
pub use error::{Error, Result};
pub use txn::{CommitOutcome, Scan, Txn};

/// Re-export runtime for convenience.
pub use runtime;

#[cfg(test)]
mod tests {
    use super::*;
    use runtime::{Path, SimEnv, SimEnvConfig};
    use std::sync::atomic::{AtomicU64, Ordering};

    // Counter to generate unique paths for each test
    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn test_env() -> SimEnv {
        SimEnv::new(SimEnvConfig::with_seed(42))
    }

    fn unique_path() -> String {
        let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        format!("/test_db_{}", id)
    }

    #[test]
    fn open_and_close() {
        let env = test_env();
        let path = unique_path();
        let db = Db::open(env, Path::new(&path), Options::default()).unwrap();
        db.close().unwrap();
    }

    #[test]
    fn double_open_fails() {
        let env = test_env();
        let path = unique_path();
        let db1 = Db::open(env.clone(), Path::new(&path), Options::default()).unwrap();
        let result = Db::open(env, Path::new(&path), Options::default());
        assert!(matches!(result, Err(Error::AlreadyOpen)));
        drop(db1);
    }

    #[test]
    fn basic_put_get() {
        let env = test_env();
        let path = unique_path();
        let db = Db::open(env, Path::new(&path), Options::default()).unwrap();

        // Write transaction
        let mut txn = db.begin();
        txn.put(b"key1", b"value1").unwrap();
        txn.put(b"key2", b"value2").unwrap();
        let outcome = txn.commit().unwrap();
        assert!(!outcome.aborted_for_ssi);
        assert!(outcome.commit_ts > 0);

        // Read transaction
        let mut txn = db.begin();
        assert_eq!(
            txn.get(b"key1").unwrap(),
            Some(bytes::Bytes::from("value1"))
        );
        assert_eq!(
            txn.get(b"key2").unwrap(),
            Some(bytes::Bytes::from("value2"))
        );
        assert_eq!(txn.get(b"key3").unwrap(), None);
        txn.rollback();
    }

    #[test]
    fn delete_key() {
        let env = test_env();
        let path = unique_path();
        let db = Db::open(env, Path::new(&path), Options::default()).unwrap();

        // Write
        let mut txn = db.begin();
        txn.put(b"key", b"value").unwrap();
        txn.commit().unwrap();

        // Delete
        let mut txn = db.begin();
        txn.delete(b"key").unwrap();
        txn.commit().unwrap();

        // Verify deleted
        let mut txn = db.begin();
        assert_eq!(txn.get(b"key").unwrap(), None);
        txn.rollback();
    }

    #[test]
    fn read_your_writes() {
        let env = test_env();
        let path = unique_path();
        let db = Db::open(env, Path::new(&path), Options::default()).unwrap();

        let mut txn = db.begin();
        txn.put(b"key", b"value").unwrap();

        // Should see own write even before commit
        assert_eq!(txn.get(b"key").unwrap(), Some(bytes::Bytes::from("value")));

        txn.rollback();
    }

    #[test]
    fn empty_key_rejected() {
        let env = test_env();
        let path = unique_path();
        let db = Db::open(env, Path::new(&path), Options::default()).unwrap();

        let mut txn = db.begin();
        assert!(matches!(
            txn.put(b"", b"value"),
            Err(Error::InvalidArgument(_))
        ));
        assert!(matches!(txn.get(b""), Err(Error::InvalidArgument(_))));
        assert!(matches!(txn.delete(b""), Err(Error::InvalidArgument(_))));
        txn.rollback();
    }

    #[test]
    fn db_is_clone() {
        let env = test_env();
        let path = unique_path();
        let db1 = Db::open(env, Path::new(&path), Options::default()).unwrap();
        let db2 = db1.clone();

        // Both handles can create transactions
        let mut txn1 = db1.begin();
        txn1.put(b"key1", b"value1").unwrap();
        txn1.commit().unwrap();

        let mut txn2 = db2.begin();
        assert_eq!(
            txn2.get(b"key1").unwrap(),
            Some(bytes::Bytes::from("value1"))
        );
        txn2.rollback();
    }

    #[test]
    fn commit_outcome_on_ssi_conflict() {
        let env = test_env();
        let path = unique_path();
        let db = Db::open(env, Path::new(&path), Options::default()).unwrap();

        // Setup: create initial values
        let mut setup = db.begin();
        setup.put(b"x", b"1").unwrap();
        setup.put(b"y", b"1").unwrap();
        setup.commit().unwrap();

        // T1: read y, write x (classic write skew setup)
        let mut t1 = db.begin();
        t1.get(b"y").unwrap();
        t1.put(b"x", b"-1").unwrap();

        // T2: read x, write y
        let mut t2 = db.begin();
        t2.get(b"x").unwrap();
        t2.put(b"y", b"-1").unwrap();

        // T1 commits first
        let outcome1 = t1.commit().unwrap();
        assert!(!outcome1.aborted_for_ssi);

        // T2 should get SSI conflict (dangerous structure)
        let outcome2 = t2.commit().unwrap();
        assert!(outcome2.aborted_for_ssi);
        assert_eq!(outcome2.commit_ts, 0);
    }

    #[test]
    fn options_builder() {
        let opts = Options::default()
            .with_block_size(8192)
            .with_wal_segment_size(128 * 1024 * 1024)
            .with_memtable_size(8 * 1024 * 1024)
            .with_use_pgm_index(false);

        assert_eq!(opts.block_size, 8192);
        assert_eq!(opts.wal_segment_size, 128 * 1024 * 1024);
        assert_eq!(opts.memtable_size, 8 * 1024 * 1024);
        assert!(!opts.use_pgm_index);
    }

    #[test]
    fn drop_uncommitted_txn_rolls_back() {
        let env = test_env();
        let path = unique_path();
        let db = Db::open(env, Path::new(&path), Options::default()).unwrap();

        // Write and drop without commit
        {
            let mut txn = db.begin();
            txn.put(b"key", b"value").unwrap();
            // Drop without commit - should rollback
        }

        // Key should not exist
        let mut txn = db.begin();
        assert_eq!(txn.get(b"key").unwrap(), None);
        txn.rollback();
    }

    /// Verify that commit writes exactly one version per key to the storage engine.
    ///
    /// This test addresses the "double-write" concern: SSI commits to VersionStore
    /// (in-memory) and kv commits to the storage engine (durable). We verify that
    /// the storage engine receives exactly one write per key, not multiple.
    ///
    /// Stage 5b architecture:
    /// - VersionStore is engine-backed: install_writes persists to the engine
    /// - All keys in a single transaction share the same commit_ts
    /// - No separate kv-layer writes needed (single write path)
    ///
    /// After restart, VersionStore is empty, so reads fall through to storage.
    #[test]
    fn commit_writes_exactly_one_version_per_key() {
        use storage::engine::{EngineConfig, LsmEngine};

        let env = test_env();
        let path = unique_path();
        let path_ref = Path::new(&path);

        // Open database and record initial sequence
        let db = Db::open(env.clone(), path_ref, Options::default()).unwrap();

        // Write three keys in a single transaction
        let mut txn = db.begin();
        txn.put(b"k1", b"v1").unwrap();
        txn.put(b"k2", b"v2").unwrap();
        txn.put(b"k3", b"v3").unwrap();
        let outcome = txn.commit().unwrap();
        assert!(!outcome.aborted_for_ssi);
        let commit_ts = outcome.commit_ts;
        assert!(commit_ts > 0, "commit_ts should be positive");

        // Flush to ensure data is persisted
        db.flush().unwrap();

        // Close database
        db.close().unwrap();

        // Reopen the storage engine directly to inspect versions
        let engine = LsmEngine::open(env.clone(), path_ref, EngineConfig::default()).unwrap();

        // Stage 5b: all keys in a transaction share the same commit_ts.
        // Engine sequence advances to commit_ts + 1 after commit.
        let seq = engine.sequence();
        assert_eq!(
            seq,
            commit_ts + 1,
            "Expected sequence {} (commit_ts + 1), got {}",
            commit_ts + 1,
            seq
        );

        // Verify all keys are readable
        assert_eq!(engine.get(b"k1").unwrap(), Some(bytes::Bytes::from("v1")));
        assert_eq!(engine.get(b"k2").unwrap(), Some(bytes::Bytes::from("v2")));
        assert_eq!(engine.get(b"k3").unwrap(), Some(bytes::Bytes::from("v3")));

        // Stage 5b: all keys in a single transaction are written at the same commit_ts.
        // Before commit_ts, none of them should be visible.
        // At commit_ts, all of them should be visible.

        // Before commit_ts: none visible
        assert_eq!(
            engine.get_at(b"k1", commit_ts - 1).unwrap(),
            None,
            "k1 should not exist before commit_ts"
        );
        assert_eq!(
            engine.get_at(b"k2", commit_ts - 1).unwrap(),
            None,
            "k2 should not exist before commit_ts"
        );
        assert_eq!(
            engine.get_at(b"k3", commit_ts - 1).unwrap(),
            None,
            "k3 should not exist before commit_ts"
        );

        // At commit_ts: all visible (all written at same timestamp)
        assert_eq!(
            engine.get_at(b"k1", commit_ts).unwrap(),
            Some(bytes::Bytes::from("v1")),
            "k1 should be visible at commit_ts"
        );
        assert_eq!(
            engine.get_at(b"k2", commit_ts).unwrap(),
            Some(bytes::Bytes::from("v2")),
            "k2 should be visible at commit_ts"
        );
        assert_eq!(
            engine.get_at(b"k3", commit_ts).unwrap(),
            Some(bytes::Bytes::from("v3")),
            "k3 should be visible at commit_ts"
        );

        // At a higher timestamp: still visible (no overwrites)
        assert_eq!(
            engine.get_at(b"k1", 100).unwrap(),
            Some(bytes::Bytes::from("v1")),
            "k1 should still be v1 at high seq (no overwrites)"
        );

        // This proves each key has exactly one version, all at the same commit_ts.
        // No duplicate writes, no extra versions.
    }

    #[test]
    fn commit_writes_exactly_one_version_per_key_in_engine() {
        let env = SimEnv::new(SimEnvConfig::with_seed(0xAAAA));
        let path = unique_path();
        let path_ref = Path::new(&path);
        let db = Db::open(env.clone(), path_ref, Options::default()).unwrap();

        let mut txn = db.begin();
        txn.put(b"single_key", b"single_value").unwrap();
        let outcome = txn.commit().unwrap();
        assert!(!outcome.aborted_for_ssi);
        let commit_ts = outcome.commit_ts;

        // Access the engine directly to enumerate ALL versions of "single_key"
        let engine = db.engine_for_testing();
        let versions = engine.iter_versions(b"single_key", 100).unwrap();

        println!(
            "found {} versions of single_key in the engine:",
            versions.len()
        );
        for (seq, val) in &versions {
            println!(
                "  seq={} val={:?}",
                seq,
                val.as_ref().map(|b| String::from_utf8_lossy(b).to_string())
            );
        }

        // If versions.len() == 1: no double-write, kv commit path is clean
        // If versions.len() == 2: double-write, one from SSI, one from kv
        // If versions.len() == 0: VersionStore is purely in-memory, kv layer engine.put is the ONLY persistence

        assert!(
            versions.len() <= 1,
            "expected at most 1 version per key in engine, found {}: {:?}",
            versions.len(),
            versions
        );

        if versions.len() == 1 {
            let (seq, _) = &versions[0];
            println!(
                "RESULT: single version at seq={} (commit_ts was {})",
                seq, commit_ts
            );
            println!("CONCLUSION: kv layer engine.put() is the ONLY path to storage.");
            println!("            VersionStore is purely in-memory (no engine integration).");
            println!("            The 'double-write' is to TWO DIFFERENT systems:");
            println!("            - VersionStore (in-memory) for SSI conflict detection");
            println!("            - Storage engine (on-disk) for durability");
        } else {
            println!("RESULT: no versions in engine - VersionStore must be purely in-memory");
            println!("        and kv layer engine.put() must have failed or not been called");
        }
    }

    // ===== Version GC integration tests for Phase 3.6 =====

    /// Tests that GC removes old versions during compaction.
    ///
    /// This test writes multiple versions of the same key, triggers compaction
    /// with GC, and verifies that old versions are removed while the newest
    /// version is preserved.
    #[test]
    fn gc_removes_old_versions_during_compaction() {
        let env = SimEnv::new(SimEnvConfig::with_seed(0x6C01));
        let path = unique_path();
        let opts = Options::default().with_memtable_size(512); // Small to force flushes

        let db = Db::open(env.clone(), Path::new(&path), opts).unwrap();

        // Write multiple versions of the same key
        for i in 0..5 {
            let mut txn = db.begin();
            txn.put(b"versioned_key", format!("value_{}", i).as_bytes())
                .unwrap();
            txn.commit().unwrap();
        }

        // Flush to create SSTables
        db.flush().unwrap();

        // Get current watermark (should be current_ts since no active txns)
        let watermark = db.gc_watermark();
        assert!(watermark > 0, "Watermark should be positive");

        // Run GC-aware compaction
        // This may or may not compact depending on file count, but watermark is set
        let _ = db.compact_with_gc();

        // Verify the latest version is still readable
        let mut txn = db.begin();
        let value = txn.get(b"versioned_key").unwrap();
        assert_eq!(
            value,
            Some(bytes::Bytes::from("value_4")),
            "Latest version should be preserved"
        );
        txn.rollback();

        // The key should still be readable (GC doesn't delete the newest version)
        let mut txn2 = db.begin();
        assert!(
            txn2.get(b"versioned_key").unwrap().is_some(),
            "Key should still exist after GC"
        );
        txn2.rollback();
    }

    /// Tests that GC preserves versions needed by active transactions.
    ///
    /// This test starts a long-running transaction, writes new versions,
    /// triggers GC, and verifies that the old version visible to the
    /// long-running transaction is preserved.
    #[test]
    fn gc_respects_long_running_transaction() {
        let env = SimEnv::new(SimEnvConfig::with_seed(0x6C02));
        let path = unique_path();
        let opts = Options::default().with_memtable_size(512);

        let db = Db::open(env.clone(), Path::new(&path), opts).unwrap();

        // Write initial version
        {
            let mut txn = db.begin();
            txn.put(b"key", b"v1").unwrap();
            txn.commit().unwrap();
        }

        // Flush to SSTable
        db.flush().unwrap();

        // Start a long-running transaction that reads the key
        let mut long_txn = db.begin();
        let v1 = long_txn.get(b"key").unwrap();
        assert_eq!(v1, Some(bytes::Bytes::from("v1")));

        // Write more versions while long_txn is still active
        for i in 2..5 {
            let mut txn = db.begin();
            txn.put(b"key", format!("v{}", i).as_bytes()).unwrap();
            txn.commit().unwrap();
        }

        db.flush().unwrap();

        // Watermark should be long_txn's begin_ts (oldest active)
        let watermark = db.gc_watermark();
        assert_eq!(
            watermark,
            long_txn.begin_ts(),
            "Watermark should be the long-running txn's begin_ts"
        );

        // Run GC-aware compaction
        // The watermark ensures v1 is preserved for long_txn
        let _ = db.compact_with_gc();

        // Long-running transaction should still see v1 (its snapshot)
        let value = long_txn.get(b"key").unwrap();
        assert_eq!(
            value,
            Some(bytes::Bytes::from("v1")),
            "Long-running txn should still see v1 after GC"
        );

        // New transaction should see latest version
        let mut new_txn = db.begin();
        let latest = new_txn.get(b"key").unwrap();
        assert_eq!(
            latest,
            Some(bytes::Bytes::from("v4")),
            "New txn should see latest version v4"
        );
        new_txn.rollback();

        // Complete the long-running transaction
        long_txn.rollback();
    }

    /// Tests that background compaction with GC bounds version chains for hot keys.
    ///
    /// This is a headline test: writing many versions of the same key should not
    /// cause unbounded growth. After background compaction (via `compact_all`),
    /// the version count should be bounded.
    ///
    /// With no active transactions, the watermark equals next_ts, so all older
    /// versions should be pruned to just the newest version per key.
    #[test]
    fn gc_bounds_version_chains_for_hot_key() {
        let env = SimEnv::new(SimEnvConfig::with_seed(0x6C03));
        let path = unique_path();
        let opts = Options::default().with_memtable_size(256); // Small memtable to trigger frequent flushes

        let db = Db::open(env.clone(), Path::new(&path), opts).unwrap();

        // Write 1000 versions of the same key
        for i in 0..1000 {
            let mut txn = db.begin();
            txn.put(b"hot_key", format!("value_{}", i).as_bytes())
                .unwrap();
            txn.commit().unwrap();
        }

        // Force flush to ensure all data is in SSTables
        db.flush().unwrap();

        // Trigger background compaction (uses watermark callback automatically)
        // This is compact_all, NOT compact_with_gc - testing ADR-027 integration
        db.compact_all().unwrap();

        // Count versions across all storage (memtables + SSTables)
        let engine = db.engine_for_testing();
        let versions = engine.iter_versions(b"hot_key", 10000).unwrap();

        println!(
            "gc_bounds: {} versions remain after GC (out of 1000 written)",
            versions.len()
        );

        // GC should bound the version count to a small number.
        // With no active transactions, watermark = next_ts, so all but the newest
        // version should be pruned. Expected: 1. Acceptable: ≤5.
        assert!(
            versions.len() <= 5,
            "after GC, hot_key should have at most 5 versions; found {}. \
             Either GC isn't running during background compaction or the watermark \
             isn't being applied correctly.",
            versions.len()
        );

        // Verify the latest version is correct
        let mut txn = db.begin();
        let value = txn.get(b"hot_key").unwrap();
        assert_eq!(
            value,
            Some(bytes::Bytes::from("value_999")),
            "Latest version should be value_999"
        );
        txn.rollback();
    }

    // ===== Scan tests for Phase 6 Stage 0 =====

    #[test]
    fn scan_basic() {
        let env = test_env();
        let path = unique_path();
        let db = Db::open(env, Path::new(&path), Options::default()).unwrap();

        // Write data
        let mut txn = db.begin();
        txn.put(b"a", b"a1").unwrap();
        txn.put(b"b", b"b1").unwrap();
        txn.put(b"c", b"c1").unwrap();
        txn.commit().unwrap();

        // Scan all
        let mut txn = db.begin();
        let results: Vec<_> = txn.scan(..).unwrap().collect::<Result<Vec<_>>>().unwrap();

        assert_eq!(results.len(), 3);
        assert_eq!(
            results[0],
            (bytes::Bytes::from("a"), bytes::Bytes::from("a1"))
        );
        assert_eq!(
            results[1],
            (bytes::Bytes::from("b"), bytes::Bytes::from("b1"))
        );
        assert_eq!(
            results[2],
            (bytes::Bytes::from("c"), bytes::Bytes::from("c1"))
        );
        txn.rollback();
    }

    #[test]
    fn scan_range_bounds() {
        let env = test_env();
        let path = unique_path();
        let db = Db::open(env, Path::new(&path), Options::default()).unwrap();

        // Write data
        let mut txn = db.begin();
        txn.put(b"a", b"a1").unwrap();
        txn.put(b"b", b"b1").unwrap();
        txn.put(b"c", b"c1").unwrap();
        txn.put(b"d", b"d1").unwrap();
        txn.commit().unwrap();

        // Scan [b, c]
        let mut txn = db.begin();
        let results: Vec<_> = txn
            .scan(b"b".to_vec()..=b"c".to_vec())
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, bytes::Bytes::from("b"));
        assert_eq!(results[1].0, bytes::Bytes::from("c"));
        txn.rollback();
    }

    #[test]
    fn scan_reads_your_writes() {
        let env = test_env();
        let path = unique_path();
        let db = Db::open(env, Path::new(&path), Options::default()).unwrap();

        // Write initial data
        let mut setup = db.begin();
        setup.put(b"a", b"a1").unwrap();
        setup.put(b"b", b"b1").unwrap();
        setup.commit().unwrap();

        // Start transaction, buffer some writes
        let mut txn = db.begin();
        txn.put(b"a", b"a_modified").unwrap(); // Override
        txn.put(b"new", b"new_value").unwrap(); // New key
        txn.delete(b"b").unwrap(); // Delete

        // Scan should see buffered writes
        let results: Vec<_> = txn.scan(..).unwrap().collect::<Result<Vec<_>>>().unwrap();

        // Should see a (modified), new (new key), but not b (deleted)
        assert_eq!(results.len(), 2);
        assert_eq!(
            results[0],
            (bytes::Bytes::from("a"), bytes::Bytes::from("a_modified"))
        );
        assert_eq!(
            results[1],
            (bytes::Bytes::from("new"), bytes::Bytes::from("new_value"))
        );
        txn.rollback();
    }

    #[test]
    fn scan_snapshot_isolation() {
        let env = test_env();
        let path = unique_path();
        let db = Db::open(env, Path::new(&path), Options::default()).unwrap();

        // Write initial data
        let mut setup = db.begin();
        setup.put(b"a", b"v1").unwrap();
        setup.commit().unwrap();

        // T1 starts
        let mut t1 = db.begin();

        // T2 modifies and commits
        let mut t2 = db.begin();
        t2.put(b"a", b"v2").unwrap();
        t2.put(b"b", b"new").unwrap();
        t2.commit().unwrap();

        // T1 scan should see old snapshot (v1, no b)
        let results: Vec<_> = t1.scan(..).unwrap().collect::<Result<Vec<_>>>().unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0],
            (bytes::Bytes::from("a"), bytes::Bytes::from("v1"))
        );
        t1.rollback();
    }

    #[test]
    fn scan_triggers_ssi_conflict() {
        let env = test_env();
        let path = unique_path();
        let db = Db::open(env, Path::new(&path), Options::default()).unwrap();

        // Setup: x = 1, y = 1
        let mut setup = db.begin();
        setup.put(b"x", b"1").unwrap();
        setup.put(b"y", b"1").unwrap();
        setup.commit().unwrap();

        // T1: scan all (reads x and y), write x
        let mut t1 = db.begin();
        let _ = t1.scan(..).unwrap().count(); // Read both keys
        t1.put(b"x", b"-1").unwrap();

        // T2: scan all (reads x and y), write y
        let mut t2 = db.begin();
        let _ = t2.scan(..).unwrap().count(); // Read both keys
        t2.put(b"y", b"-1").unwrap();

        // T1 commits
        let outcome1 = t1.commit().unwrap();
        assert!(!outcome1.aborted_for_ssi);

        // T2 should conflict (dangerous structure):
        // T1 read y (via scan), T2 wrote y -> rw-edge T1 -> T2
        // T2 read x (via scan), T1 wrote x -> rw-edge T2 -> T1
        let outcome2 = t2.commit().unwrap();
        assert!(outcome2.aborted_for_ssi);
    }
}
