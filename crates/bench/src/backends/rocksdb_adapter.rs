//! RocksDB backend adapter.
//!
//! Wraps rocksdb::TransactionDB with pessimistic transactions (strongest serializable mode).
//!
//! # Transaction Mode
//! - Pessimistic locking via TransactionDB
//! - Write-write conflicts detected at lock acquisition time
//! - Commits may fail with "Resource busy" or "Deadlock" on conflict
//!
//! # Durability
//! - WriteOptions uses default settings (sync = false)
//! - Commits do NOT fsync to disk by default; data is buffered in OS cache
//! - This makes RocksDB appear faster than sync-per-commit backends
//! - For fair comparison, Stage 2 should either enable sync or document this gap
//!
//! # TODO (Stage 2)
//! - Consider setting WriteOptions::set_sync(true) for apples-to-apples durability
//! - Or run benchmarks in two modes: "fast" (no sync) and "durable" (sync)

use super::{dir_size, Backend, BackendTxn, CommitOutcome, Error, Result};
use rocksdb::{
    Options as RocksOptions, TransactionDB, TransactionDBOptions, TransactionOptions, WriteOptions,
};
use std::ops::Bound;
use std::path::Path;
use std::sync::Arc;

/// RocksDB backend using pessimistic transactions.
pub struct RocksdbBackend {
    db: Arc<TransactionDB>,
    path: std::path::PathBuf,
}

impl Backend for RocksdbBackend {
    type Txn<'a> = RocksdbTxn<'a>;

    fn open(path: &Path) -> Result<Self> {
        std::fs::create_dir_all(path)?;

        let mut opts = RocksOptions::default();
        opts.create_if_missing(true);

        let txn_db_opts = TransactionDBOptions::default();

        let db = TransactionDB::open(&opts, &txn_db_opts, path)
            .map_err(|e| Error::Other(e.to_string()))?;

        Ok(Self {
            db: Arc::new(db),
            path: path.to_path_buf(),
        })
    }

    fn begin(&self) -> Result<Self::Txn<'_>> {
        let write_opts = WriteOptions::default();
        let txn_opts = TransactionOptions::default();
        let txn = self.db.transaction_opt(&write_opts, &txn_opts);
        Ok(RocksdbTxn {
            txn: Some(txn),
            db: &self.db,
        })
    }

    fn close(self) -> Result<()> {
        // TransactionDB drops automatically
        Ok(())
    }

    fn disk_size_bytes(&self) -> Result<u64> {
        dir_size(&self.path)
    }
}

/// RocksDB transaction wrapper.
pub struct RocksdbTxn<'a> {
    txn: Option<rocksdb::Transaction<'a, TransactionDB>>,
    #[allow(dead_code)]
    db: &'a Arc<TransactionDB>, // Kept for potential future use
}

impl<'a> BackendTxn<'a> for RocksdbTxn<'a> {
    fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let txn = self
            .txn
            .as_ref()
            .ok_or_else(|| Error::Other("transaction already finished".to_string()))?;
        txn.get(key).map_err(|e| Error::Other(e.to_string()))
    }

    fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        let txn = self
            .txn
            .as_ref()
            .ok_or_else(|| Error::Other("transaction already finished".to_string()))?;
        txn.put(key, value).map_err(|e| Error::Other(e.to_string()))
    }

    fn delete(&mut self, key: &[u8]) -> Result<()> {
        let txn = self
            .txn
            .as_ref()
            .ok_or_else(|| Error::Other("transaction already finished".to_string()))?;
        txn.delete(key).map_err(|e| Error::Other(e.to_string()))
    }

    fn scan(&mut self, start: Bound<&[u8]>, end: Bound<&[u8]>) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let txn = self
            .txn
            .as_ref()
            .ok_or_else(|| Error::Other("transaction already finished".to_string()))?;

        let mut results = Vec::new();
        let iter = txn.iterator(rocksdb::IteratorMode::Start);

        // Convert bounds to owned for comparison
        let start_key: Option<Vec<u8>> = match start {
            Bound::Included(k) | Bound::Excluded(k) => Some(k.to_vec()),
            Bound::Unbounded => None,
        };
        let end_key: Option<Vec<u8>> = match end {
            Bound::Included(k) | Bound::Excluded(k) => Some(k.to_vec()),
            Bound::Unbounded => None,
        };

        for item in iter {
            let (k, v) = item.map_err(|e| Error::Other(e.to_string()))?;
            let key = k.to_vec();

            // Check start bound
            let after_start = match (&start_key, &start) {
                (Some(sk), Bound::Included(_)) => key >= *sk,
                (Some(sk), Bound::Excluded(_)) => key > *sk,
                _ => true,
            };

            // Check end bound
            let before_end = match (&end_key, &end) {
                (Some(ek), Bound::Included(_)) => key <= *ek,
                (Some(ek), Bound::Excluded(_)) => key < *ek,
                _ => true,
            };

            if !after_start {
                continue;
            }
            if !before_end {
                break;
            }

            results.push((key, v.to_vec()));
        }

        Ok(results)
    }

    fn commit(mut self) -> Result<CommitOutcome> {
        let txn = self
            .txn
            .take()
            .ok_or_else(|| Error::Other("transaction already finished".to_string()))?;

        match txn.commit() {
            Ok(()) => Ok(CommitOutcome {
                success: true,
                aborted_for_conflict: false,
            }),
            Err(e) => {
                let msg = e.to_string();
                // Check for write conflict / deadlock
                if msg.contains("Resource busy")
                    || msg.contains("Deadlock")
                    || msg.contains("write conflict")
                {
                    Ok(CommitOutcome {
                        success: false,
                        aborted_for_conflict: true,
                    })
                } else {
                    Err(Error::Other(msg))
                }
            }
        }
    }

    fn rollback(mut self) -> Result<()> {
        if let Some(txn) = self.txn.take() {
            txn.rollback().map_err(|e| Error::Other(e.to_string()))?;
        }
        Ok(())
    }
}
