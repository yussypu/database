//! Sled backend adapter.
//!
//! Wraps sled::Db with the transaction() API. Sled's transactions are serializable.
//!
//! # Transaction Mode
//! - Serializable transactions via transaction() closure API
//! - Optimistic concurrency; conflicts retry automatically inside closure
//! - Our adapter buffers ops and executes in transaction() on commit
//!
//! # Durability
//! - Sled does NOT fsync per commit by default
//! - Writes are batched in memory and flushed periodically by background threads
//! - This makes sled appear significantly faster than sync-per-commit backends
//! - Data may be lost on crash if not yet flushed
//!
//! # TODO (Stage 2)
//! - Consider calling db.flush() after each commit for apples-to-apples durability
//! - Or document that sled numbers reflect "eventual durability" mode
//! - Sled's flush() is expensive; expect ~10-100x slowdown with per-commit flush

use super::{dir_size, Backend, BackendTxn, CommitOutcome, Error, Result};
use std::ops::Bound;
use std::path::Path;
use std::sync::Arc;

/// Sled backend using native transactions.
pub struct SledBackend {
    db: Arc<sled::Db>,
    path: std::path::PathBuf,
}

impl Backend for SledBackend {
    type Txn<'a> = SledTxn<'a>;

    fn open(path: &Path) -> Result<Self> {
        std::fs::create_dir_all(path)?;

        let db = sled::open(path).map_err(|e| Error::Other(e.to_string()))?;

        Ok(Self {
            db: Arc::new(db),
            path: path.to_path_buf(),
        })
    }

    fn begin(&self) -> Result<Self::Txn<'_>> {
        // Sled doesn't have explicit begin; transactions are executed via transaction().
        // We buffer operations and execute them in commit().
        Ok(SledTxn {
            db: &self.db,
            ops: Vec::new(),
            committed: false,
        })
    }

    fn close(self) -> Result<()> {
        // WORKAROUND: sled's Drop impl calls flush(), which deadlocks after
        // sustained concurrent write load (sled issues #1134, #1152). The
        // deadlock occurs in sled::pagecache::iobuf::make_stable_inner,
        // blocking on a condvar that never signals.
        //
        // We intentionally leak the sled::Db to avoid triggering the
        // deadlocking Drop impl. This means:
        // 1. File descriptors remain open until process exit
        // 2. Data may be lost if not yet auto-flushed (sled flushes every 500ms)
        //
        // This is acceptable for benchmarking since each benchmark run is a
        // separate process, and we document that sled numbers reflect
        // batched-durability throughput. See bench/README.md.
        std::mem::forget(self.db);
        Ok(())
    }

    fn disk_size_bytes(&self) -> Result<u64> {
        dir_size(&self.path)
    }
}

/// Operation to buffer before commit.
#[derive(Clone)]
enum SledOp {
    Put(Vec<u8>, Vec<u8>),
    Delete(Vec<u8>),
}

/// Sled transaction wrapper.
///
/// Buffers operations and executes them atomically in commit() via sled's transaction API.
pub struct SledTxn<'a> {
    db: &'a Arc<sled::Db>,
    ops: Vec<SledOp>,
    committed: bool,
}

impl<'a> BackendTxn<'a> for SledTxn<'a> {
    fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        // First check buffered writes
        for op in self.ops.iter().rev() {
            match op {
                SledOp::Put(k, v) if k == key => return Ok(Some(v.clone())),
                SledOp::Delete(k) if k == key => return Ok(None),
                _ => {}
            }
        }

        // Then check the database
        self.db
            .get(key)
            .map(|opt| opt.map(|v| v.to_vec()))
            .map_err(|e| Error::Other(e.to_string()))
    }

    fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        self.ops.push(SledOp::Put(key.to_vec(), value.to_vec()));
        Ok(())
    }

    fn delete(&mut self, key: &[u8]) -> Result<()> {
        self.ops.push(SledOp::Delete(key.to_vec()));
        Ok(())
    }

    fn scan(&mut self, start: Bound<&[u8]>, end: Bound<&[u8]>) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        // Build a map of buffered changes
        let mut buffered: std::collections::BTreeMap<Vec<u8>, Option<Vec<u8>>> =
            std::collections::BTreeMap::new();
        for op in &self.ops {
            match op {
                SledOp::Put(k, v) => {
                    buffered.insert(k.clone(), Some(v.clone()));
                }
                SledOp::Delete(k) => {
                    buffered.insert(k.clone(), None);
                }
            }
        }

        // Scan from database
        let iter = self.db.range::<&[u8], _>(..);

        let mut results: std::collections::BTreeMap<Vec<u8>, Vec<u8>> =
            std::collections::BTreeMap::new();

        // Convert bounds for comparison
        let start_key: Option<Vec<u8>> = match start {
            Bound::Included(k) | Bound::Excluded(k) => Some(k.to_vec()),
            Bound::Unbounded => None,
        };
        let end_key: Option<Vec<u8>> = match end {
            Bound::Included(k) | Bound::Excluded(k) => Some(k.to_vec()),
            Bound::Unbounded => None,
        };

        let in_range = |key: &[u8]| -> bool {
            let after_start = match (&start_key, &start) {
                (Some(sk), Bound::Included(_)) => key >= sk.as_slice(),
                (Some(sk), Bound::Excluded(_)) => key > sk.as_slice(),
                _ => true,
            };
            let before_end = match (&end_key, &end) {
                (Some(ek), Bound::Included(_)) => key <= ek.as_slice(),
                (Some(ek), Bound::Excluded(_)) => key < ek.as_slice(),
                _ => true,
            };
            after_start && before_end
        };

        // Add database entries
        for item in iter {
            let (k, v) = item.map_err(|e| Error::Other(e.to_string()))?;
            let key = k.to_vec();
            if in_range(&key) {
                results.insert(key, v.to_vec());
            }
        }

        // Apply buffered changes
        for (k, v) in buffered {
            if in_range(&k) {
                match v {
                    Some(val) => {
                        results.insert(k, val);
                    }
                    None => {
                        results.remove(&k);
                    }
                }
            }
        }

        Ok(results.into_iter().collect())
    }

    fn commit(mut self) -> Result<CommitOutcome> {
        if self.ops.is_empty() {
            self.committed = true;
            return Ok(CommitOutcome {
                success: true,
                aborted_for_conflict: false,
            });
        }

        let ops = std::mem::take(&mut self.ops);
        let result = self.db.transaction(|tx| {
            for op in &ops {
                match op {
                    SledOp::Put(k, v) => {
                        tx.insert(k.as_slice(), v.as_slice())?;
                    }
                    SledOp::Delete(k) => {
                        tx.remove(k.as_slice())?;
                    }
                }
            }
            Ok(())
        });

        self.committed = true;

        match result {
            Ok(()) => Ok(CommitOutcome {
                success: true,
                aborted_for_conflict: false,
            }),
            Err(sled::transaction::TransactionError::Abort(())) => Ok(CommitOutcome {
                success: false,
                aborted_for_conflict: true,
            }),
            Err(sled::transaction::TransactionError::Storage(e)) => {
                Err(Error::Other(e.to_string()))
            }
        }
    }

    fn rollback(mut self) -> Result<()> {
        self.ops.clear();
        self.committed = true;
        Ok(())
    }
}
