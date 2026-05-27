//! LMDB backend adapter.
//!
//! Wraps lmdb-rkv with read-write transactions. LMDB has one writer at a time,
//! so all writes are trivially serializable.
//!
//! # Transaction Mode
//! - Single-writer serialization (only one write transaction at a time)
//! - No write-write conflicts possible; writers queue behind each other
//! - Readers never block writers; writers never block readers (MVCC via CoW)
//!
//! # Durability
//! - commit() calls msync/fsync by default (MDB_NOSYNC not set)
//! - Every acknowledged commit is durable on disk
//! - Uses memory-mapped I/O; OS page cache provides read performance

use super::{dir_size, Backend, BackendTxn, CommitOutcome, Error, Result};
use lmdb::{Cursor, Database, DatabaseFlags, Environment, Transaction, WriteFlags};
use std::ops::Bound;
use std::path::Path;
use std::sync::Arc;

/// LMDB backend using single-writer transactions.
pub struct LmdbBackend {
    env: Arc<Environment>,
    db: Database,
    path: std::path::PathBuf,
}

// SAFETY: lmdb-rkv's Environment and Database are thread-safe for the operations we use.
// Multiple read transactions can run concurrently; write transactions are serialized by LMDB.
unsafe impl Send for LmdbBackend {}
unsafe impl Sync for LmdbBackend {}

impl Backend for LmdbBackend {
    type Txn<'a> = LmdbTxn<'a>;

    fn open(path: &Path) -> Result<Self> {
        // Create parent directory, LMDB creates data.mdb and lock.mdb inside
        std::fs::create_dir_all(path)?;

        let env = Environment::new()
            .set_map_size(1024 * 1024 * 1024) // 1 GB map size
            .set_max_dbs(1)
            .open(path)
            .map_err(|e| Error::Other(e.to_string()))?;

        let db = env
            .create_db(None, DatabaseFlags::empty())
            .map_err(|e| Error::Other(e.to_string()))?;

        Ok(Self {
            env: Arc::new(env),
            db,
            path: path.to_path_buf(),
        })
    }

    fn begin(&self) -> Result<Self::Txn<'_>> {
        let txn = self
            .env
            .begin_rw_txn()
            .map_err(|e| Error::Other(e.to_string()))?;
        Ok(LmdbTxn {
            txn: Some(txn),
            db: self.db,
        })
    }

    fn close(self) -> Result<()> {
        // Environment drops automatically
        Ok(())
    }

    fn disk_size_bytes(&self) -> Result<u64> {
        dir_size(&self.path)
    }
}

/// LMDB transaction wrapper.
pub struct LmdbTxn<'a> {
    txn: Option<lmdb::RwTransaction<'a>>,
    db: Database,
}

impl<'a> BackendTxn<'a> for LmdbTxn<'a> {
    fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let txn = self
            .txn
            .as_ref()
            .ok_or_else(|| Error::Other("transaction already finished".to_string()))?;

        match txn.get(self.db, &key) {
            Ok(v) => Ok(Some(v.to_vec())),
            Err(lmdb::Error::NotFound) => Ok(None),
            Err(e) => Err(Error::Other(e.to_string())),
        }
    }

    fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        let txn = self
            .txn
            .as_mut()
            .ok_or_else(|| Error::Other("transaction already finished".to_string()))?;
        txn.put(self.db, &key, &value, WriteFlags::empty())
            .map_err(|e| Error::Other(e.to_string()))
    }

    fn delete(&mut self, key: &[u8]) -> Result<()> {
        let txn = self
            .txn
            .as_mut()
            .ok_or_else(|| Error::Other("transaction already finished".to_string()))?;
        match txn.del(self.db, &key, None) {
            Ok(()) => Ok(()),
            Err(lmdb::Error::NotFound) => Ok(()), // Deleting non-existent key is OK
            Err(e) => Err(Error::Other(e.to_string())),
        }
    }

    fn scan(&mut self, start: Bound<&[u8]>, end: Bound<&[u8]>) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let txn = self
            .txn
            .as_ref()
            .ok_or_else(|| Error::Other("transaction already finished".to_string()))?;

        let mut results = Vec::new();
        {
            let mut cursor = txn
                .open_ro_cursor(self.db)
                .map_err(|e| Error::Other(e.to_string()))?;

            // Convert bounds to owned for comparison
            let start_key: Option<Vec<u8>> = match start {
                Bound::Included(k) | Bound::Excluded(k) => Some(k.to_vec()),
                Bound::Unbounded => None,
            };
            let end_key: Option<Vec<u8>> = match end {
                Bound::Included(k) | Bound::Excluded(k) => Some(k.to_vec()),
                Bound::Unbounded => None,
            };

            // Position cursor at start
            let iter = match &start_key {
                Some(k) => cursor.iter_from(k.as_slice()),
                None => cursor.iter_start(),
            };

            for item in iter {
                let (k, v) = item.map_err(|e| Error::Other(e.to_string()))?;
                let key = k.to_vec();

                // Check start bound (iter_from positions at >= key, need to handle Excluded)
                let after_start = match (&start_key, &start) {
                    (Some(sk), Bound::Excluded(_)) => key > *sk,
                    _ => true, // Included or Unbounded already handled by iter_from
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
        }

        Ok(results)
    }

    fn commit(mut self) -> Result<CommitOutcome> {
        let txn = self
            .txn
            .take()
            .ok_or_else(|| Error::Other("transaction already finished".to_string()))?;

        txn.commit().map_err(|e| Error::Other(e.to_string()))?;
        Ok(CommitOutcome {
            success: true,
            aborted_for_conflict: false,
        })
    }

    fn rollback(mut self) -> Result<()> {
        if let Some(txn) = self.txn.take() {
            txn.abort();
        }
        Ok(())
    }
}
