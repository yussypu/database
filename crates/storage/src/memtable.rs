//! Memtable implementation using a concurrent skiplist.
//!
//! The memtable is an in-memory sorted data structure that buffers writes
//! before they are flushed to SSTables. It provides:
//!
//! - O(log n) point lookups
//! - O(log n) insertions
//! - Ordered iteration for range scans and flushing
//! - Thread-safe concurrent access
//!
//! # Lifecycle
//!
//! 1. Writes go to the active memtable
//! 2. When size threshold is reached, memtable becomes immutable
//! 3. Immutable memtable is flushed to L0 SSTable
//! 4. After flush completes, memtable is dropped

use bytes::Bytes;
use crossbeam_skiplist::SkipMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

/// A value in the memtable, which may be a put or delete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemtableValue {
    /// A put operation with the value.
    Put(Bytes),
    /// A delete (tombstone) marker.
    Delete,
}

impl MemtableValue {
    /// Returns the size of this value in bytes.
    pub fn size(&self) -> usize {
        match self {
            MemtableValue::Put(v) => v.len(),
            MemtableValue::Delete => 0,
        }
    }

    /// Returns true if this is a delete tombstone.
    pub fn is_delete(&self) -> bool {
        matches!(self, MemtableValue::Delete)
    }
}

/// Result of an MVCC-aware lookup in a memtable or SSTable.
///
/// This enum distinguishes between three cases:
/// - The key was found with a value
/// - The key was found but marked as deleted (tombstone)
/// - The key was not found in this data structure
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LookupResult {
    /// The key was found with this value.
    Found(Bytes),
    /// The key was found but has been deleted (tombstone).
    Deleted,
    /// The key was not found in this data structure.
    NotFound,
}

/// A key with a sequence number for MVCC ordering.
///
/// Keys are ordered first by user key (ascending), then by sequence number (descending).
/// This ensures that newer versions of a key appear before older versions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InternalKey {
    /// The user-provided key.
    pub user_key: Bytes,
    /// Sequence number (higher = newer).
    pub seq: u64,
}

impl InternalKey {
    /// Creates a new internal key.
    pub fn new(user_key: Bytes, seq: u64) -> Self {
        Self { user_key, seq }
    }

    /// Returns the size of this key in bytes.
    pub fn size(&self) -> usize {
        self.user_key.len() + 8 // 8 bytes for sequence number
    }
}

impl PartialOrd for InternalKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for InternalKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // First compare by user key (ascending)
        match self.user_key.cmp(&other.user_key) {
            std::cmp::Ordering::Equal => {
                // Then by sequence number (descending - newer first)
                other.seq.cmp(&self.seq)
            }
            other => other,
        }
    }
}

/// Default memtable size threshold (4 MB).
pub const DEFAULT_MEMTABLE_SIZE: usize = 4 * 1024 * 1024;

/// Configuration for memtables.
#[derive(Debug, Clone)]
pub struct MemtableConfig {
    /// Maximum size in bytes before the memtable should be flushed.
    pub max_size: usize,
}

impl Default for MemtableConfig {
    fn default() -> Self {
        Self {
            max_size: DEFAULT_MEMTABLE_SIZE,
        }
    }
}

/// A concurrent, sorted in-memory table.
///
/// Thread-safe for concurrent reads and writes.
pub struct Memtable {
    /// The underlying skiplist.
    map: SkipMap<InternalKey, MemtableValue>,
    /// Approximate size in bytes.
    size: AtomicUsize,
    /// Maximum size before flush.
    max_size: usize,
    /// Sequence number generator.
    next_seq: AtomicU64,
    /// Unique ID for this memtable.
    id: u64,
}

impl Memtable {
    /// Creates a new memtable with the given configuration.
    pub fn new(id: u64, config: MemtableConfig) -> Self {
        Self {
            map: SkipMap::new(),
            size: AtomicUsize::new(0),
            max_size: config.max_size,
            // Start at 1 so that seq=0 means "before any write"
            next_seq: AtomicU64::new(1),
            id,
        }
    }

    /// Creates a new memtable with default configuration.
    pub fn with_id(id: u64) -> Self {
        Self::new(id, MemtableConfig::default())
    }

    /// Returns the memtable's unique ID.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Inserts a key-value pair into the memtable.
    ///
    /// Returns the sequence number assigned to this write.
    pub fn put(&self, key: Bytes, value: Bytes) -> u64 {
        let seq = self.next_seq.fetch_add(1, Ordering::SeqCst);
        let internal_key = InternalKey::new(key.clone(), seq);

        let entry_size = internal_key.size() + value.len();
        self.size.fetch_add(entry_size, Ordering::Relaxed);

        self.map.insert(internal_key, MemtableValue::Put(value));
        seq
    }

    /// Inserts a key-value pair with a specific sequence number.
    ///
    /// Used during WAL replay.
    pub fn put_with_seq(&self, key: Bytes, value: Bytes, seq: u64) {
        let internal_key = InternalKey::new(key, seq);
        let entry_size = internal_key.size() + value.len();
        self.size.fetch_add(entry_size, Ordering::Relaxed);
        self.map.insert(internal_key, MemtableValue::Put(value));

        // Update next_seq if necessary
        loop {
            let current = self.next_seq.load(Ordering::Relaxed);
            if seq >= current {
                if self
                    .next_seq
                    .compare_exchange(current, seq + 1, Ordering::SeqCst, Ordering::Relaxed)
                    .is_ok()
                {
                    break;
                }
            } else {
                break;
            }
        }
    }

    /// Marks a key as deleted (tombstone).
    ///
    /// Returns the sequence number assigned to this delete.
    pub fn delete(&self, key: Bytes) -> u64 {
        let seq = self.next_seq.fetch_add(1, Ordering::SeqCst);
        let internal_key = InternalKey::new(key.clone(), seq);

        let entry_size = internal_key.size();
        self.size.fetch_add(entry_size, Ordering::Relaxed);

        self.map.insert(internal_key, MemtableValue::Delete);
        seq
    }

    /// Marks a key as deleted with a specific sequence number.
    ///
    /// Used during WAL replay.
    pub fn delete_with_seq(&self, key: Bytes, seq: u64) {
        let internal_key = InternalKey::new(key, seq);
        let entry_size = internal_key.size();
        self.size.fetch_add(entry_size, Ordering::Relaxed);
        self.map.insert(internal_key, MemtableValue::Delete);

        // Update next_seq if necessary
        loop {
            let current = self.next_seq.load(Ordering::Relaxed);
            if seq >= current {
                if self
                    .next_seq
                    .compare_exchange(current, seq + 1, Ordering::SeqCst, Ordering::Relaxed)
                    .is_ok()
                {
                    break;
                }
            } else {
                break;
            }
        }
    }

    /// Gets the most recent value for a key at or before the given sequence number.
    ///
    /// Returns `None` if the key doesn't exist or was deleted.
    pub fn get(&self, key: &[u8], read_seq: u64) -> Option<Bytes> {
        match self.lookup(key, read_seq) {
            LookupResult::Found(v) => Some(v),
            LookupResult::Deleted | LookupResult::NotFound => None,
        }
    }

    /// MVCC-aware lookup that distinguishes between found, deleted, and not found.
    ///
    /// This is the preferred method for MVCC reads as it allows the caller to
    /// stop searching lower levels when a tombstone is encountered.
    pub fn lookup(&self, key: &[u8], read_seq: u64) -> LookupResult {
        // We need to find the entry with:
        // - user_key == key
        // - seq <= read_seq
        // - highest seq among qualifying entries

        // Due to our ordering (user_key asc, seq desc), entries for the same
        // user_key are ordered with highest seq first. So we search forward
        // from (key, read_seq) and take the first entry that matches our key.

        // Search starting from (key, read_seq). Due to descending seq ordering,
        // (key, read_seq) will be positioned right before or at entries with
        // user_key == key and seq <= read_seq.
        let search_key = InternalKey::new(Bytes::copy_from_slice(key), read_seq);

        // Use range starting from search_key to find entries >= search_key
        for entry in self.map.range(search_key..) {
            if entry.key().user_key.as_ref() == key {
                // Found an entry for our key. Due to descending seq order,
                // this is the one with highest seq <= read_seq (or the first
                // entry with seq > read_seq if none qualify).
                if entry.key().seq <= read_seq {
                    return match entry.value() {
                        MemtableValue::Put(v) => LookupResult::Found(v.clone()),
                        MemtableValue::Delete => LookupResult::Deleted,
                    };
                }
                // seq > read_seq, keep looking
            } else {
                // Passed our key, no matching entry exists
                break;
            }
        }

        LookupResult::NotFound
    }

    /// Gets the most recent value for a key (at any sequence number).
    pub fn get_latest(&self, key: &[u8]) -> Option<Bytes> {
        self.get(key, u64::MAX)
    }

    /// Returns the approximate size of the memtable in bytes.
    pub fn approximate_size(&self) -> usize {
        self.size.load(Ordering::Relaxed)
    }

    /// Returns true if the memtable has reached its size threshold.
    pub fn should_flush(&self) -> bool {
        self.approximate_size() >= self.max_size
    }

    /// Returns the number of entries in the memtable.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Returns true if the memtable is empty.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Returns the next sequence number that will be assigned.
    pub fn next_seq(&self) -> u64 {
        self.next_seq.load(Ordering::SeqCst)
    }

    /// Checks if there are any writes for a key with sequence > ts.
    ///
    /// Used for SSI write-write conflict detection.
    pub fn has_write_after(&self, key: &[u8], ts: u64) -> bool {
        // Search for entries with matching user_key and seq > ts.
        // Due to our ordering (user_key asc, seq desc), we search starting
        // from (key, u64::MAX) to find entries for this key with highest seq first.
        let search_key = InternalKey::new(Bytes::copy_from_slice(key), u64::MAX);

        // We only need the first entry - if it matches our key, check its seq
        if let Some(entry) = self.map.range(search_key..).next() {
            if entry.key().user_key.as_ref() == key {
                // Found an entry for this key - check if seq > ts
                // Entries are in descending seq order, so if this seq <= ts,
                // all remaining entries for this key will also be <= ts
                return entry.key().seq > ts;
            }
        }

        false
    }

    /// Returns an iterator over all entries in sorted order.
    pub fn iter(&self) -> MemtableIterator<'_> {
        MemtableIterator {
            inner: self.map.iter(),
        }
    }

    /// Returns an iterator over entries in a key range.
    ///
    /// Note: This clones keys and values to avoid lifetime issues with the skiplist.
    pub fn range(
        &self,
        start: &[u8],
        end: &[u8],
    ) -> impl Iterator<Item = (InternalKey, MemtableValue)> + '_ {
        let start_key = InternalKey::new(Bytes::copy_from_slice(start), u64::MAX);
        let end_key = InternalKey::new(Bytes::copy_from_slice(end), 0);

        self.map
            .range(start_key..=end_key)
            .map(|e| (e.key().clone(), e.value().clone()))
    }
}

/// Iterator over memtable entries.
///
/// Clones keys and values due to crossbeam-skiplist lifetime constraints.
pub struct MemtableIterator<'a> {
    inner: crossbeam_skiplist::map::Iter<'a, InternalKey, MemtableValue>,
}

impl<'a> Iterator for MemtableIterator<'a> {
    type Item = (InternalKey, MemtableValue);

    fn next(&mut self) -> Option<Self::Item> {
        self.inner
            .next()
            .map(|e| (e.key().clone(), e.value().clone()))
    }
}

/// An immutable memtable that is being flushed.
///
/// This is a wrapper around a Memtable that prevents further modifications.
pub struct ImmutableMemtable {
    inner: Arc<Memtable>,
}

impl ImmutableMemtable {
    /// Creates a new immutable memtable from an existing memtable.
    pub fn new(memtable: Memtable) -> Self {
        Self {
            inner: Arc::new(memtable),
        }
    }

    /// Gets the most recent value for a key at or before the given sequence number.
    pub fn get(&self, key: &[u8], read_seq: u64) -> Option<Bytes> {
        self.inner.get(key, read_seq)
    }

    /// MVCC-aware lookup that distinguishes between found, deleted, and not found.
    pub fn lookup(&self, key: &[u8], read_seq: u64) -> LookupResult {
        self.inner.lookup(key, read_seq)
    }

    /// Checks if there are any writes for a key with sequence > ts.
    pub fn has_write_after(&self, key: &[u8], ts: u64) -> bool {
        self.inner.has_write_after(key, ts)
    }

    /// Returns an iterator over all entries.
    pub fn iter(&self) -> MemtableIterator<'_> {
        self.inner.iter()
    }

    /// Returns the memtable's ID.
    pub fn id(&self) -> u64 {
        self.inner.id()
    }

    /// Returns the approximate size.
    pub fn approximate_size(&self) -> usize {
        self.inner.approximate_size()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_put_get() {
        let mem = Memtable::with_id(1);
        mem.put(Bytes::from("key1"), Bytes::from("value1"));
        mem.put(Bytes::from("key2"), Bytes::from("value2"));

        assert_eq!(mem.get_latest(b"key1"), Some(Bytes::from("value1")));
        assert_eq!(mem.get_latest(b"key2"), Some(Bytes::from("value2")));
        assert_eq!(mem.get_latest(b"key3"), None);
    }

    #[test]
    fn overwrite_key() {
        let mem = Memtable::with_id(1);
        mem.put(Bytes::from("key"), Bytes::from("value1"));
        mem.put(Bytes::from("key"), Bytes::from("value2"));
        mem.put(Bytes::from("key"), Bytes::from("value3"));

        // Should get the most recent value
        assert_eq!(mem.get_latest(b"key"), Some(Bytes::from("value3")));

        // Should have 3 entries (all versions kept)
        assert_eq!(mem.len(), 3);
    }

    #[test]
    fn delete_key() {
        let mem = Memtable::with_id(1);
        mem.put(Bytes::from("key"), Bytes::from("value"));
        mem.delete(Bytes::from("key"));

        // Should return None (deleted)
        assert_eq!(mem.get_latest(b"key"), None);
    }

    #[test]
    fn mvcc_read_at_sequence() {
        let mem = Memtable::with_id(1);

        let seq1 = mem.put(Bytes::from("key"), Bytes::from("v1"));
        let seq2 = mem.put(Bytes::from("key"), Bytes::from("v2"));
        let seq3 = mem.put(Bytes::from("key"), Bytes::from("v3"));

        // Read at different sequence numbers
        assert_eq!(mem.get(b"key", seq1), Some(Bytes::from("v1")));
        assert_eq!(mem.get(b"key", seq2), Some(Bytes::from("v2")));
        assert_eq!(mem.get(b"key", seq3), Some(Bytes::from("v3")));

        // Read before any write
        assert_eq!(mem.get(b"key", 0), None);
    }

    #[test]
    fn mvcc_delete_at_sequence() {
        let mem = Memtable::with_id(1);

        let seq1 = mem.put(Bytes::from("key"), Bytes::from("value"));
        let seq2 = mem.delete(Bytes::from("key"));
        let seq3 = mem.put(Bytes::from("key"), Bytes::from("new_value"));

        assert_eq!(mem.get(b"key", seq1), Some(Bytes::from("value")));
        assert_eq!(mem.get(b"key", seq2), None); // Deleted
        assert_eq!(mem.get(b"key", seq3), Some(Bytes::from("new_value")));
    }

    #[test]
    fn iterator_order() {
        let mem = Memtable::with_id(1);

        // Insert in random order
        mem.put(Bytes::from("c"), Bytes::from("3"));
        mem.put(Bytes::from("a"), Bytes::from("1"));
        mem.put(Bytes::from("b"), Bytes::from("2"));

        // Iterator should return in sorted order
        let entries: Vec<_> = mem.iter().collect();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].0.user_key.as_ref(), b"a");
        assert_eq!(entries[1].0.user_key.as_ref(), b"b");
        assert_eq!(entries[2].0.user_key.as_ref(), b"c");
    }

    #[test]
    fn empty_memtable() {
        let mem = Memtable::with_id(1);
        assert!(mem.is_empty());
        assert_eq!(mem.len(), 0);
        assert_eq!(mem.get_latest(b"any"), None);
    }

    #[test]
    fn size_tracking() {
        let config = MemtableConfig { max_size: 100 };
        let mem = Memtable::new(1, config);

        assert!(!mem.should_flush());

        // Add some data
        for i in 0..20 {
            mem.put(
                Bytes::from(format!("key{:02}", i)),
                Bytes::from(format!("value{:02}", i)),
            );
        }

        assert!(mem.should_flush());
    }

    #[test]
    fn replay_with_seq() {
        let mem = Memtable::with_id(1);

        // Simulate WAL replay with specific sequence numbers
        mem.put_with_seq(Bytes::from("key1"), Bytes::from("v1"), 5);
        mem.put_with_seq(Bytes::from("key2"), Bytes::from("v2"), 10);
        mem.delete_with_seq(Bytes::from("key1"), 15);

        // Next seq should be higher than all replayed seqs
        assert!(mem.next_seq() > 15);

        // Values should be accessible
        assert_eq!(mem.get(b"key1", 10), Some(Bytes::from("v1")));
        assert_eq!(mem.get(b"key1", 20), None); // Deleted
        assert_eq!(mem.get(b"key2", 20), Some(Bytes::from("v2")));
    }
}
