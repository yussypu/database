//! Database handle and options.
//!
//! # Design Decisions (ADR-022)
//!
//! - `Db` is `Clone` via `Arc<DbInner>`. Cloning is cheap (one Arc bump).
//! - `Db::open` takes an explicit `Env`. Deterministic simulation is a first-class feature.
//! - Multiple `Db` handles can exist concurrently; each can produce its own transactions.

use crate::error::{Error, Result};
use crate::txn::Txn;
use mvcc::{SSITransactionManager, VersionStore};
use parking_lot::Mutex;
use runtime::{Env, Path, PathBuf};
use std::collections::BTreeSet;
use std::sync::Arc;
use storage::engine::{EngineConfig, LsmEngine};

/// Global registry of open database paths to prevent double-open.
/// Uses BTreeSet for deterministic iteration order (required for simulation).
static OPEN_PATHS: Mutex<Option<BTreeSet<PathBuf>>> = Mutex::new(None);

fn register_path(path: &Path) -> Result<()> {
    let mut guard = OPEN_PATHS.lock();
    let paths = guard.get_or_insert_with(BTreeSet::new);
    let canonical = path.to_path_buf();
    if paths.contains(&canonical) {
        return Err(Error::AlreadyOpen);
    }
    paths.insert(canonical);
    Ok(())
}

fn unregister_path(path: &Path) {
    let mut guard = OPEN_PATHS.lock();
    if let Some(paths) = guard.as_mut() {
        paths.remove(&path.to_path_buf());
    }
}

/// Configuration options for opening a database.
///
/// Uses the builder pattern. Marked `#[non_exhaustive]` so fields can be added
/// without breaking semver.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Options {
    /// Block size for SSTables.
    pub block_size: usize,

    /// WAL segment size in bytes.
    pub wal_segment_size: u64,

    /// Maximum memtable size before flush.
    pub memtable_size: usize,

    /// Whether to use PGM learned indexes.
    pub use_pgm_index: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            block_size: 4096,
            wal_segment_size: 64 * 1024 * 1024, // 64 MB
            memtable_size: 4 * 1024 * 1024,     // 4 MB
            use_pgm_index: true,
        }
    }
}

impl Options {
    /// Set the block size for SSTables.
    #[must_use]
    pub fn with_block_size(mut self, size: usize) -> Self {
        self.block_size = size;
        self
    }

    /// Set the WAL segment size.
    #[must_use]
    pub fn with_wal_segment_size(mut self, size: u64) -> Self {
        self.wal_segment_size = size;
        self
    }

    /// Set the memtable size.
    #[must_use]
    pub fn with_memtable_size(mut self, size: usize) -> Self {
        self.memtable_size = size;
        self
    }

    /// Enable or disable PGM learned indexes.
    #[must_use]
    pub fn with_use_pgm_index(mut self, enabled: bool) -> Self {
        self.use_pgm_index = enabled;
        self
    }
}

/// Internal database state shared across all `Db` handles.
pub(crate) struct DbInner<E: Env + Clone + 'static> {
    pub(crate) path: PathBuf,
    pub(crate) engine: Arc<LsmEngine<E>>,
    pub(crate) ssi_manager: Arc<SSITransactionManager<E>>,
}

impl<E: Env + Clone> Drop for DbInner<E> {
    fn drop(&mut self) {
        unregister_path(&self.path);
    }
}

/// A handle to an open database.
///
/// `Db` is cheap to clone (Arc bump). Each clone can independently create
/// transactions. There is one underlying engine and one SSI transaction manager
/// per opened path.
///
/// # Example
///
/// ```ignore
/// use kv::{Db, Options};
/// use runtime::RealEnv;
///
/// let env = RealEnv::new();
/// let db = Db::open(env, "/path/to/db", Options::default())?;
///
/// let mut txn = db.begin();
/// txn.put(b"key", b"value")?;
/// let outcome = txn.commit()?;
/// if outcome.aborted_for_ssi {
///     // Retry the transaction
/// }
/// ```
pub struct Db<E: Env + Clone> {
    inner: Arc<DbInner<E>>,
}

impl<E: Env + Clone> Clone for Db<E> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<E: Env + Clone> Db<E> {
    /// Opens or creates a database at the given path.
    ///
    /// The `Env` parameter is explicit because deterministic simulation is a
    /// first-class feature of this database. Users provide `RealEnv::new()` for
    /// production or `SimEnv::new(config)` for testing.
    ///
    /// # Errors
    ///
    /// - `Error::AlreadyOpen` if a database at this path is already open.
    /// - `Error::Io` on I/O failure.
    /// - `Error::Corruption` if existing data is corrupted.
    pub fn open(env: E, path: &Path, options: Options) -> Result<Self> {
        register_path(path)?;

        let engine_config = EngineConfig {
            memtable_size: options.memtable_size,
            wal_segment_size: options.wal_segment_size,
            ..EngineConfig::default()
        };

        let engine = match LsmEngine::open(env, path, engine_config) {
            Ok(e) => Arc::new(e),
            Err(e) => {
                unregister_path(path);
                return Err(e.into());
            }
        };

        // Create engine-backed VersionStore (Stage 5b).
        // The VersionStore now routes all MVCC operations through the engine.
        // SSI timestamps are synchronized from recovery values automatically.
        let version_store = Arc::new(VersionStore::new(Arc::clone(&engine)));
        let ssi_manager = Arc::new(SSITransactionManager::new(version_store));

        // Register watermark source for GC during background compaction.
        // This allows the engine to get the SSI watermark without importing mvcc types.
        let ssi_clone = Arc::clone(&ssi_manager);
        engine.set_watermark_source(Arc::new(move || ssi_clone.min_active_begin_ts()));

        let inner = DbInner {
            path: path.to_path_buf(),
            engine,
            ssi_manager,
        };

        Ok(Self {
            inner: Arc::new(inner),
        })
    }

    /// Begins a new transaction.
    ///
    /// The returned `Txn` is `Send` but not `Sync`: it can move between threads
    /// but cannot be shared. Each transaction sees a consistent snapshot of the
    /// database as of its begin timestamp.
    pub fn begin(&self) -> Txn<E> {
        let ssi_txn = self.inner.ssi_manager.begin();
        Txn::new(Arc::clone(&self.inner), ssi_txn)
    }

    /// Returns the path where this database is stored.
    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    /// Forces all buffered data to disk.
    ///
    /// This is usually not needed since commits already sync the WAL.
    pub fn flush(&self) -> Result<()> {
        self.inner.engine.flush()?;
        Ok(())
    }

    /// Closes the database, releasing resources.
    ///
    /// This is optional; the database will close when the last `Db` handle is
    /// dropped. Calling `close` explicitly ensures the path is unregistered
    /// immediately.
    ///
    /// # Errors
    ///
    /// Returns `Error::AlreadyOpen` if other `Db` handles still exist (Arc
    /// strong count > 1).
    pub fn close(self) -> Result<()> {
        match Arc::try_unwrap(self.inner) {
            Ok(_inner) => {
                // DbInner::drop unregisters the path
                Ok(())
            }
            Err(_arc) => Err(Error::AlreadyOpen),
        }
    }

    /// Test-only accessor for the underlying storage engine.
    #[cfg(test)]
    pub(crate) fn engine_for_testing(&self) -> &LsmEngine<E> {
        &self.inner.engine
    }

    /// Runs compaction with version garbage collection.
    ///
    /// Per ADR-027, this performs compaction while applying GC rules:
    /// - Keep all versions needed by active transactions
    /// - Keep the newest version for new transactions
    /// - Discard older versions that are no longer needed
    ///
    /// The watermark is computed from the SSI transaction manager's
    /// `min_active_begin_ts()` to ensure active snapshots are preserved.
    ///
    /// Returns `true` if compaction was performed, `false` if no compaction
    /// was needed.
    pub fn compact_with_gc(&self) -> Result<bool> {
        let watermark = self.inner.ssi_manager.min_active_begin_ts();
        Ok(self.inner.engine.compact_with_gc(|| watermark)?)
    }

    /// Returns the current GC watermark.
    ///
    /// This is the minimum begin_ts among all active transactions, or the
    /// current timestamp if no transactions are active. Versions with
    /// commit_ts <= watermark (except the newest per key) can be garbage
    /// collected.
    pub fn gc_watermark(&self) -> u64 {
        self.inner.ssi_manager.min_active_begin_ts()
    }

    /// Runs compaction until no more compaction is needed.
    ///
    /// This uses the background compaction path which automatically applies
    /// GC via the registered watermark source. Useful for tests and benchmarks
    /// that need a fully compacted state.
    pub fn compact_all(&self) -> Result<()> {
        Ok(self.inner.engine.compact_all()?)
    }
}
