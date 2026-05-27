//! CrackedDB backend adapter.
//!
//! Wraps kv::Db with SSI (strongest serializable mode, which is the default).
//!
//! # Transaction Mode
//! - Serializable Snapshot Isolation (SSI) with rw-antidependency tracking
//! - Transactions may abort on commit if dangerous structures are detected
//!
//! # Durability
//! - Commits fsync the WAL before returning (wal_sync mode)
//! - Every acknowledged commit is durable on disk
//! - This is the strictest durability guarantee among the tested backends

use super::{dir_size, Backend, BackendTxn, CommitOutcome, Error, Result};
use kv::{Db, Options};
use runtime::{Path as RuntimePath, RealEnv};
use std::ops::Bound;
use std::path::Path;

/// CrackedDB backend using SSI transactions.
pub struct CrackeddbBackend {
    db: Db<RealEnv>,
    path: std::path::PathBuf,
}

impl Backend for CrackeddbBackend {
    type Txn<'a> = CrackeddbTxn<'a>;

    fn open(path: &Path) -> Result<Self> {
        // Create directory if it doesn't exist
        std::fs::create_dir_all(path)?;

        let env = RealEnv::new();
        let runtime_path = RuntimePath::new(
            path.to_str()
                .ok_or_else(|| Error::Other("invalid path".to_string()))?,
        );

        let db = Db::open(env, runtime_path, Options::default())
            .map_err(|e| Error::Other(e.to_string()))?;

        Ok(Self {
            db,
            path: path.to_path_buf(),
        })
    }

    fn begin(&self) -> Result<Self::Txn<'_>> {
        let txn = self.db.begin();
        Ok(CrackeddbTxn {
            txn: Some(txn),
            _marker: std::marker::PhantomData,
        })
    }

    fn close(self) -> Result<()> {
        // Db drops automatically
        Ok(())
    }

    fn disk_size_bytes(&self) -> Result<u64> {
        dir_size(&self.path)
    }
}

/// CrackedDB transaction wrapper.
pub struct CrackeddbTxn<'a> {
    txn: Option<kv::Txn<RealEnv>>,
    _marker: std::marker::PhantomData<&'a ()>,
}

impl<'a> BackendTxn<'a> for CrackeddbTxn<'a> {
    fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let txn = self
            .txn
            .as_mut()
            .ok_or_else(|| Error::Other("transaction already finished".to_string()))?;
        txn.get(key)
            .map(|opt| opt.map(|b| b.to_vec()))
            .map_err(|e| Error::Other(e.to_string()))
    }

    fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        let txn = self
            .txn
            .as_mut()
            .ok_or_else(|| Error::Other("transaction already finished".to_string()))?;
        txn.put(key, value).map_err(|e| Error::Other(e.to_string()))
    }

    fn delete(&mut self, key: &[u8]) -> Result<()> {
        let txn = self
            .txn
            .as_mut()
            .ok_or_else(|| Error::Other("transaction already finished".to_string()))?;
        txn.delete(key).map_err(|e| Error::Other(e.to_string()))
    }

    fn scan(&mut self, start: Bound<&[u8]>, end: Bound<&[u8]>) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let txn = self
            .txn
            .as_mut()
            .ok_or_else(|| Error::Other("transaction already finished".to_string()))?;

        // Convert bounds to Vec<u8> for kv::Txn::scan
        let start_bound: Bound<Vec<u8>> = match start {
            Bound::Included(k) => Bound::Included(k.to_vec()),
            Bound::Excluded(k) => Bound::Excluded(k.to_vec()),
            Bound::Unbounded => Bound::Unbounded,
        };
        let end_bound: Bound<Vec<u8>> = match end {
            Bound::Included(k) => Bound::Included(k.to_vec()),
            Bound::Excluded(k) => Bound::Excluded(k.to_vec()),
            Bound::Unbounded => Bound::Unbounded,
        };

        let scan = txn
            .scan((start_bound, end_bound))
            .map_err(|e| Error::Other(e.to_string()))?;

        let mut results = Vec::new();
        for entry in scan {
            let (k, v) = entry.map_err(|e| Error::Other(e.to_string()))?;
            results.push((k.to_vec(), v.to_vec()));
        }
        Ok(results)
    }

    fn commit(mut self) -> Result<CommitOutcome> {
        let txn = self
            .txn
            .take()
            .ok_or_else(|| Error::Other("transaction already finished".to_string()))?;
        let outcome = txn.commit().map_err(|e| Error::Other(e.to_string()))?;
        Ok(CommitOutcome {
            success: !outcome.aborted_for_ssi,
            aborted_for_conflict: outcome.aborted_for_ssi,
        })
    }

    fn rollback(mut self) -> Result<()> {
        if let Some(txn) = self.txn.take() {
            txn.rollback();
        }
        Ok(())
    }
}
