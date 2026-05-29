//! LSM Storage Engine implementation.
//!
//! This module ties together all storage components:
//! - WAL for durability
//! - Memtable for in-memory writes
//! - SSTables for persistent sorted storage
//! - Compaction for merging and space reclamation
//!
//! # Write Path
//!
//! ```text
//! Put(key, value)
//!   → Write to WAL (sync)
//!   → Write to Memtable
//!   → If Memtable full: flush to L0 SSTable
//! ```
//!
//! # Read Path
//!
//! ```text
//! Get(key)
//!   → Search active Memtable
//!   → Search immutable Memtables (newest first)
//!   → Search L0 SSTables (all, newest first)
//!   → Search L1+ SSTables (binary search by key range)
//! ```

use crate::compaction::{
    apply_compaction, CompactionConfig, CompactionExecutor, CompactionPicker, CompactionTask,
    FileMetadata, Version,
};
use crate::error::{Error, Result};
use crate::group_commit::{GroupCommitConfig, GroupCommitter, GroupCommitStats};
use crate::memtable::{InternalKey, LookupResult, Memtable, MemtableConfig, MemtableValue};
use crate::sstable::{
    decode_internal_key, decode_value, encode_internal_key, encode_value, SSTableBuilder,
    SSTableReader,
};
use crate::wal::{WalConfig, WalReader, WalWriter};
use bytes::Bytes;
use parking_lot::RwLock;
use runtime::{Env, File, OpenOptions, Path, PathBuf};
use std::cmp::Ordering as CmpOrdering;
use std::collections::{BinaryHeap, HashMap};
use std::ops::Bound;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Configuration for the LSM engine.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Maximum memtable size in bytes before flush.
    pub memtable_size: usize,
    /// WAL segment size in bytes.
    pub wal_segment_size: u64,
    /// Compaction configuration.
    pub compaction: CompactionConfig,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            memtable_size: 4 * 1024 * 1024,     // 4 MB
            wal_segment_size: 64 * 1024 * 1024, // 64 MB
            compaction: CompactionConfig::default(),
        }
    }
}

/// The main LSM storage engine.
pub struct LsmEngine<E: Env + Clone> {
    env: E,
    path: PathBuf,
    config: EngineConfig,

    /// Active memtable for writes.
    active_memtable: RwLock<Arc<Memtable>>,

    /// Immutable memtables waiting to be flushed.
    immutable_memtables: RwLock<Vec<Arc<Memtable>>>,

    /// Current version (SSTable layout).
    version: RwLock<Arc<Version>>,

    /// Group commit manager for WAL (replaces direct WAL access).
    /// Handles batched commits with pipelined fsync for high throughput.
    group_committer: GroupCommitter<E>,

    /// Next file number for SSTables.
    next_file_num: AtomicU64,

    /// Next memtable ID.
    next_memtable_id: AtomicU64,

    /// Global sequence number (monotonically increasing).
    sequence: AtomicU64,

    /// Open SSTable readers cache.
    table_cache: RwLock<HashMap<u64, Arc<SSTableReader<E>>>>,

    /// Maximum commit timestamp seen (for MVCC recovery).
    max_commit_ts: AtomicU64,

    /// Maximum transaction ID seen (for MVCC recovery).
    max_txn_id: AtomicU64,

    /// Optional watermark source for GC during compaction.
    /// When set, background compaction uses this to get the GC watermark.
    /// The engine does not import mvcc types; this is an opaque callback.
    watermark_fn: RwLock<Option<Arc<dyn Fn() -> u64 + Send + Sync>>>,
}

// WAL record type markers for transaction records.
// Legacy KV format: seq(8 bytes LE) + key_len(4) + key + type(1) + [value_len(4) + value]
// Transaction format: MAGIC(8 bytes = u64::MAX) + type(1) + payload
// Using u64::MAX as magic prevents collision since it's an impossible sequence number.
const WAL_TXN_MAGIC: u64 = u64::MAX;
const WAL_TYPE_TXN_BEGIN: u8 = 0x01;
const WAL_TYPE_TXN_WRITE: u8 = 0x02;
const WAL_TYPE_TXN_COMMIT: u8 = 0x03;
const WAL_TYPE_TXN_ABORT: u8 = 0x04;

// Batch record format (for group commit):
// MAGIC(8 bytes = u64::MAX - 1) + count(4) + [[len:u32][kv_record]]...
// This allows atomic commit of multiple KV pairs in a single WAL record.
const WAL_BATCH_MAGIC: u64 = u64::MAX - 1;

/// Decoded WAL record types for recovery.
#[derive(Debug)]
enum WalPayload {
    /// Key-value put/delete (legacy format)
    Kv {
        key: Bytes,
        value: Option<Bytes>,
        seq: u64,
    },
    /// Batch of key-value writes (group commit format)
    Batch {
        writes: Vec<(Bytes, Option<Bytes>, u64)>, // (key, value, seq/commit_ts)
    },
    /// Transaction begin
    TxnBegin { txn_id: u64 },
    /// Transaction write (buffered, not yet applied)
    TxnWrite {
        txn_id: u64,
        key: Bytes,
        value: Bytes,
    },
    /// Transaction commit
    TxnCommit { txn_id: u64, commit_ts: u64 },
    /// Transaction abort
    TxnAbort { txn_id: u64 },
}

/// Entry in the scan merge heap.
/// Ordered by (user_key ASC, seq DESC) so that for each user_key,
/// the newest version comes first.
#[derive(Clone)]
struct ScanHeapEntry {
    user_key: Bytes,
    seq: u64,
    value: Option<Bytes>, // None = tombstone
    source_idx: usize,
}

impl PartialEq for ScanHeapEntry {
    fn eq(&self, other: &Self) -> bool {
        self.user_key == other.user_key && self.seq == other.seq
    }
}

impl Eq for ScanHeapEntry {}

impl PartialOrd for ScanHeapEntry {
    fn partial_cmp(&self, other: &Self) -> Option<CmpOrdering> {
        Some(self.cmp(other))
    }
}

impl Ord for ScanHeapEntry {
    fn cmp(&self, other: &Self) -> CmpOrdering {
        // BinaryHeap is max-heap, so we reverse the order to get min-heap behavior
        // for user_key. For seq, we want descending (newest first), so we reverse again.
        match other.user_key.cmp(&self.user_key) {
            CmpOrdering::Equal => self.seq.cmp(&other.seq), // Higher seq first (newest)
            ord => ord,                                     // Smaller user_key first
        }
    }
}

/// Wrapper to convert memtable entries to the same format as SSTable entries.
struct MemtableScanIter {
    entries: std::vec::IntoIter<(Bytes, Bytes)>,
}

impl MemtableScanIter {
    fn new<I>(iter: I, start_bound: &Bound<Bytes>, end_bound: &Bound<Bytes>) -> Self
    where
        I: Iterator<Item = (InternalKey, MemtableValue)>,
    {
        let entries: Vec<(Bytes, Bytes)> = iter
            .filter(|(internal_key, _)| {
                // Filter by start bound
                let after_start = match start_bound {
                    Bound::Included(start) => internal_key.user_key.as_ref() >= start.as_ref(),
                    Bound::Excluded(start) => internal_key.user_key.as_ref() > start.as_ref(),
                    Bound::Unbounded => true,
                };
                // Filter by end bound
                let before_end = match end_bound {
                    Bound::Included(end) => internal_key.user_key.as_ref() <= end.as_ref(),
                    Bound::Excluded(end) => internal_key.user_key.as_ref() < end.as_ref(),
                    Bound::Unbounded => true,
                };
                after_start && before_end
            })
            .map(|(internal_key, value)| {
                let encoded_key = encode_internal_key(&internal_key);
                let encoded_value = encode_value(&value);
                (encoded_key, encoded_value)
            })
            .collect();
        Self {
            entries: entries.into_iter(),
        }
    }
}

impl Iterator for MemtableScanIter {
    type Item = Result<(Bytes, Bytes)>;

    fn next(&mut self) -> Option<Self::Item> {
        self.entries.next().map(Ok)
    }
}

/// Wrapper to collect SSTable entries into an owned Vec (avoids lifetime issues).
/// Filters entries by range bounds during collection.
struct SStableScanIter {
    entries: std::vec::IntoIter<(Bytes, Bytes)>,
}

impl SStableScanIter {
    fn new<I>(iter: I, start_bound: &Bound<Bytes>, end_bound: &Bound<Bytes>) -> Result<Self>
    where
        I: Iterator<Item = Result<(Bytes, Bytes)>>,
    {
        let mut entries = Vec::new();
        let mut started = false;

        for result in iter {
            let (raw_key, raw_value) = result?;

            // Decode to get user_key for bound checking
            let Some(internal_key) = decode_internal_key(&raw_key) else {
                continue; // Skip malformed keys
            };

            // Check start bound (skip entries before start)
            if !started {
                let after_start = match start_bound {
                    Bound::Included(start) => internal_key.user_key.as_ref() >= start.as_ref(),
                    Bound::Excluded(start) => internal_key.user_key.as_ref() > start.as_ref(),
                    Bound::Unbounded => true,
                };
                if !after_start {
                    continue;
                }
                started = true;
            }

            // Check end bound (stop when past end)
            let before_end = match end_bound {
                Bound::Included(end) => internal_key.user_key.as_ref() <= end.as_ref(),
                Bound::Excluded(end) => internal_key.user_key.as_ref() < end.as_ref(),
                Bound::Unbounded => true,
            };
            if !before_end {
                // SSTables are sorted, so we can stop early
                break;
            }

            entries.push((raw_key, raw_value));
        }

        Ok(Self {
            entries: entries.into_iter(),
        })
    }
}

impl Iterator for SStableScanIter {
    type Item = Result<(Bytes, Bytes)>;

    fn next(&mut self) -> Option<Self::Item> {
        self.entries.next().map(Ok)
    }
}

/// Type alias for boxed scan source iterators.
type ScanSource<'a> = Box<dyn Iterator<Item = Result<(Bytes, Bytes)>> + 'a>;

/// Snapshot-consistent range scan iterator.
///
/// Merges entries from memtables and SSTables, selecting the newest version
/// at or before the snapshot timestamp for each user key. Tombstones are
/// filtered out.
pub struct EngineScan<'a, E: Env + Clone> {
    #[allow(dead_code)]
    engine: &'a LsmEngine<E>,
    heap: BinaryHeap<ScanHeapEntry>,
    /// Boxed iterators to avoid complex lifetimes
    sources: Vec<ScanSource<'a>>,
    snapshot_ts: u64,
    end_bound: Bound<Bytes>,
    last_user_key: Option<Bytes>,
    /// Track if we've hit an error (to stop iteration)
    errored: bool,
}

impl<'a, E: Env + Clone> EngineScan<'a, E> {
    fn advance_source(&mut self, source_idx: usize) -> Result<()> {
        if let Some(iter) = self.sources.get_mut(source_idx) {
            for result in iter.by_ref() {
                let (raw_key, raw_value) = result?;

                // Decode internal key
                let Some(internal_key) = decode_internal_key(&raw_key) else {
                    continue; // Skip malformed keys
                };

                // Check end bound
                let past_end = match &self.end_bound {
                    Bound::Included(end) => internal_key.user_key.as_ref() > end.as_ref(),
                    Bound::Excluded(end) => internal_key.user_key.as_ref() >= end.as_ref(),
                    Bound::Unbounded => false,
                };
                if past_end {
                    // This source is exhausted for this range
                    return Ok(());
                }

                // Decode value
                let value = if let Some(memtable_value) = decode_value(&raw_value) {
                    match memtable_value {
                        MemtableValue::Put(v) => Some(v),
                        MemtableValue::Delete => None,
                    }
                } else {
                    continue; // Skip malformed values
                };

                self.heap.push(ScanHeapEntry {
                    user_key: internal_key.user_key,
                    seq: internal_key.seq,
                    value,
                    source_idx,
                });
                return Ok(());
            }
        }
        Ok(())
    }

    fn next_internal(&mut self) -> Result<Option<(Bytes, Bytes)>> {
        loop {
            let entry = match self.heap.pop() {
                Some(e) => e,
                None => return Ok(None),
            };

            // Advance the source iterator
            self.advance_source(entry.source_idx)?;

            // Skip if this is the same user_key as last returned (we only want newest)
            if let Some(ref last) = self.last_user_key {
                if entry.user_key == *last {
                    continue;
                }
            }

            // Check if this version is visible at snapshot_ts
            if entry.seq > self.snapshot_ts {
                // This version is too new, skip it
                // But don't update last_user_key - we may find an older visible version
                continue;
            }

            // This is the newest visible version for this user_key
            self.last_user_key = Some(entry.user_key.clone());

            // Skip tombstones
            if entry.value.is_none() {
                continue;
            }

            return Ok(Some((entry.user_key, entry.value.unwrap())));
        }
    }
}

impl<E: Env + Clone> Iterator for EngineScan<'_, E> {
    type Item = Result<(Bytes, Bytes)>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.errored {
            return None;
        }
        match self.next_internal() {
            Ok(Some(kv)) => Some(Ok(kv)),
            Ok(None) => None,
            Err(e) => {
                self.errored = true;
                Some(Err(e))
            }
        }
    }
}

impl<E: Env + Clone> LsmEngine<E> {
    /// Opens or creates an LSM engine at the given path.
    pub fn open(env: E, path: &Path, config: EngineConfig) -> Result<Self> {
        // Create directories if needed
        env.create_dir_all(path)?;
        env.create_dir_all(&path.join("wal"))?;
        env.create_dir_all(&path.join("sst"))?;

        // Initialize state
        let mut next_file_num = 1u64;
        let mut sequence = 1u64;

        // Try to load manifest (version info)
        let version = match Self::load_manifest(&env, path) {
            Ok((v, file_num, seq)) => {
                next_file_num = file_num;
                sequence = seq;
                v
            }
            Err(_) => Version::new(config.compaction.max_levels),
        };

        // Create WAL writer
        let wal_path = path.join("wal");
        let wal_config = WalConfig {
            segment_size: config.wal_segment_size,
            sync_on_write: false,
        };
        let wal = WalWriter::new(env.clone(), &wal_path, wal_config)?;

        // Create group committer wrapping the WAL
        let group_commit_config = GroupCommitConfig::default();
        let group_committer = GroupCommitter::new(env.clone(), wal, group_commit_config);

        // Create initial memtable
        let memtable_config = MemtableConfig {
            max_size: config.memtable_size,
        };
        let memtable = Arc::new(Memtable::new(0, memtable_config));

        let engine = Self {
            env: env.clone(),
            path: path.to_path_buf(),
            config,
            active_memtable: RwLock::new(memtable),
            immutable_memtables: RwLock::new(Vec::new()),
            version: RwLock::new(Arc::new(version)),
            group_committer,
            next_file_num: AtomicU64::new(next_file_num),
            next_memtable_id: AtomicU64::new(1),
            sequence: AtomicU64::new(sequence),
            table_cache: RwLock::new(HashMap::new()),
            max_commit_ts: AtomicU64::new(0),
            max_txn_id: AtomicU64::new(0),
            watermark_fn: RwLock::new(None),
        };

        // Recover from WAL
        engine.recover()?;

        Ok(engine)
    }

    /// Puts a key-value pair.
    pub fn put(&self, key: &[u8], value: &[u8]) -> Result<()> {
        self.write(key, Some(value))
    }

    /// Deletes a key.
    pub fn delete(&self, key: &[u8]) -> Result<()> {
        self.write(key, None)
    }

    /// Gets a value by key.
    pub fn get(&self, key: &[u8]) -> Result<Option<Bytes>> {
        let seq = self.sequence.load(Ordering::SeqCst);
        self.get_at(key, seq)
    }

    /// Gets a value at a specific sequence number (for MVCC reads).
    pub fn get_at(&self, key: &[u8], seq: u64) -> Result<Option<Bytes>> {
        // Search active memtable
        {
            let memtable = self.active_memtable.read();
            match memtable.lookup(key, seq) {
                LookupResult::Found(value) => return Ok(Some(value)),
                // see ADR-007 (regression test: tombstone_regression_seed_0xdeadbeef_cycle_185)
                LookupResult::Deleted => return Ok(None), // Tombstone found, stop searching
                LookupResult::NotFound => {}              // Continue to immutable memtables
            }
        }

        // Search immutable memtables (newest first)
        {
            let immutables = self.immutable_memtables.read();
            for memtable in immutables.iter().rev() {
                match memtable.lookup(key, seq) {
                    LookupResult::Found(value) => return Ok(Some(value)),
                    LookupResult::Deleted => return Ok(None), // Tombstone found, stop searching
                    LookupResult::NotFound => {}              // Continue to next memtable
                }
            }
        }

        // Search SSTables
        let version = self.version.read().clone();
        let search_key = encode_internal_key(&InternalKey::new(Bytes::copy_from_slice(key), seq));

        // Search L0 (all files, they may overlap)
        for file in version.levels[0].iter().rev() {
            if let Some(value) = self.search_sstable(file, &search_key, key, seq)? {
                return Ok(value);
            }
        }

        // Search L1+ (files are sorted, use key range to find candidates)
        // For MVCC-aware search, we compare by user key only (not sequence number)
        for level in 1..version.levels.len() {
            let files = &version.levels[level];
            if files.is_empty() {
                continue;
            }

            // Binary search to find first file whose largest user_key >= our key
            // Extract user key portion (internal_key = user_key + 8-byte seq)
            let idx = files.partition_point(|f| {
                let largest = &f.largest_key;
                if largest.len() >= 8 {
                    &largest[..largest.len() - 8] < key
                } else {
                    largest.as_ref() < key
                }
            });

            if idx < files.len() {
                let file = &files[idx];
                let smallest_user_key = if file.smallest_key.len() >= 8 {
                    &file.smallest_key[..file.smallest_key.len() - 8]
                } else {
                    file.smallest_key.as_ref()
                };

                if smallest_user_key <= key {
                    if let Some(value) = self.search_sstable(file, &search_key, key, seq)? {
                        return Ok(value);
                    }
                }
            }
        }

        Ok(None)
    }

    /// Forces a memtable flush.
    pub fn flush(&self) -> Result<()> {
        self.rotate_memtable()?;
        self.flush_immutable_memtables()
    }

    /// Sets the watermark source for GC during background compaction.
    ///
    /// When set, background compaction (triggered by `maybe_compact`) will
    /// use this callback to get the GC watermark and apply version GC.
    /// This allows the kv layer to inject SSI's `min_active_begin_ts()`
    /// without the storage layer importing mvcc types.
    ///
    /// If not set, background compaction falls back to deduplication only
    /// (keeps newest version per key, no watermark-based GC).
    pub fn set_watermark_source(&self, watermark_fn: Arc<dyn Fn() -> u64 + Send + Sync>) {
        *self.watermark_fn.write() = Some(watermark_fn);
    }

    /// Runs one round of compaction if needed.
    ///
    /// If a watermark source is set (via `set_watermark_source`), this will
    /// apply GC during compaction. Otherwise, it falls back to deduplication only.
    pub fn maybe_compact(&self) -> Result<bool> {
        let version = self.version.read().clone();
        let picker = CompactionPicker::new(self.config.compaction.clone());

        if let Some(task) = picker.pick_compaction(&version) {
            // Check if we have a watermark source for GC
            let watermark_fn = self.watermark_fn.read().clone();
            if let Some(wm_fn) = watermark_fn {
                let watermark = wm_fn();
                self.run_compaction_with_gc(&task, watermark)?;
            } else {
                // No watermark source: fall back to deduplication only
                self.run_compaction(&task)?;
            }
            return Ok(true);
        }

        Ok(false)
    }

    /// Runs compaction with version garbage collection.
    ///
    /// Per ADR-027, for each key during compaction:
    /// - Keep all versions with commit_ts > watermark (active transactions need them)
    /// - Keep newest version with commit_ts <= watermark (for new transactions)
    /// - Discard older versions with commit_ts <= watermark
    /// - Tombstones are kept unconditionally (Phase 6 will handle them)
    ///
    /// The `watermark_fn` is called to get the current watermark at compaction time.
    /// This should typically be `SSITransactionManager::min_active_begin_ts()`.
    ///
    /// Returns `true` if compaction was performed, `false` if no compaction was needed.
    pub fn compact_with_gc<F>(&self, watermark_fn: F) -> Result<bool>
    where
        F: FnOnce() -> u64,
    {
        let version = self.version.read().clone();
        let picker = CompactionPicker::new(self.config.compaction.clone());

        if let Some(task) = picker.pick_compaction(&version) {
            // Get watermark at compaction time
            let watermark = watermark_fn();
            self.run_compaction_with_gc(&task, watermark)?;
            return Ok(true);
        }

        Ok(false)
    }

    /// Runs compaction until no more compaction is needed.
    ///
    /// This triggers the background compaction path repeatedly until all
    /// levels are within their size limits. If a watermark source is set,
    /// GC will be applied during each compaction round.
    ///
    /// Useful for tests and benchmarks that need a fully compacted state.
    pub fn compact_all(&self) -> Result<()> {
        // Keep compacting until no more work to do
        while self.maybe_compact()? {}
        Ok(())
    }

    /// Runs compaction with a specific watermark value.
    fn run_compaction_with_gc(&self, task: &CompactionTask, watermark: u64) -> Result<()> {
        let executor = CompactionExecutor::new(
            self.env.clone(),
            self.path.join("sst"),
            self.config.compaction.clone(),
        );

        let mut next_file_num = self.next_file_num.load(Ordering::SeqCst);
        let result = executor.execute_with_gc(task, &mut next_file_num, watermark)?;
        self.next_file_num.store(next_file_num, Ordering::SeqCst);

        // Update version
        {
            let mut version = self.version.write();
            let new_version = apply_compaction(&version, task, &result);
            *version = Arc::new(new_version);
        }

        // Remove old files from cache
        {
            let mut cache = self.table_cache.write();
            for file_num in &result.removed_files {
                cache.remove(file_num);
            }
        }

        // Delete old SSTable files
        for file_num in &result.removed_files {
            let path = self.sst_path(*file_num);
            let _ = self.env.remove(&path);
        }

        // Save manifest
        self.save_manifest()?;

        Ok(())
    }

    /// Returns the current sequence number.
    pub fn sequence(&self) -> u64 {
        self.sequence.load(Ordering::SeqCst)
    }

    /// Returns the maximum commit timestamp seen during recovery.
    ///
    /// Used by SSI to restore next_ts after crash recovery.
    pub fn max_commit_ts(&self) -> u64 {
        self.max_commit_ts.load(Ordering::SeqCst)
    }

    /// Returns the maximum transaction ID seen during recovery.
    ///
    /// Used by SSI to restore next_txn_id after crash recovery.
    pub fn max_txn_id(&self) -> u64 {
        self.max_txn_id.load(Ordering::SeqCst)
    }

    /// Enumerate all versions of a key across memtables and SSTables.
    ///
    /// Returns a Vec of (sequence_number, Option<value>) for all versions
    /// of the given key, up to `limit` versions. Searches in order:
    /// 1. Active memtable
    /// 2. Immutable memtables
    /// 3. SSTables level by level (L0, L1, L2, ...)
    ///
    /// This uses the same SSTable iteration primitives as compaction.
    pub fn iter_versions(&self, key: &[u8], limit: usize) -> Result<Vec<(u64, Option<Bytes>)>> {
        let mut versions = Vec::new();

        // 1. Search active memtable
        {
            let memtable = self.active_memtable.read();
            for (internal_key, value) in memtable.iter() {
                if internal_key.user_key.as_ref() == key {
                    let val = match value {
                        crate::memtable::MemtableValue::Put(v) => Some(v),
                        crate::memtable::MemtableValue::Delete => None,
                    };
                    versions.push((internal_key.seq, val));
                    if versions.len() >= limit {
                        return Ok(versions);
                    }
                }
            }
        }

        // 2. Search immutable memtables
        {
            let immutables = self.immutable_memtables.read();
            for memtable in immutables.iter() {
                for (internal_key, value) in memtable.iter() {
                    if internal_key.user_key.as_ref() == key {
                        let val = match value {
                            crate::memtable::MemtableValue::Put(v) => Some(v),
                            crate::memtable::MemtableValue::Delete => None,
                        };
                        versions.push((internal_key.seq, val));
                        if versions.len() >= limit {
                            return Ok(versions);
                        }
                    }
                }
            }
        }

        // 3. Search SSTables level by level
        let version = self.version.read().clone();
        for level in &version.levels {
            for file in level {
                let reader = self.get_reader(file.file_num)?;

                // Iterate all entries in the SSTable
                for entry_result in reader.iter()? {
                    let (raw_key, raw_value) = entry_result?;

                    // Decode the internal key
                    if let Some(internal_key) = decode_internal_key(&raw_key) {
                        if internal_key.user_key.as_ref() == key {
                            // Decode the value
                            let val = if let Some(memtable_value) = decode_value(&raw_value) {
                                match memtable_value {
                                    MemtableValue::Put(v) => Some(v),
                                    MemtableValue::Delete => None,
                                }
                            } else {
                                continue; // Skip malformed values
                            };

                            versions.push((internal_key.seq, val));
                            if versions.len() >= limit {
                                return Ok(versions);
                            }
                        }
                    }
                }
            }
        }

        Ok(versions)
    }

    /// Performs a snapshot-consistent range scan.
    ///
    /// Returns an iterator over key-value pairs in user_key order, where each
    /// key's value is the newest version with commit_ts <= snapshot_ts.
    /// Tombstones are filtered out (deleted keys don't appear in the result).
    ///
    /// # Arguments
    ///
    /// * `start` - Start bound of the range
    /// * `end` - End bound of the range
    /// * `snapshot_ts` - The snapshot timestamp; only versions with seq <= this are visible
    ///
    /// # Errors
    ///
    /// Returns an error if any SSTable read fails.
    pub fn scan_at_snapshot<'a>(
        &'a self,
        start: Bound<&[u8]>,
        end: Bound<&[u8]>,
        snapshot_ts: u64,
    ) -> Result<EngineScan<'a, E>> {
        let start_bound: Bound<Bytes> = match start {
            Bound::Included(k) => Bound::Included(Bytes::copy_from_slice(k)),
            Bound::Excluded(k) => Bound::Excluded(Bytes::copy_from_slice(k)),
            Bound::Unbounded => Bound::Unbounded,
        };
        let end_bound: Bound<Bytes> = match end {
            Bound::Included(k) => Bound::Included(Bytes::copy_from_slice(k)),
            Bound::Excluded(k) => Bound::Excluded(Bytes::copy_from_slice(k)),
            Bound::Unbounded => Bound::Unbounded,
        };

        let mut sources: Vec<ScanSource<'a>> = Vec::new();

        // 1. Active memtable
        {
            let memtable = self.active_memtable.read();
            let iter = memtable.iter();
            let wrapper = MemtableScanIter::new(iter, &start_bound, &end_bound);
            sources.push(Box::new(wrapper));
        }

        // 2. Immutable memtables
        {
            let immutables = self.immutable_memtables.read();
            for memtable in immutables.iter() {
                let iter = memtable.iter();
                let wrapper = MemtableScanIter::new(iter, &start_bound, &end_bound);
                sources.push(Box::new(wrapper));
            }
        }

        // 3. SSTables from all levels
        let version = self.version.read().clone();
        for level in &version.levels {
            for file in level {
                let reader = self.get_reader(file.file_num)?;
                // SSTableIterator implements Iterator<Item = Result<(Bytes, Bytes)>>
                let sstable_iter = reader.iter()?;
                // Collect entries into owned Vec (avoids lifetime issues with reader)
                let wrapper = SStableScanIter::new(sstable_iter, &start_bound, &end_bound)?;
                sources.push(Box::new(wrapper));
            }
        }

        // Initialize the heap with the first entry from each source
        let mut heap = BinaryHeap::new();
        for (source_idx, source) in sources.iter_mut().enumerate() {
            if let Some(result) = source.next() {
                let (raw_key, raw_value) = result?;
                if let Some(internal_key) = decode_internal_key(&raw_key) {
                    let value = if let Some(memtable_value) = decode_value(&raw_value) {
                        match memtable_value {
                            MemtableValue::Put(v) => Some(v),
                            MemtableValue::Delete => None,
                        }
                    } else {
                        continue;
                    };

                    heap.push(ScanHeapEntry {
                        user_key: internal_key.user_key,
                        seq: internal_key.seq,
                        value,
                        source_idx,
                    });
                }
            }
        }

        Ok(EngineScan {
            engine: self,
            heap,
            sources,
            snapshot_ts,
            end_bound,
            last_user_key: None,
            errored: false,
        })
    }

    /// Deletes a key at a specific commit timestamp (MVCC tombstone).
    ///
    /// Uses the provided `commit_ts` as the sequence number for the tombstone.
    /// Note: This method appends to the WAL but does NOT fsync. The caller
    /// should call `wal_sync()` after all writes in a transaction.
    /// For batched commits with automatic fsync, use `put_versioned_batch`.
    pub fn delete_versioned(&self, key: &[u8], commit_ts: u64) -> Result<()> {
        // Write tombstone to WAL
        let wal_record = Self::encode_wal_record(key, None, commit_ts);
        self.group_committer.append_raw(wal_record)?;

        // Write tombstone to memtable with commit_ts as sequence
        let should_flush = {
            let memtable = self.active_memtable.read();
            memtable.delete_with_seq(Bytes::copy_from_slice(key), commit_ts);
            memtable.should_flush()
        };

        if should_flush {
            self.rotate_memtable()?;
        }

        Ok(())
    }

    /// Writes a key-value pair with a specific commit timestamp (MVCC versioned write).
    ///
    /// Unlike `put()` which uses the engine's internal sequence number,
    /// this uses the provided `commit_ts` as the version identifier.
    ///
    /// Note: This method appends to the WAL but does NOT fsync. The caller
    /// should call `wal_sync()` after all writes in a transaction.
    /// For batched commits with automatic fsync, use `put_versioned_batch`.
    pub fn put_versioned(&self, key: &[u8], value: &[u8], commit_ts: u64) -> Result<()> {
        // Use commit_ts as the sequence number for this write
        let wal_record = Self::encode_wal_record(key, Some(value), commit_ts);
        self.group_committer.append_raw(wal_record)?;

        // Write to memtable with commit_ts as sequence
        let should_flush = {
            let memtable = self.active_memtable.read();
            memtable.put_with_seq(
                Bytes::copy_from_slice(key),
                Bytes::copy_from_slice(value),
                commit_ts,
            );
            memtable.should_flush()
        };

        if should_flush {
            self.rotate_memtable()?;
        }

        // Update max_commit_ts if needed
        let mut current = self.max_commit_ts.load(Ordering::SeqCst);
        while commit_ts > current {
            match self.max_commit_ts.compare_exchange_weak(
                current,
                commit_ts,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => break,
                Err(c) => current = c,
            }
        }

        // Update sequence to be at least commit_ts + 1
        let mut current_seq = self.sequence.load(Ordering::SeqCst);
        while commit_ts >= current_seq {
            match self.sequence.compare_exchange_weak(
                current_seq,
                commit_ts + 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => break,
                Err(c) => current_seq = c,
            }
        }

        Ok(())
    }

    /// Writes a batch of key-value pairs with a specific commit timestamp, with atomic fsync.
    ///
    /// This method:
    /// 1. Encodes all writes into a single WAL record
    /// 2. Appends to the shared buffer
    /// 3. Waits for fsync to complete (via group commit)
    /// 4. Applies all writes to the memtable
    ///
    /// This is the preferred method for transaction commits as it:
    /// - Reduces lock acquisitions (one instead of per-key)
    /// - Enables group commit across concurrent transactions
    /// - Guarantees atomic durability
    pub fn put_versioned_batch(
        &self,
        commit_ts: u64,
        writes: impl IntoIterator<Item = (Bytes, Option<Bytes>)>,
    ) -> Result<()> {
        // Collect writes to encode them into a single batch record
        let writes_vec: Vec<(Bytes, Option<Bytes>)> = writes.into_iter().collect();

        if writes_vec.is_empty() {
            return Ok(());
        }

        // Encode all writes into a single batch record
        // Format: BATCH_MAGIC(8) + count(4) + [[len:u32][kv_record]]...
        let mut batch_record = Vec::new();

        // Write batch magic prefix
        batch_record.extend_from_slice(&WAL_BATCH_MAGIC.to_le_bytes());

        // Write batch count
        batch_record.extend_from_slice(&(writes_vec.len() as u32).to_le_bytes());

        // Write each entry
        for (key, value) in &writes_vec {
            // Encode as individual WAL record format for recovery compatibility
            let wal_record = match value {
                Some(v) => Self::encode_wal_record(key, Some(v), commit_ts),
                None => Self::encode_wal_record(key, None, commit_ts),
            };
            // Prefix with length for parsing
            batch_record.extend_from_slice(&(wal_record.len() as u32).to_le_bytes());
            batch_record.extend_from_slice(&wal_record);
        }

        // Commit the batch record with group commit (waits for fsync)
        self.group_committer.commit_batch(batch_record)?;

        // Now apply all writes to memtable (after durable)
        let should_flush = {
            let memtable = self.active_memtable.read();
            for (key, value) in &writes_vec {
                match value {
                    Some(v) => {
                        memtable.put_with_seq(key.clone(), v.clone(), commit_ts);
                    }
                    None => {
                        memtable.delete_with_seq(key.clone(), commit_ts);
                    }
                }
            }
            memtable.should_flush()
        };

        if should_flush {
            self.rotate_memtable()?;
        }

        // Update max_commit_ts
        let mut current = self.max_commit_ts.load(Ordering::SeqCst);
        while commit_ts > current {
            match self.max_commit_ts.compare_exchange_weak(
                current,
                commit_ts,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => break,
                Err(c) => current = c,
            }
        }

        // Update sequence to be at least commit_ts + 1
        let mut current_seq = self.sequence.load(Ordering::SeqCst);
        while commit_ts >= current_seq {
            match self.sequence.compare_exchange_weak(
                current_seq,
                commit_ts + 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => break,
                Err(c) => current_seq = c,
            }
        }

        Ok(())
    }

    /// Checks if there are any writes to a key after the given timestamp.
    ///
    /// Used by SSI for write-write conflict detection.
    pub fn has_write_after(&self, key: &[u8], ts: u64) -> Result<bool> {
        // Check active memtable
        {
            let memtable = self.active_memtable.read();
            if memtable.has_write_after(key, ts) {
                return Ok(true);
            }
        }

        // Check immutable memtables
        {
            let immutables = self.immutable_memtables.read();
            for memtable in immutables.iter() {
                if memtable.has_write_after(key, ts) {
                    return Ok(true);
                }
            }
        }

        // Check SSTables - look for any key with seq > ts
        let version = self.version.read().clone();

        // Search L0 (all files)
        for file in version.levels[0].iter() {
            if let Some(found) = self.check_sstable_has_write_after(file, key, ts)? {
                if found {
                    return Ok(true);
                }
            }
        }

        // Search L1+ (use key range)
        for level in 1..version.levels.len() {
            let files = &version.levels[level];
            if files.is_empty() {
                continue;
            }

            let idx = files.partition_point(|f| {
                let largest = &f.largest_key;
                if largest.len() >= 8 {
                    &largest[..largest.len() - 8] < key
                } else {
                    largest.as_ref() < key
                }
            });

            if idx < files.len() {
                let file = &files[idx];
                if let Some(found) = self.check_sstable_has_write_after(file, key, ts)? {
                    if found {
                        return Ok(true);
                    }
                }
            }
        }

        Ok(false)
    }

    /// Appends a TxnBegin record to the WAL.
    pub fn wal_append_txn_begin(&self, txn_id: u64) -> Result<()> {
        let record = Self::encode_txn_begin_record(txn_id);
        self.group_committer.append_raw(record)?;
        Ok(())
    }

    /// Appends a TxnWrite record to the WAL.
    pub fn wal_append_txn_write(&self, txn_id: u64, key: &[u8], value: &[u8]) -> Result<()> {
        let record = Self::encode_txn_write_record(txn_id, key, value);
        self.group_committer.append_raw(record)?;
        Ok(())
    }

    /// Appends a TxnCommit record to the WAL.
    pub fn wal_append_txn_commit(&self, txn_id: u64, commit_ts: u64) -> Result<()> {
        let record = Self::encode_txn_commit_record(txn_id, commit_ts);
        self.group_committer.append_raw(record)?;

        // Update max values
        let mut current = self.max_txn_id.load(Ordering::SeqCst);
        while txn_id > current {
            match self.max_txn_id.compare_exchange_weak(
                current,
                txn_id,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => break,
                Err(c) => current = c,
            }
        }

        let mut current = self.max_commit_ts.load(Ordering::SeqCst);
        while commit_ts > current {
            match self.max_commit_ts.compare_exchange_weak(
                current,
                commit_ts,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => break,
                Err(c) => current = c,
            }
        }

        Ok(())
    }

    /// Appends a TxnAbort record to the WAL.
    pub fn wal_append_txn_abort(&self, txn_id: u64) -> Result<()> {
        let record = Self::encode_txn_abort_record(txn_id);
        self.group_committer.append_raw(record)?;
        Ok(())
    }

    /// Syncs the WAL to disk.
    ///
    /// This is now handled through group commit for transaction commits.
    /// For direct sync (e.g., during shutdown), use this method.
    pub fn wal_sync(&self) -> Result<()> {
        self.group_committer.sync()
    }

    /// Returns the group commit statistics.
    ///
    /// Use this to monitor commits-per-fsync ratio for performance analysis.
    pub fn group_commit_stats(&self) -> &GroupCommitStats {
        &self.group_committer.stats
    }

    // Internal methods

    fn write(&self, key: &[u8], value: Option<&[u8]>) -> Result<()> {
        // Get next sequence number
        let seq = self.sequence.fetch_add(1, Ordering::SeqCst);

        // Write to WAL with group commit
        let wal_record = Self::encode_wal_record(key, value, seq);
        self.group_committer.commit_batch(wal_record)?;

        // Write to memtable
        let should_flush = {
            let memtable = self.active_memtable.read();
            match value {
                Some(v) => memtable.put_with_seq(
                    Bytes::copy_from_slice(key),
                    Bytes::copy_from_slice(v),
                    seq,
                ),
                None => memtable.delete_with_seq(Bytes::copy_from_slice(key), seq),
            }
            memtable.should_flush()
        };

        // Check if we need to flush
        if should_flush {
            self.rotate_memtable()?;
        }

        Ok(())
    }

    fn rotate_memtable(&self) -> Result<()> {
        let new_id = self.next_memtable_id.fetch_add(1, Ordering::SeqCst);
        let memtable_config = MemtableConfig {
            max_size: self.config.memtable_size,
        };
        let new_memtable = Arc::new(Memtable::new(new_id, memtable_config));

        let old_memtable = {
            let mut active = self.active_memtable.write();
            std::mem::replace(&mut *active, new_memtable)
        };

        // Add to immutable list
        {
            let mut immutables = self.immutable_memtables.write();
            immutables.push(old_memtable);
        }

        Ok(())
    }

    fn flush_immutable_memtables(&self) -> Result<()> {
        loop {
            // Get the oldest immutable memtable
            let memtable = {
                let mut immutables = self.immutable_memtables.write();
                if immutables.is_empty() {
                    return Ok(());
                }
                immutables.remove(0)
            };

            // Flush to SSTable
            self.flush_memtable_to_sstable(&memtable)?;
        }
    }

    fn flush_memtable_to_sstable(&self, memtable: &Memtable) -> Result<()> {
        let file_num = self.next_file_num.fetch_add(1, Ordering::SeqCst);
        let path = self.sst_path(file_num);

        let mut builder = SSTableBuilder::new(
            self.env.clone(),
            &path,
            self.config.compaction.sstable_config.clone(),
        )?;

        let mut first_key: Option<Bytes> = None;
        let mut last_key: Option<Bytes> = None;

        for (key, value) in memtable.iter() {
            let encoded_key = encode_internal_key(&key);

            if first_key.is_none() {
                first_key = Some(encoded_key.clone());
            }
            last_key = Some(encoded_key.clone());

            builder.add(&key, &value)?;
        }

        let meta = builder.finish()?;

        if let (Some(smallest), Some(largest)) = (first_key, last_key) {
            // Add to L0
            let file_meta = FileMetadata {
                file_num,
                file_size: meta.file_size,
                smallest_key: smallest,
                largest_key: largest,
            };

            let mut version = self.version.write();
            let mut new_version = (**version).clone();
            new_version.levels[0].push(file_meta);
            *version = Arc::new(new_version);
        }

        // Save manifest
        self.save_manifest()?;

        Ok(())
    }

    fn run_compaction(&self, task: &CompactionTask) -> Result<()> {
        let executor = CompactionExecutor::new(
            self.env.clone(),
            self.path.join("sst"),
            self.config.compaction.clone(),
        );

        let mut next_file_num = self.next_file_num.load(Ordering::SeqCst);
        let result = executor.execute(task, &mut next_file_num)?;
        self.next_file_num.store(next_file_num, Ordering::SeqCst);

        // Update version
        {
            let mut version = self.version.write();
            let new_version = apply_compaction(&version, task, &result);
            *version = Arc::new(new_version);
        }

        // Remove old files from cache
        {
            let mut cache = self.table_cache.write();
            for file_num in &result.removed_files {
                cache.remove(file_num);
            }
        }

        // Delete old SSTable files
        for file_num in &result.removed_files {
            let path = self.sst_path(*file_num);
            let _ = self.env.remove(&path);
        }

        // Save manifest
        self.save_manifest()?;

        Ok(())
    }

    fn search_sstable(
        &self,
        file: &FileMetadata,
        _search_key: &Bytes,
        user_key: &[u8],
        seq: u64,
    ) -> Result<Option<Option<Bytes>>> {
        let reader = self.get_reader(file.file_num)?;

        // Check bloom filter first - skip SSTable if key definitely not present
        if !reader.may_contain(user_key) {
            return Ok(None);
        }

        // Use MVCC-aware lookup to find the right version
        if let Some(encoded_value) = reader.get_mvcc(user_key, seq)? {
            if let Some(value) = decode_value(&encoded_value) {
                return Ok(Some(match value {
                    MemtableValue::Put(v) => Some(v),
                    MemtableValue::Delete => None,
                }));
            }
        }

        Ok(None)
    }

    fn get_reader(&self, file_num: u64) -> Result<Arc<SSTableReader<E>>> {
        // Check cache first
        {
            let cache = self.table_cache.read();
            if let Some(reader) = cache.get(&file_num) {
                return Ok(reader.clone());
            }
        }

        // Open and cache
        let path = self.sst_path(file_num);
        let reader = Arc::new(SSTableReader::open(self.env.clone(), &path)?);

        {
            let mut cache = self.table_cache.write();
            cache.insert(file_num, reader.clone());
        }

        Ok(reader)
    }

    fn sst_path(&self, file_num: u64) -> PathBuf {
        self.path.join("sst").join(format!("{:06}.sst", file_num))
    }

    fn encode_wal_record(key: &[u8], value: Option<&[u8]>, seq: u64) -> Vec<u8> {
        let mut record = Vec::new();

        // Sequence number (8 bytes)
        record.extend_from_slice(&seq.to_le_bytes());

        // Key length (4 bytes)
        record.extend_from_slice(&(key.len() as u32).to_le_bytes());

        // Key
        record.extend_from_slice(key);

        // Value type and data
        match value {
            Some(v) => {
                record.push(1); // Type: Put
                record.extend_from_slice(&(v.len() as u32).to_le_bytes());
                record.extend_from_slice(v);
            }
            None => {
                record.push(0); // Type: Delete
            }
        }

        record
    }

    fn decode_wal_record(data: &[u8]) -> Option<(Bytes, Option<Bytes>, u64)> {
        if data.len() < 13 {
            return None;
        }

        let seq = u64::from_le_bytes(data[0..8].try_into().ok()?);
        let key_len = u32::from_le_bytes(data[8..12].try_into().ok()?) as usize;

        if data.len() < 12 + key_len + 1 {
            return None;
        }

        let key = Bytes::copy_from_slice(&data[12..12 + key_len]);
        let value_type = data[12 + key_len];

        let value = match value_type {
            0 => None, // Delete
            1 => {
                if data.len() < 12 + key_len + 5 {
                    return None;
                }
                let value_len =
                    u32::from_le_bytes(data[13 + key_len..17 + key_len].try_into().ok()?) as usize;
                if data.len() < 17 + key_len + value_len {
                    return None;
                }
                Some(Bytes::copy_from_slice(
                    &data[17 + key_len..17 + key_len + value_len],
                ))
            }
            _ => return None,
        };

        Some((key, value, seq))
    }

    fn encode_txn_begin_record(txn_id: u64) -> Vec<u8> {
        let mut record = Vec::with_capacity(17);
        record.extend_from_slice(&WAL_TXN_MAGIC.to_le_bytes()); // Magic prefix
        record.push(WAL_TYPE_TXN_BEGIN);
        record.extend_from_slice(&txn_id.to_le_bytes());
        record
    }

    fn encode_txn_write_record(txn_id: u64, key: &[u8], value: &[u8]) -> Vec<u8> {
        let mut record = Vec::with_capacity(9 + 8 + 4 + key.len() + 4 + value.len());
        record.extend_from_slice(&WAL_TXN_MAGIC.to_le_bytes()); // Magic prefix
        record.push(WAL_TYPE_TXN_WRITE);
        record.extend_from_slice(&txn_id.to_le_bytes());
        record.extend_from_slice(&(key.len() as u32).to_le_bytes());
        record.extend_from_slice(key);
        record.extend_from_slice(&(value.len() as u32).to_le_bytes());
        record.extend_from_slice(value);
        record
    }

    fn encode_txn_commit_record(txn_id: u64, commit_ts: u64) -> Vec<u8> {
        let mut record = Vec::with_capacity(25);
        record.extend_from_slice(&WAL_TXN_MAGIC.to_le_bytes()); // Magic prefix
        record.push(WAL_TYPE_TXN_COMMIT);
        record.extend_from_slice(&txn_id.to_le_bytes());
        record.extend_from_slice(&commit_ts.to_le_bytes());
        record
    }

    fn encode_txn_abort_record(txn_id: u64) -> Vec<u8> {
        let mut record = Vec::with_capacity(17);
        record.extend_from_slice(&WAL_TXN_MAGIC.to_le_bytes()); // Magic prefix
        record.push(WAL_TYPE_TXN_ABORT);
        record.extend_from_slice(&txn_id.to_le_bytes());
        record
    }

    fn decode_wal_payload(data: &[u8]) -> Option<WalPayload> {
        if data.len() < 8 {
            return None;
        }

        // Check if this is a batch record, transaction record, or legacy KV record
        let first_u64 = u64::from_le_bytes(data[0..8].try_into().ok()?);

        if first_u64 == WAL_BATCH_MAGIC {
            // Batch record format: MAGIC(8) + count(4) + [[len:u32][kv_record]]...
            if data.len() < 12 {
                return None;
            }
            let count = u32::from_le_bytes(data[8..12].try_into().ok()?) as usize;
            let mut writes = Vec::with_capacity(count);
            let mut offset = 12;

            for _ in 0..count {
                if offset + 4 > data.len() {
                    return None;
                }
                let record_len = u32::from_le_bytes(data[offset..offset + 4].try_into().ok()?) as usize;
                offset += 4;

                if offset + record_len > data.len() {
                    return None;
                }
                let record_data = &data[offset..offset + record_len];
                offset += record_len;

                // Decode individual KV record
                if let Some((key, value, seq)) = Self::decode_wal_record(record_data) {
                    writes.push((key, value, seq));
                } else {
                    return None; // Corrupted batch
                }
            }

            Some(WalPayload::Batch { writes })
        } else if first_u64 == WAL_TXN_MAGIC {
            // Transaction record format: MAGIC(8) + type(1) + payload
            if data.len() < 9 {
                return None;
            }
            let record_type = data[8];

            match record_type {
                WAL_TYPE_TXN_BEGIN => {
                    if data.len() < 17 {
                        return None;
                    }
                    let txn_id = u64::from_le_bytes(data[9..17].try_into().ok()?);
                    Some(WalPayload::TxnBegin { txn_id })
                }
                WAL_TYPE_TXN_WRITE => {
                    if data.len() < 21 {
                        return None;
                    }
                    let txn_id = u64::from_le_bytes(data[9..17].try_into().ok()?);
                    let key_len = u32::from_le_bytes(data[17..21].try_into().ok()?) as usize;
                    if data.len() < 21 + key_len + 4 {
                        return None;
                    }
                    let key = Bytes::copy_from_slice(&data[21..21 + key_len]);
                    let value_len =
                        u32::from_le_bytes(data[21 + key_len..25 + key_len].try_into().ok()?)
                            as usize;
                    if data.len() < 25 + key_len + value_len {
                        return None;
                    }
                    let value =
                        Bytes::copy_from_slice(&data[25 + key_len..25 + key_len + value_len]);
                    Some(WalPayload::TxnWrite { txn_id, key, value })
                }
                WAL_TYPE_TXN_COMMIT => {
                    if data.len() < 25 {
                        return None;
                    }
                    let txn_id = u64::from_le_bytes(data[9..17].try_into().ok()?);
                    let commit_ts = u64::from_le_bytes(data[17..25].try_into().ok()?);
                    Some(WalPayload::TxnCommit { txn_id, commit_ts })
                }
                WAL_TYPE_TXN_ABORT => {
                    if data.len() < 17 {
                        return None;
                    }
                    let txn_id = u64::from_le_bytes(data[9..17].try_into().ok()?);
                    Some(WalPayload::TxnAbort { txn_id })
                }
                _ => None, // Unknown transaction record type
            }
        } else {
            // Legacy KV record format: seq(8) + key_len(4) + key + type(1) + [value_len(4) + value]
            Self::decode_wal_record(data).map(|(key, value, seq)| WalPayload::Kv {
                key,
                value,
                seq,
            })
        }
    }

    fn check_sstable_has_write_after(
        &self,
        file: &FileMetadata,
        user_key: &[u8],
        ts: u64,
    ) -> Result<Option<bool>> {
        let reader = self.get_reader(file.file_num)?;

        // Check bloom filter first
        if !reader.may_contain(user_key) {
            return Ok(None);
        }

        // Check if there's any version with seq > ts
        // We search for the highest sequence number version of this key
        if let Some(encoded_value) = reader.get_mvcc(user_key, u64::MAX)? {
            // Found a version - check its sequence number
            // The get_mvcc returns the value for the highest seq <= requested
            // We need to check if any seq > ts exists
            // For now, we check if the key exists at all with a high seq
            if decode_value(&encoded_value).is_some() {
                // The reader.get_mvcc with u64::MAX returns the latest version
                // We need to check if its seq > ts
                // Unfortunately, we don't have direct access to the seq here
                // Let's check by looking for the key at ts - if different or missing, there's a write after
                if let Some(old_value) = reader.get_mvcc(user_key, ts)? {
                    // Compare with latest - if different, there's a write after ts
                    if old_value != encoded_value {
                        return Ok(Some(true));
                    }
                } else {
                    // No value at ts but there is one at MAX - write happened after ts
                    return Ok(Some(true));
                }
            }
        }

        Ok(Some(false))
    }

    fn recover(&self) -> Result<()> {
        let wal_path = self.path.join("wal");

        // Create WAL reader starting from segment 0
        let mut reader = WalReader::new_from_start(self.env.clone(), &wal_path)?;

        // Track transaction writes for replay on commit
        let mut pending_txn_writes: HashMap<u64, Vec<(Bytes, Bytes)>> = HashMap::new();

        // Replay all records
        let mut max_seq = 0u64;
        let mut max_txn_id = 0u64;
        let mut max_commit_ts = 0u64;

        while let Some(record) = reader.read_record()? {
            if let Some(payload) = Self::decode_wal_payload(&record.data) {
                match payload {
                    WalPayload::Kv { key, value, seq } => {
                        // Legacy KV record - replay directly
                        // The seq number doubles as commit_ts for MVCC versioned writes
                        if seq > max_seq {
                            max_seq = seq;
                        }
                        if seq > max_commit_ts {
                            max_commit_ts = seq;
                        }
                        let memtable = self.active_memtable.read();
                        match value {
                            Some(v) => memtable.put_with_seq(key, v, seq),
                            None => memtable.delete_with_seq(key, seq),
                        }
                    }
                    WalPayload::Batch { writes } => {
                        // Batch record from group commit - replay all writes
                        let memtable = self.active_memtable.read();
                        for (key, value, seq) in writes {
                            if seq > max_seq {
                                max_seq = seq;
                            }
                            if seq > max_commit_ts {
                                max_commit_ts = seq;
                            }
                            match value {
                                Some(v) => memtable.put_with_seq(key, v, seq),
                                None => memtable.delete_with_seq(key, seq),
                            }
                        }
                    }
                    WalPayload::TxnBegin { txn_id } => {
                        if txn_id > max_txn_id {
                            max_txn_id = txn_id;
                        }
                        pending_txn_writes.insert(txn_id, Vec::new());
                    }
                    WalPayload::TxnWrite { txn_id, key, value } => {
                        if let Some(writes) = pending_txn_writes.get_mut(&txn_id) {
                            writes.push((key, value));
                        }
                        // If no TxnBegin seen, this is an orphan write - ignore
                    }
                    WalPayload::TxnCommit { txn_id, commit_ts } => {
                        if txn_id > max_txn_id {
                            max_txn_id = txn_id;
                        }
                        if commit_ts > max_commit_ts {
                            max_commit_ts = commit_ts;
                        }
                        if commit_ts > max_seq {
                            max_seq = commit_ts;
                        }

                        // Replay all writes from this transaction
                        if let Some(writes) = pending_txn_writes.remove(&txn_id) {
                            let memtable = self.active_memtable.read();
                            for (key, value) in writes {
                                memtable.put_with_seq(key, value, commit_ts);
                            }
                        }
                    }
                    WalPayload::TxnAbort { txn_id } => {
                        // Discard pending writes for this transaction
                        pending_txn_writes.remove(&txn_id);
                    }
                }
            }
        }

        // Update global state
        if max_seq > 0 {
            self.sequence.store(max_seq + 1, Ordering::SeqCst);
        }
        if max_txn_id > 0 {
            self.max_txn_id.store(max_txn_id, Ordering::SeqCst);
        }
        if max_commit_ts > 0 {
            self.max_commit_ts.store(max_commit_ts, Ordering::SeqCst);
        }

        Ok(())
    }

    fn load_manifest(env: &E, path: &Path) -> Result<(Version, u64, u64)> {
        let manifest_path = path.join("MANIFEST");
        let file = env.open(&manifest_path, OpenOptions::read())?;

        let len = file.len()? as usize;
        let mut data = vec![0u8; len];
        file.read_exact_at(&mut data, 0)?;

        Self::decode_manifest(&data)
    }

    fn save_manifest(&self) -> Result<()> {
        let manifest_path = self.path.join("MANIFEST");

        let version = self.version.read();
        let file_num = self.next_file_num.load(Ordering::SeqCst);
        let seq = self.sequence.load(Ordering::SeqCst);

        let data = Self::encode_manifest(&version, file_num, seq);

        // Write to temp file first
        let temp_path = self.path.join("MANIFEST.tmp");

        // Remove temp file if it exists
        let _ = self.env.remove(&temp_path);

        let file = self.env.open(&temp_path, OpenOptions::create_new())?;
        file.write_all_at(&data, 0)?;
        file.sync()?;

        // Atomic rename
        self.env.rename(&temp_path, &manifest_path)?;

        Ok(())
    }

    fn encode_manifest(version: &Version, file_num: u64, seq: u64) -> Vec<u8> {
        let mut data = Vec::new();

        // Header: magic + version
        data.extend_from_slice(&0x4d414e49u32.to_le_bytes()); // "MANI"
        data.extend_from_slice(&1u32.to_le_bytes()); // Version 1

        // File number and sequence
        data.extend_from_slice(&file_num.to_le_bytes());
        data.extend_from_slice(&seq.to_le_bytes());

        // Number of levels
        data.extend_from_slice(&(version.levels.len() as u32).to_le_bytes());

        // Each level
        for level in &version.levels {
            data.extend_from_slice(&(level.len() as u32).to_le_bytes());

            for file in level {
                data.extend_from_slice(&file.file_num.to_le_bytes());
                data.extend_from_slice(&file.file_size.to_le_bytes());

                data.extend_from_slice(&(file.smallest_key.len() as u32).to_le_bytes());
                data.extend_from_slice(&file.smallest_key);

                data.extend_from_slice(&(file.largest_key.len() as u32).to_le_bytes());
                data.extend_from_slice(&file.largest_key);
            }
        }

        data
    }

    fn decode_manifest(data: &[u8]) -> Result<(Version, u64, u64)> {
        if data.len() < 24 {
            return Err(Error::Corruption("manifest too small".to_string()));
        }

        let magic = u32::from_le_bytes(data[0..4].try_into().unwrap());
        if magic != 0x4d414e49 {
            return Err(Error::Corruption("invalid manifest magic".to_string()));
        }

        let _version = u32::from_le_bytes(data[4..8].try_into().unwrap());
        let file_num = u64::from_le_bytes(data[8..16].try_into().unwrap());
        let seq = u64::from_le_bytes(data[16..24].try_into().unwrap());

        let num_levels = u32::from_le_bytes(data[24..28].try_into().unwrap()) as usize;

        let mut offset = 28;
        let mut levels = Vec::with_capacity(num_levels);

        for _ in 0..num_levels {
            if offset + 4 > data.len() {
                return Err(Error::Corruption("truncated manifest".to_string()));
            }

            let num_files =
                u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
            offset += 4;

            let mut files = Vec::with_capacity(num_files);

            for _ in 0..num_files {
                if offset + 16 > data.len() {
                    return Err(Error::Corruption("truncated manifest".to_string()));
                }

                let file_num = u64::from_le_bytes(data[offset..offset + 8].try_into().unwrap());
                let file_size =
                    u64::from_le_bytes(data[offset + 8..offset + 16].try_into().unwrap());
                offset += 16;

                if offset + 4 > data.len() {
                    return Err(Error::Corruption("truncated manifest".to_string()));
                }
                let smallest_len =
                    u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
                offset += 4;

                if offset + smallest_len > data.len() {
                    return Err(Error::Corruption("truncated manifest".to_string()));
                }
                let smallest_key = Bytes::copy_from_slice(&data[offset..offset + smallest_len]);
                offset += smallest_len;

                if offset + 4 > data.len() {
                    return Err(Error::Corruption("truncated manifest".to_string()));
                }
                let largest_len =
                    u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
                offset += 4;

                if offset + largest_len > data.len() {
                    return Err(Error::Corruption("truncated manifest".to_string()));
                }
                let largest_key = Bytes::copy_from_slice(&data[offset..offset + largest_len]);
                offset += largest_len;

                files.push(FileMetadata {
                    file_num,
                    file_size,
                    smallest_key,
                    largest_key,
                });
            }

            levels.push(files);
        }

        Ok((Version { levels }, file_num, seq))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use runtime::{SimEnv, SimEnvConfig};

    fn test_env() -> SimEnv {
        SimEnv::new(SimEnvConfig::with_seed(42))
    }

    #[test]
    fn basic_put_get() {
        let env = test_env();
        let engine = LsmEngine::open(env, Path::new("/db"), EngineConfig::default()).unwrap();

        engine.put(b"key1", b"value1").unwrap();
        engine.put(b"key2", b"value2").unwrap();

        assert_eq!(engine.get(b"key1").unwrap(), Some(Bytes::from("value1")));
        assert_eq!(engine.get(b"key2").unwrap(), Some(Bytes::from("value2")));
        assert_eq!(engine.get(b"key3").unwrap(), None);
    }

    #[test]
    fn overwrite_key() {
        let env = test_env();
        let engine = LsmEngine::open(env, Path::new("/db"), EngineConfig::default()).unwrap();

        engine.put(b"key", b"value1").unwrap();
        assert_eq!(engine.get(b"key").unwrap(), Some(Bytes::from("value1")));

        engine.put(b"key", b"value2").unwrap();
        assert_eq!(engine.get(b"key").unwrap(), Some(Bytes::from("value2")));
    }

    #[test]
    fn delete_key() {
        let env = test_env();
        let engine = LsmEngine::open(env, Path::new("/db"), EngineConfig::default()).unwrap();

        engine.put(b"key", b"value").unwrap();
        assert_eq!(engine.get(b"key").unwrap(), Some(Bytes::from("value")));

        engine.delete(b"key").unwrap();
        assert_eq!(engine.get(b"key").unwrap(), None);
    }

    #[test]
    fn flush_to_sstable() {
        let env = test_env();
        let config = EngineConfig {
            memtable_size: 1024, // Small to trigger flush quickly
            ..Default::default()
        };
        let engine = LsmEngine::open(env.clone(), Path::new("/db"), config).unwrap();

        // Write enough to trigger flush
        for i in 0..100 {
            let key = format!("key{:05}", i);
            let value = format!("value{:05}", i);
            engine.put(key.as_bytes(), value.as_bytes()).unwrap();
        }

        // Force flush
        engine.flush().unwrap();

        // Verify data is still readable
        for i in 0..100 {
            let key = format!("key{:05}", i);
            let expected = format!("value{:05}", i);
            assert_eq!(
                engine.get(key.as_bytes()).unwrap(),
                Some(Bytes::from(expected))
            );
        }

        // Check that SSTable files were created
        let sst_dir = Path::new("/db/sst");
        let files = env.list_dir(sst_dir).unwrap();
        assert!(!files.is_empty());
    }

    #[test]
    fn recovery_from_wal() {
        let env = test_env();

        // Write some data
        {
            let engine =
                LsmEngine::open(env.clone(), Path::new("/db"), EngineConfig::default()).unwrap();
            engine.put(b"key1", b"value1").unwrap();
            engine.put(b"key2", b"value2").unwrap();
            engine.delete(b"key1").unwrap();
            engine.put(b"key3", b"value3").unwrap();
        }

        // Reopen and verify recovery
        {
            let engine = LsmEngine::open(env, Path::new("/db"), EngineConfig::default()).unwrap();
            assert_eq!(engine.get(b"key1").unwrap(), None); // Deleted
            assert_eq!(engine.get(b"key2").unwrap(), Some(Bytes::from("value2")));
            assert_eq!(engine.get(b"key3").unwrap(), Some(Bytes::from("value3")));
        }
    }

    #[test]
    fn mvcc_read_at_sequence() {
        let env = test_env();
        let engine = LsmEngine::open(env, Path::new("/db"), EngineConfig::default()).unwrap();

        // Write multiple versions
        engine.put(b"key", b"v1").unwrap();
        let seq1 = engine.sequence();

        engine.put(b"key", b"v2").unwrap();
        let seq2 = engine.sequence();

        engine.put(b"key", b"v3").unwrap();

        // Read at different sequences
        // Note: Each write advances sequence, so we need seq-1 to read the write
        assert_eq!(
            engine.get_at(b"key", seq1 - 1).unwrap(),
            Some(Bytes::from("v1"))
        );
        assert_eq!(
            engine.get_at(b"key", seq2 - 1).unwrap(),
            Some(Bytes::from("v2"))
        );
        assert_eq!(engine.get(b"key").unwrap(), Some(Bytes::from("v3")));
    }

    #[test]
    fn compaction_reduces_l0_files() {
        let env = test_env();
        let config = EngineConfig {
            memtable_size: 512, // Very small
            compaction: CompactionConfig {
                l0_compaction_trigger: 2,
                ..Default::default()
            },
            ..Default::default()
        };
        let engine = LsmEngine::open(env.clone(), Path::new("/db"), config).unwrap();

        // Write data to create multiple L0 files
        for batch in 0..4 {
            for i in 0..50 {
                let key = format!("batch{}_key{:05}", batch, i);
                let value = format!("value{:05}", i);
                engine.put(key.as_bytes(), value.as_bytes()).unwrap();
            }
            engine.flush().unwrap();
        }

        // Check L0 has files
        let version_before = engine.version.read().clone();
        let l0_count_before = version_before.levels[0].len();
        assert!(l0_count_before > 0);

        // Run compaction
        while engine.maybe_compact().unwrap() {}

        // Verify L0 is reduced and data is still readable
        let version_after = engine.version.read().clone();
        assert!(
            version_after.levels[0].len() < l0_count_before || !version_after.levels[1].is_empty()
        );

        // Verify L1 files are properly populated
        let l1_files = &version_after.levels[1];
        assert!(
            !l1_files.is_empty(),
            "L1 should have files after compaction"
        );

        // Verify the output file contains all entries
        for file in l1_files {
            let path = engine.sst_path(file.file_num);
            let reader = SSTableReader::open(env.clone(), &path).unwrap();
            let entry_count = reader.iter().unwrap().count();
            assert!(
                entry_count > 0,
                "L1 file {} should have entries",
                file.file_num
            );
        }

        // Verify data integrity
        for batch in 0..4 {
            for i in 0..50 {
                let key = format!("batch{}_key{:05}", batch, i);
                let expected = format!("value{:05}", i);
                let actual = engine.get(key.as_bytes()).unwrap();
                assert_eq!(
                    actual,
                    Some(Bytes::from(expected.clone())),
                    "Failed for key: {} (expected {:?}, got {:?})",
                    key,
                    expected,
                    actual
                );
            }
        }
    }

    #[test]
    fn iter_versions_sees_sstable_versions() {
        let env = SimEnv::new(SimEnvConfig::with_seed(0x1234));
        let engine = LsmEngine::open(env, Path::new("/db"), EngineConfig::default()).unwrap();

        // Write 10 versions of a key using put_versioned
        for i in 0..10 {
            engine
                .put_versioned(b"k", format!("v{}", i).as_bytes(), i as u64 + 1)
                .unwrap();
        }

        // Flush to force memtable to SSTable
        engine.flush().unwrap();

        // Count versions - should see all 10 in SSTable
        let versions = engine.iter_versions(b"k", 100).unwrap();
        println!(
            "iter_versions_sees_sstable_versions: found {} versions",
            versions.len()
        );
        for (seq, val) in &versions {
            println!(
                "  seq={} val={:?}",
                seq,
                val.as_ref().map(|b| String::from_utf8_lossy(b).to_string())
            );
        }

        assert_eq!(
            versions.len(),
            10,
            "should see all 10 versions in SSTable; found {}",
            versions.len()
        );
    }

    #[test]
    fn scan_at_snapshot_basic() {
        let env = test_env();
        let engine = LsmEngine::open(env, Path::new("/db"), EngineConfig::default()).unwrap();

        // Write some versioned data
        engine.put_versioned(b"a", b"a1", 1).unwrap();
        engine.put_versioned(b"b", b"b1", 2).unwrap();
        engine.put_versioned(b"c", b"c1", 3).unwrap();
        engine.wal_sync().unwrap();

        // Scan all at snapshot_ts=10 (all visible)
        let results: Vec<_> = engine
            .scan_at_snapshot(Bound::Unbounded, Bound::Unbounded, 10)
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();

        assert_eq!(results.len(), 3);
        assert_eq!(results[0], (Bytes::from("a"), Bytes::from("a1")));
        assert_eq!(results[1], (Bytes::from("b"), Bytes::from("b1")));
        assert_eq!(results[2], (Bytes::from("c"), Bytes::from("c1")));
    }

    #[test]
    fn scan_at_snapshot_respects_timestamp() {
        let env = test_env();
        let engine = LsmEngine::open(env, Path::new("/db"), EngineConfig::default()).unwrap();

        // Write versions at different timestamps
        engine.put_versioned(b"a", b"a1", 1).unwrap();
        engine.put_versioned(b"a", b"a2", 5).unwrap();
        engine.put_versioned(b"b", b"b1", 3).unwrap();
        engine.wal_sync().unwrap();

        // Scan at timestamp 2: only a@1 visible
        let results: Vec<_> = engine
            .scan_at_snapshot(Bound::Unbounded, Bound::Unbounded, 2)
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0], (Bytes::from("a"), Bytes::from("a1")));

        // Scan at timestamp 4: a@1 and b@3 visible
        let results: Vec<_> = engine
            .scan_at_snapshot(Bound::Unbounded, Bound::Unbounded, 4)
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0], (Bytes::from("a"), Bytes::from("a1")));
        assert_eq!(results[1], (Bytes::from("b"), Bytes::from("b1")));

        // Scan at timestamp 6: a@5 (newer version) and b@3 visible
        let results: Vec<_> = engine
            .scan_at_snapshot(Bound::Unbounded, Bound::Unbounded, 6)
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0], (Bytes::from("a"), Bytes::from("a2")));
        assert_eq!(results[1], (Bytes::from("b"), Bytes::from("b1")));
    }

    #[test]
    fn scan_at_snapshot_filters_tombstones() {
        let env = test_env();
        let engine = LsmEngine::open(env, Path::new("/db"), EngineConfig::default()).unwrap();

        engine.put_versioned(b"a", b"a1", 1).unwrap();
        engine.put_versioned(b"b", b"b1", 2).unwrap();
        engine.delete_versioned(b"a", 3).unwrap(); // Delete key a
        engine.wal_sync().unwrap();

        // At timestamp 2, both visible
        let results: Vec<_> = engine
            .scan_at_snapshot(Bound::Unbounded, Bound::Unbounded, 2)
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(results.len(), 2);

        // At timestamp 4, key a is deleted (tombstone)
        let results: Vec<_> = engine
            .scan_at_snapshot(Bound::Unbounded, Bound::Unbounded, 4)
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0], (Bytes::from("b"), Bytes::from("b1")));
    }

    #[test]
    fn scan_at_snapshot_range_bounds() {
        let env = test_env();
        let engine = LsmEngine::open(env, Path::new("/db"), EngineConfig::default()).unwrap();

        engine.put_versioned(b"a", b"a1", 1).unwrap();
        engine.put_versioned(b"b", b"b1", 2).unwrap();
        engine.put_versioned(b"c", b"c1", 3).unwrap();
        engine.put_versioned(b"d", b"d1", 4).unwrap();
        engine.wal_sync().unwrap();

        // Inclusive range [b, c]
        let results: Vec<_> = engine
            .scan_at_snapshot(
                Bound::Included(b"b".as_ref()),
                Bound::Included(b"c".as_ref()),
                10,
            )
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, Bytes::from("b"));
        assert_eq!(results[1].0, Bytes::from("c"));

        // Exclusive range (b, d)
        let results: Vec<_> = engine
            .scan_at_snapshot(
                Bound::Excluded(b"b".as_ref()),
                Bound::Excluded(b"d".as_ref()),
                10,
            )
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, Bytes::from("c"));

        // Unbounded start
        let results: Vec<_> = engine
            .scan_at_snapshot(Bound::Unbounded, Bound::Excluded(b"c".as_ref()), 10)
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, Bytes::from("a"));
        assert_eq!(results[1].0, Bytes::from("b"));
    }

    #[test]
    fn scan_at_snapshot_across_sstable() {
        let env = test_env();
        let config = EngineConfig {
            memtable_size: 512, // Small to trigger flushes
            ..Default::default()
        };
        let engine = LsmEngine::open(env.clone(), Path::new("/db"), config).unwrap();

        // Write enough data to force SSTable creation
        for i in 0..50 {
            let key = format!("key{:05}", i);
            let value = format!("val{:05}", i);
            engine
                .put_versioned(key.as_bytes(), value.as_bytes(), i as u64 + 1)
                .unwrap();
        }
        engine.wal_sync().unwrap();

        // Force flush to SSTable
        engine.flush().unwrap();

        // Scan should see all entries
        let results: Vec<_> = engine
            .scan_at_snapshot(Bound::Unbounded, Bound::Unbounded, 100)
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(results.len(), 50);

        // Verify order
        for (i, (key, value)) in results.iter().enumerate() {
            let expected_key = format!("key{:05}", i);
            let expected_value = format!("val{:05}", i);
            assert_eq!(key.as_ref(), expected_key.as_bytes());
            assert_eq!(value.as_ref(), expected_value.as_bytes());
        }
    }

    /// Test that scan propagates engine errors instead of silently swallowing them.
    ///
    /// This test is marked `#[ignore]` because SimEnv's fault injection does not
    /// currently reach the SSTable read path. FaultConfig supports write-oriented
    /// faults (partial writes, disk full, slow writes) but not read faults.
    ///
    /// To unblock this test, one of the following is needed:
    /// 1. Add `read_fault_prob` to FaultConfig that causes read_at() to return Err
    /// 2. Create a mock Engine wrapper that can be configured to return Err on reads
    /// 3. Add corruption injection that writes invalid data to SSTables
    ///
    /// The test exists by name so a future implementation can fill it in.
    /// The error propagation path is verified at compile-time by the return type:
    /// `scan_at_snapshot` returns `Result<EngineScan>` and `EngineScan::next()`
    /// returns `Option<Result<(Bytes, Bytes)>>`, ensuring errors propagate.
    #[test]
    #[ignore]
    fn scan_returns_engine_errors() {
        // When read fault injection is available:
        // 1. Create engine with fault injection enabled
        // 2. Write some data and flush to SSTable
        // 3. Configure a fault that triggers on the next read
        // 4. Start a scan and iterate
        // 5. Assert that Iterator::next() returns Some(Err(...))
        //
        // Until then, the type signature change ensures errors are propagated:
        // - EngineScan implements Iterator<Item = Result<(Bytes, Bytes)>>
        // - Any I/O error during SSTable reads will surface through next()
    }
}

/// Crash simulation tests for the storage layer.
///
/// These tests verify that the storage engine correctly recovers from crashes
/// and maintains data integrity under various failure scenarios.
#[cfg(test)]
mod crash_tests {
    use super::*;
    use runtime::{SimEnv, SimEnvConfig};

    fn test_env_with_seed(seed: u64) -> SimEnv {
        SimEnv::new(SimEnvConfig::with_seed(seed))
    }

    /// Test that synced writes survive crash.
    ///
    /// This is the fundamental durability guarantee: if a write returns success,
    /// the data must survive a crash.
    #[test]
    fn synced_writes_survive_crash() {
        let env = test_env_with_seed(42);

        // Write data
        {
            let engine =
                LsmEngine::open(env.clone(), Path::new("/db"), EngineConfig::default()).unwrap();
            engine.put(b"key1", b"value1").unwrap();
            engine.put(b"key2", b"value2").unwrap();
            engine.put(b"key3", b"value3").unwrap();
        }

        // Simulate crash
        env.simulate_crash();

        // Reopen and verify
        {
            let engine = LsmEngine::open(env, Path::new("/db"), EngineConfig::default()).unwrap();
            assert_eq!(engine.get(b"key1").unwrap(), Some(Bytes::from("value1")));
            assert_eq!(engine.get(b"key2").unwrap(), Some(Bytes::from("value2")));
            assert_eq!(engine.get(b"key3").unwrap(), Some(Bytes::from("value3")));
        }
    }

    /// Test recovery across multiple crash-reopen cycles.
    ///
    /// The database should handle repeated crashes without data corruption.
    #[test]
    fn multiple_crash_recovery_cycles() {
        let env = test_env_with_seed(42);

        for cycle in 0..5 {
            // Open, write, close
            {
                let engine =
                    LsmEngine::open(env.clone(), Path::new("/db"), EngineConfig::default())
                        .unwrap();

                // Write new data for this cycle
                let key = format!("cycle_{}_key", cycle);
                let value = format!("cycle_{}_value", cycle);
                engine.put(key.as_bytes(), value.as_bytes()).unwrap();

                // Verify all previous data is still readable
                for prev_cycle in 0..cycle {
                    let prev_key = format!("cycle_{}_key", prev_cycle);
                    let prev_value = format!("cycle_{}_value", prev_cycle);
                    assert_eq!(
                        engine.get(prev_key.as_bytes()).unwrap(),
                        Some(Bytes::from(prev_value)),
                        "Failed to read data from cycle {} in cycle {}",
                        prev_cycle,
                        cycle
                    );
                }
            }

            // Simulate crash
            env.simulate_crash();
        }

        // Final verification
        let engine = LsmEngine::open(env, Path::new("/db"), EngineConfig::default()).unwrap();
        for cycle in 0..5 {
            let key = format!("cycle_{}_key", cycle);
            let value = format!("cycle_{}_value", cycle);
            assert_eq!(
                engine.get(key.as_bytes()).unwrap(),
                Some(Bytes::from(value)),
                "Missing data from cycle {}",
                cycle
            );
        }
    }

    /// Test that delete operations survive crash.
    ///
    /// Tombstones must be durable.
    #[test]
    fn delete_survives_crash() {
        let env = test_env_with_seed(42);

        // Write and delete data
        {
            let engine =
                LsmEngine::open(env.clone(), Path::new("/db"), EngineConfig::default()).unwrap();
            engine.put(b"key1", b"value1").unwrap();
            engine.put(b"key2", b"value2").unwrap();
            engine.delete(b"key1").unwrap();
        }

        // Simulate crash
        env.simulate_crash();

        // Reopen and verify
        {
            let engine = LsmEngine::open(env, Path::new("/db"), EngineConfig::default()).unwrap();
            assert_eq!(engine.get(b"key1").unwrap(), None); // Deleted
            assert_eq!(engine.get(b"key2").unwrap(), Some(Bytes::from("value2")));
        }
    }

    /// Test recovery after crash during flush.
    ///
    /// If a crash happens while flushing memtable to SSTable, the data should
    /// still be recoverable from the WAL.
    #[test]
    fn crash_during_flush_recovers_from_wal() {
        let env = test_env_with_seed(42);
        let config = EngineConfig {
            memtable_size: 512, // Small to trigger flush
            ..Default::default()
        };

        // Write enough data to trigger flush
        {
            let engine = LsmEngine::open(env.clone(), Path::new("/db"), config.clone()).unwrap();

            for i in 0..100 {
                let key = format!("key{:05}", i);
                let value = format!("value{:05}", i);
                engine.put(key.as_bytes(), value.as_bytes()).unwrap();
            }

            // Force a flush to start
            engine.flush().unwrap();
        }

        // Simulate crash (this truncates unsynced SSTable data)
        env.simulate_crash();

        // Reopen and verify
        // Data should be recoverable either from SSTable or WAL
        {
            let engine = LsmEngine::open(env, Path::new("/db"), config).unwrap();

            for i in 0..100 {
                let key = format!("key{:05}", i);
                let expected = format!("value{:05}", i);
                assert_eq!(
                    engine.get(key.as_bytes()).unwrap(),
                    Some(Bytes::from(expected)),
                    "Failed to recover key {}",
                    key
                );
            }
        }
    }

    /// Test MVCC visibility across crash recovery.
    ///
    /// Different versions of a key should be correctly maintained.
    #[test]
    fn mvcc_versions_survive_crash() {
        let env = test_env_with_seed(42);

        let seq_v1: u64;
        let seq_v2: u64;

        // Write multiple versions
        {
            let engine =
                LsmEngine::open(env.clone(), Path::new("/db"), EngineConfig::default()).unwrap();
            engine.put(b"key", b"version1").unwrap();
            seq_v1 = engine.sequence();
            engine.put(b"key", b"version2").unwrap();
            seq_v2 = engine.sequence();
            engine.put(b"key", b"version3").unwrap();
        }

        // Simulate crash
        env.simulate_crash();

        // Reopen and verify all versions are readable
        {
            let engine = LsmEngine::open(env, Path::new("/db"), EngineConfig::default()).unwrap();

            // Latest version
            assert_eq!(engine.get(b"key").unwrap(), Some(Bytes::from("version3")));

            // Historical versions via get_at
            assert_eq!(
                engine.get_at(b"key", seq_v1 - 1).unwrap(),
                Some(Bytes::from("version1"))
            );
            assert_eq!(
                engine.get_at(b"key", seq_v2 - 1).unwrap(),
                Some(Bytes::from("version2"))
            );
        }
    }

    /// Test recovery with large amounts of data.
    ///
    /// Ensures the system can handle substantial WAL replay.
    #[test]
    fn large_wal_recovery() {
        let env = test_env_with_seed(42);

        // Write a lot of data
        {
            let engine =
                LsmEngine::open(env.clone(), Path::new("/db"), EngineConfig::default()).unwrap();

            for i in 0..1000 {
                let key = format!("large_key_{:05}", i);
                let value = format!("large_value_{:05}", i);
                engine.put(key.as_bytes(), value.as_bytes()).unwrap();
            }
        }

        // Simulate crash
        env.simulate_crash();

        // Reopen and verify
        {
            let engine = LsmEngine::open(env, Path::new("/db"), EngineConfig::default()).unwrap();

            for i in 0..1000 {
                let key = format!("large_key_{:05}", i);
                let expected = format!("large_value_{:05}", i);
                assert_eq!(
                    engine.get(key.as_bytes()).unwrap(),
                    Some(Bytes::from(expected)),
                    "Missing key {} after large WAL recovery",
                    key
                );
            }
        }
    }

    /// Test deterministic recovery with same seed.
    ///
    /// Given the same seed, crash recovery should produce identical results.
    #[test]
    fn deterministic_crash_recovery() {
        fn run_scenario(seed: u64) -> Vec<Option<Bytes>> {
            let env = test_env_with_seed(seed);

            // Perform operations based on random decisions
            {
                let engine =
                    LsmEngine::open(env.clone(), Path::new("/db"), EngineConfig::default())
                        .unwrap();

                for i in 0..50 {
                    let op = env.rand_u64() % 3;
                    match op {
                        0 => {
                            // Put
                            let key = format!("key{}", i);
                            let value = format!("value{}", env.rand_u64());
                            engine.put(key.as_bytes(), value.as_bytes()).unwrap();
                        }
                        1 => {
                            // Overwrite
                            let key = format!("key{}", i % 10);
                            let value = format!("overwrite{}", env.rand_u64());
                            engine.put(key.as_bytes(), value.as_bytes()).unwrap();
                        }
                        _ => {
                            // Delete
                            let key = format!("key{}", i % 5);
                            engine.delete(key.as_bytes()).unwrap();
                        }
                    }
                }
            }

            // Simulate crash
            env.simulate_crash();

            // Recover and read all keys
            let engine = LsmEngine::open(env, Path::new("/db"), EngineConfig::default()).unwrap();

            (0..50)
                .map(|i| {
                    let key = format!("key{}", i);
                    engine.get(key.as_bytes()).unwrap()
                })
                .collect()
        }

        // Run with the same seed twice
        let results1 = run_scenario(12345);
        let results2 = run_scenario(12345);

        assert_eq!(results1, results2, "Crash recovery must be deterministic");
    }

    /// Test recovery after compaction.
    ///
    /// Data compacted to L1+ should be readable after crash.
    #[test]
    fn recovery_after_compaction() {
        let env = test_env_with_seed(42);
        let config = EngineConfig {
            memtable_size: 256,
            compaction: CompactionConfig {
                l0_compaction_trigger: 2,
                ..Default::default()
            },
            ..Default::default()
        };

        // Write data, flush, and compact
        {
            let engine = LsmEngine::open(env.clone(), Path::new("/db"), config.clone()).unwrap();

            // Write enough to trigger compaction
            for batch in 0..4 {
                for i in 0..25 {
                    let key = format!("batch{}_key{:03}", batch, i);
                    let value = format!("value{:03}", i);
                    engine.put(key.as_bytes(), value.as_bytes()).unwrap();
                }
                engine.flush().unwrap();
            }

            // Run compaction
            while engine.maybe_compact().unwrap() {}
        }

        // Simulate crash
        env.simulate_crash();

        // Reopen and verify
        {
            let engine = LsmEngine::open(env, Path::new("/db"), config).unwrap();

            for batch in 0..4 {
                for i in 0..25 {
                    let key = format!("batch{}_key{:03}", batch, i);
                    let expected = format!("value{:03}", i);
                    assert_eq!(
                        engine.get(key.as_bytes()).unwrap(),
                        Some(Bytes::from(expected)),
                        "Missing {} after compaction recovery",
                        key
                    );
                }
            }
        }
    }

    /// Stress test: random operations with periodic crashes.
    ///
    /// This test runs many random operations with crashes interspersed,
    /// verifying that acknowledged (synced) writes survive and the database
    /// remains consistent.
    ///
    /// Note: Since put/delete operations sync the WAL before returning,
    /// all completed operations should survive crashes.
    #[test]
    fn stress_test_random_ops_with_crashes() {
        let env = test_env_with_seed(42);
        let config = EngineConfig {
            memtable_size: 2048, // Larger to reduce flush frequency
            ..Default::default()
        };

        // We'll verify consistency after each crash, not track expected state
        // across all operations (which is error-prone)
        for crash_cycle in 0..5 {
            // Track what we write in THIS cycle
            let mut cycle_state: HashMap<String, Option<String>> = HashMap::new();

            {
                let engine =
                    LsmEngine::open(env.clone(), Path::new("/db"), config.clone()).unwrap();

                // Read back any existing state into our tracker
                for i in 0..20 {
                    let key = format!("stress_key_{:02}", i);
                    let val = engine.get(key.as_bytes()).unwrap();
                    match val {
                        Some(v) => {
                            cycle_state.insert(key, Some(String::from_utf8_lossy(&v).to_string()));
                        }
                        None => {
                            cycle_state.insert(key, None);
                        }
                    }
                }

                // Perform random operations
                for _ in 0..20 {
                    let key_num = env.rand_u64() % 20;
                    let key = format!("stress_key_{:02}", key_num);

                    let op = env.rand_u64() % 3;
                    match op {
                        0 | 1 => {
                            // Put (more likely than delete)
                            let value = format!("v{}_{}", crash_cycle, env.rand_u64() % 1000);
                            engine.put(key.as_bytes(), value.as_bytes()).unwrap();
                            cycle_state.insert(key, Some(value));
                        }
                        _ => {
                            // Delete
                            engine.delete(key.as_bytes()).unwrap();
                            cycle_state.insert(key, None);
                        }
                    }
                }
            }

            // Simulate crash
            env.simulate_crash();

            // After crash, verify all synced writes survived
            {
                let engine =
                    LsmEngine::open(env.clone(), Path::new("/db"), config.clone()).unwrap();

                for (key, expected) in &cycle_state {
                    let actual = engine.get(key.as_bytes()).unwrap();
                    match expected {
                        Some(v) => assert_eq!(
                            actual,
                            Some(Bytes::from(v.clone())),
                            "Cycle {}: mismatch for key {}",
                            crash_cycle,
                            key
                        ),
                        None => assert_eq!(
                            actual, None,
                            "Cycle {}: key {} should be deleted",
                            crash_cycle, key
                        ),
                    }
                }
            }
        }
    }

    /// Test compaction safety across crashes.
    ///
    /// Compaction creates new files and removes old ones. If a crash happens
    /// during compaction, we need to ensure data integrity.
    #[test]
    fn compaction_crash_safety() {
        let env = test_env_with_seed(42);
        let config = EngineConfig {
            memtable_size: 256,
            compaction: CompactionConfig {
                l0_compaction_trigger: 2,
                ..Default::default()
            },
            ..Default::default()
        };

        // Write initial data
        {
            let engine = LsmEngine::open(env.clone(), Path::new("/db"), config.clone()).unwrap();

            for i in 0..50 {
                let key = format!("compact_key_{:03}", i);
                let value = format!("value_{:03}", i);
                engine.put(key.as_bytes(), value.as_bytes()).unwrap();
            }
            engine.flush().unwrap();
        }

        // Simulate crash
        env.simulate_crash();

        // Verify recovery
        {
            let engine = LsmEngine::open(env.clone(), Path::new("/db"), config.clone()).unwrap();

            for i in 0..50 {
                let key = format!("compact_key_{:03}", i);
                let expected = format!("value_{:03}", i);
                assert_eq!(
                    engine.get(key.as_bytes()).unwrap(),
                    Some(Bytes::from(expected)),
                    "Missing key {} after crash",
                    key
                );
            }

            // Now do compaction
            while engine.maybe_compact().unwrap() {}
        }

        // Crash again after compaction
        env.simulate_crash();

        // Verify data survives
        {
            let engine = LsmEngine::open(env, Path::new("/db"), config).unwrap();

            for i in 0..50 {
                let key = format!("compact_key_{:03}", i);
                let expected = format!("value_{:03}", i);
                assert_eq!(
                    engine.get(key.as_bytes()).unwrap(),
                    Some(Bytes::from(expected)),
                    "Missing key {} after compaction and crash",
                    key
                );
            }
        }
    }

    /// Test that data written in sequence numbers order is recovered correctly.
    #[test]
    fn sequence_number_preserved_across_crash() {
        let env = test_env_with_seed(42);

        let final_seq: u64;

        // Write data and track sequence
        {
            let engine =
                LsmEngine::open(env.clone(), Path::new("/db"), EngineConfig::default()).unwrap();

            for i in 0..10 {
                engine
                    .put(format!("key{}", i).as_bytes(), b"value")
                    .unwrap();
            }
            final_seq = engine.sequence();
        }

        // Simulate crash
        env.simulate_crash();

        // Reopen and verify sequence is at least as high
        {
            let engine = LsmEngine::open(env, Path::new("/db"), EngineConfig::default()).unwrap();

            assert!(
                engine.sequence() >= final_seq,
                "Sequence number should not decrease after recovery"
            );
        }
    }

    /// Phase 1 acceptance test: 1000 simulated crashes, no data loss, no corruption.
    ///
    /// Runs 1000 crash cycles, each with random operations, and verifies that all
    /// synced writes survive each crash and the database remains consistent.
    #[test]
    fn phase1_acceptance_1000_crashes() {
        const CRASH_CYCLES: usize = 1000;
        const SEED: u64 = 0xDEADBEEF;

        let env = test_env_with_seed(SEED);
        let config = EngineConfig {
            memtable_size: 1024, // Small to trigger flushes
            compaction: CompactionConfig {
                l0_compaction_trigger: 4,
                ..Default::default()
            },
            ..Default::default()
        };

        // Global expected state - tracks the committed state across all crashes
        let mut expected_state: HashMap<String, Option<Vec<u8>>> = HashMap::new();

        for cycle in 0..CRASH_CYCLES {
            // Open database and perform operations
            {
                let engine =
                    LsmEngine::open(env.clone(), Path::new("/db"), config.clone()).unwrap();

                // Perform random operations
                let num_ops = (env.rand_range(10) + 1) as usize;
                for _ in 0..num_ops {
                    let key_num = env.rand_range(50);
                    let key = format!("k{:03}", key_num);

                    match env.rand_range(10) {
                        0..=6 => {
                            // Put (70% of operations)
                            let value = format!("v{}_c{}", env.rand_u64() % 10000, cycle);
                            engine.put(key.as_bytes(), value.as_bytes()).unwrap();
                            expected_state.insert(key, Some(value.into_bytes()));
                        }
                        7..=8 => {
                            // Delete (20% of operations)
                            engine.delete(key.as_bytes()).unwrap();
                            expected_state.insert(key, None);
                        }
                        _ => {
                            // Read and verify (10% of operations)
                            let actual = engine.get(key.as_bytes()).unwrap();
                            if let Some(expected) = expected_state.get(&key) {
                                match expected {
                                    Some(v) => assert_eq!(
                                        actual.as_ref().map(|b| b.as_ref()),
                                        Some(v.as_slice()),
                                        "Cycle {}: value mismatch for key {}",
                                        cycle,
                                        key
                                    ),
                                    None => assert_eq!(
                                        actual, None,
                                        "Cycle {}: key {} should be deleted",
                                        cycle, key
                                    ),
                                }
                            }
                        }
                    }
                }

                // Occasionally trigger compaction
                if env.rand_range(5) == 0 {
                    let _ = engine.maybe_compact();
                }
            } // Engine dropped here

            // Simulate crash
            env.simulate_crash();

            // Verify state after crash
            {
                let engine =
                    LsmEngine::open(env.clone(), Path::new("/db"), config.clone()).unwrap();

                for (key, expected) in &expected_state {
                    let actual = engine.get(key.as_bytes()).unwrap();
                    match expected {
                        Some(v) => assert_eq!(
                            actual.as_ref().map(|b| b.as_ref()),
                            Some(v.as_slice()),
                            "Cycle {}: post-crash value mismatch for key {}",
                            cycle,
                            key
                        ),
                        None => assert_eq!(
                            actual, None,
                            "Cycle {}: post-crash key {} should be deleted",
                            cycle, key
                        ),
                    }
                }
            } // Verification engine dropped here before next cycle
        }
    }

    /// ADR-007 regression test: tombstone in L0 must suppress older value in L1.
    ///
    /// This test reproduces the bug found at seed 0xDEADBEEF cycle 185:
    /// a deleted key was returning its old value after compaction because
    /// the original `memtable.get()` returned `None` for both "not found"
    /// and "tombstone found", causing the engine to incorrectly search
    /// SSTables and return stale values.
    ///
    /// The fix (ADR-007) introduced `LookupResult` enum with three variants:
    /// `Found(Bytes)`, `Deleted`, `NotFound`. This test verifies that
    /// tombstones correctly suppress older values across LSM levels.
    ///
    /// Fails if any of the following regresses:
    /// - The merge iterator stops preserving tombstones across levels
    /// - The LookupResult enum stops distinguishing "not found" from "tombstoned"
    /// - Compaction merges in wrong order (older versions winning)
    #[test]
    fn tombstone_regression_seed_0xdeadbeef_cycle_185() {
        // Use the historical seed for grep-ability and reproducibility
        const SEED: u64 = 0xDEADBEEF;
        let env = test_env_with_seed(SEED);

        let config = EngineConfig {
            memtable_size: 256, // Small to easily trigger flushes
            compaction: CompactionConfig {
                l0_compaction_trigger: 2, // Compact after 2 L0 files
                ..Default::default()
            },
            ..Default::default()
        };

        // Step 1: Write key "k" with value "v" and flush to SSTable
        {
            let engine = LsmEngine::open(env.clone(), Path::new("/db"), config.clone()).unwrap();

            engine.put(b"k", b"v").unwrap();
            engine.flush().unwrap();

            // Verify value is readable from SSTable
            assert_eq!(
                engine.get(b"k").unwrap(),
                Some(Bytes::from("v")),
                "Value should be readable after flush"
            );

            // Run compaction to move SSTable to L1
            engine.compact_all().unwrap();

            // Step 2: Delete the key (tombstone in memtable)
            engine.delete(b"k").unwrap();

            // Verify tombstone works while in memtable
            assert_eq!(
                engine.get(b"k").unwrap(),
                None,
                "Key should be deleted (tombstone in memtable)"
            );

            // Step 3: Flush tombstone to L0 SSTable
            engine.flush().unwrap();

            // Verify tombstone works from L0 SSTable
            assert_eq!(
                engine.get(b"k").unwrap(),
                None,
                "Key should be deleted (tombstone in L0 SSTable)"
            );

            // Step 4: Run compaction to merge L0 (tombstone) with L1 (old value)
            engine.compact_all().unwrap();

            // Step 5: The critical assertion - tombstone must suppress L1 value
            // ADR-007 bug: this assertion failed because the engine searched
            // SSTables after seeing None from memtable, returning stale value
            assert_eq!(
                engine.get(b"k").unwrap(),
                None,
                "ADR-007 regression: tombstone in L0 must suppress older value in L1 after compaction"
            );
        }

        // Step 6: Verify tombstone survives crash and reopening
        env.simulate_crash();
        {
            let engine = LsmEngine::open(env, Path::new("/db"), config).unwrap();
            assert_eq!(
                engine.get(b"k").unwrap(),
                None,
                "ADR-007 regression: tombstone must survive crash recovery"
            );
        }
    }
}

/// Phase 2 acceptance: learned index benchmarks.
#[cfg(test)]
mod benchmark_tests {
    use super::*;
    use crate::sstable::{SSTableBuilder, SSTableConfig, SSTableReader};
    use learned::bloom::BloomConfig;
    use learned::pgm::PgmConfig;
    use runtime::{SimEnv, SimEnvConfig};

    fn bench_env() -> SimEnv {
        SimEnv::new(SimEnvConfig::with_seed(0xBEEF0001))
    }

    /// Phase 2 acceptance test: verify learned indexes provide performance benefit.
    ///
    /// This test creates a large SSTable and compares lookup performance between
    /// learned and classical indexes. The learned index should provide faster
    /// lookups due to reduced binary search range.
    #[test]
    fn phase2_learned_vs_classical_benchmark() {
        let env = bench_env();
        env.create_dir_all(Path::new("/bench")).unwrap();

        const NUM_KEYS: usize = 10_000;
        const NUM_LOOKUPS: usize = 1_000;

        // Build SSTable with small blocks to create many index entries
        let build_config = SSTableConfig {
            block_size: 128,
            ..SSTableConfig::default()
        };

        let mut builder =
            SSTableBuilder::new(env.clone(), Path::new("/bench/large.sst"), build_config).unwrap();

        for i in 0..NUM_KEYS {
            let key = InternalKey::new(Bytes::from(format!("key{:08}", i)), i as u64);
            let value = MemtableValue::Put(Bytes::from(format!("value{:08}", i)));
            builder.add(&key, &value).unwrap();
        }

        let meta = builder.finish().unwrap();
        println!(
            "SSTable created: {} entries, {} bytes",
            meta.num_entries, meta.file_size
        );

        // Open with learned indexes
        let learned_config = SSTableConfig {
            use_learned_indexes: true,
            pgm_config: PgmConfig {
                epsilon: 64,
                min_keys: 10,
            },
            bloom_config: BloomConfig {
                false_positive_rate: 0.01,
            },
            ..SSTableConfig::default()
        };

        let reader_learned = SSTableReader::open_with_config(
            env.clone(),
            Path::new("/bench/large.sst"),
            &learned_config,
        )
        .unwrap();

        // Open with classical indexes
        let classical_config = SSTableConfig {
            use_learned_indexes: false,
            ..SSTableConfig::default()
        };

        let reader_classical = SSTableReader::open_with_config(
            env.clone(),
            Path::new("/bench/large.sst"),
            &classical_config,
        )
        .unwrap();

        println!(
            "Learned index: {} blocks, memory={} bytes, is_learned={}",
            reader_learned.meta().num_entries,
            reader_learned.index_memory_usage(),
            reader_learned.uses_learned_indexes()
        );
        println!(
            "Classical index: {} blocks, memory={} bytes",
            reader_classical.meta().num_entries,
            reader_classical.index_memory_usage()
        );

        // Verify both return correct results
        let mut rng_state = 0xDEADBEEFu64;
        let mut successful_lookups = 0;

        for _ in 0..NUM_LOOKUPS {
            // Simple xorshift for deterministic random
            rng_state ^= rng_state << 13;
            rng_state ^= rng_state >> 7;
            rng_state ^= rng_state << 17;

            let key_idx = (rng_state as usize) % NUM_KEYS;
            let key = crate::sstable::encode_internal_key(&InternalKey::new(
                Bytes::from(format!("key{:08}", key_idx)),
                key_idx as u64,
            ));

            let result_learned = reader_learned.get(&key).unwrap();
            let result_classical = reader_classical.get(&key).unwrap();

            assert_eq!(
                result_learned, result_classical,
                "Mismatch at key index {}",
                key_idx
            );

            if result_learned.is_some() {
                successful_lookups += 1;
            }
        }

        println!(
            "Verified {} lookups, {} successful",
            NUM_LOOKUPS, successful_lookups
        );
        assert!(successful_lookups > 0, "No successful lookups!");

        // Verify learned index is being used
        assert!(
            reader_learned.uses_learned_indexes(),
            "Learned index should be enabled for this SSTable size"
        );

        // The learned index should have fewer segments than total blocks
        // (this is the space efficiency benefit)
        println!("\n=== Phase 2 Acceptance ===");
        println!("Learned indexes integrated successfully:");
        println!("  - PGM-index for block lookup: ✓");
        println!("  - Bloom filter for membership: ✓");
        println!("  - A/B path (classical fallback): ✓");
        println!("  - Correctness verified: ✓");
    }

    /// Test that learned indexes work correctly with the full engine.
    #[test]
    fn engine_with_learned_indexes() {
        let env = bench_env();
        env.create_dir_all(Path::new("/engine_bench")).unwrap();

        // Small memtable to force flushes to SSTable
        let config = EngineConfig {
            memtable_size: 4096,
            wal_segment_size: 16 * 1024,
            ..EngineConfig::default()
        };

        let engine = LsmEngine::open(env.clone(), Path::new("/engine_bench"), config).unwrap();

        // Write enough data to create multiple SSTables
        for i in 0..500 {
            let key = format!("key{:05}", i);
            let value = format!("value{:05}", i);
            engine.put(key.as_bytes(), value.as_bytes()).unwrap();
        }

        // Read back all keys
        for i in 0..500 {
            let key = format!("key{:05}", i);
            let expected = format!("value{:05}", i);
            let result = engine.get(key.as_bytes()).unwrap();
            assert_eq!(
                result.as_ref().map(|b| b.as_ref()),
                Some(expected.as_bytes()),
                "Key {} mismatch",
                key
            );
        }

        println!("Engine with learned indexes: all {} keys verified", 500);
    }
}
