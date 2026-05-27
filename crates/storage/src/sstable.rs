//! SSTable (Sorted String Table) implementation.
//!
//! SSTables are immutable sorted files that store key-value pairs.
//! They use block-based layout with optional learned indexes.
//!
//! # File Format
//!
//! ```text
//! +------------------+
//! | Data Block 0     |
//! +------------------+
//! | Data Block 1     |
//! +------------------+
//! | ...              |
//! +------------------+
//! | Data Block N     |
//! +------------------+
//! | Index Block      |
//! +------------------+
//! | Footer (48 bytes)|
//! +------------------+
//! ```
//!
//! # Data Block Format
//!
//! ```text
//! +------------------+
//! | Entry 0          |
//! | Entry 1          |
//! | ...              |
//! | Entry N          |
//! +------------------+
//! | Restart Point 0  | (4 bytes each)
//! | Restart Point 1  |
//! | ...              |
//! +------------------+
//! | Num Restarts     | (4 bytes)
//! +------------------+
//! | CRC32C           | (4 bytes)
//! +------------------+
//! ```
//!
//! Each entry uses prefix compression:
//! ```text
//! | shared_len (varint) | unshared_len (varint) | value_len (varint) |
//! | unshared_key_bytes | value_bytes |
//! ```

use crate::error::{Error, Result};
use crate::memtable::{InternalKey, MemtableValue};
use bytes::{BufMut, Bytes, BytesMut};
use learned::bloom::{BloomConfig, BloomFilter};
use learned::pgm::{key_to_u128, BlockIndex, PgmConfig};
use runtime::{Env, File, OpenOptions, Path};

/// Default block size (4 KB).
pub const DEFAULT_BLOCK_SIZE: usize = 4 * 1024;

/// Restart interval for prefix compression.
const RESTART_INTERVAL: usize = 16;

/// Magic number for SSTable footer.
/// Changed from 0x88e241b785f4cff7 to invalidate old format without bloom filter.
const FOOTER_MAGIC: u64 = 0x88e241b785f4cff8;

/// Footer size in bytes.
const FOOTER_SIZE: usize = 48;

/// Configuration for SSTable creation.
#[derive(Debug, Clone)]
pub struct SSTableConfig {
    /// Target block size in bytes.
    pub block_size: usize,
    /// Whether to use learned indexes (PGM for block lookup, learned Bloom filter).
    /// If false, uses classical binary search and standard Bloom filter.
    pub use_learned_indexes: bool,
    /// Configuration for the PGM-index.
    pub pgm_config: PgmConfig,
    /// Configuration for the Bloom filter.
    pub bloom_config: BloomConfig,
}

impl Default for SSTableConfig {
    fn default() -> Self {
        Self {
            block_size: DEFAULT_BLOCK_SIZE,
            use_learned_indexes: true,
            pgm_config: PgmConfig::default(),
            bloom_config: BloomConfig::default(),
        }
    }
}

/// Builder for creating SSTables.
pub struct SSTableBuilder<E: Env> {
    _env: E,
    file: E::File,
    config: SSTableConfig,
    /// Current data block being built.
    data_block: BlockBuilder,
    /// Index entries: (first_key, block_offset, block_size).
    index_entries: Vec<(Bytes, u64, u32)>,
    /// Current file offset.
    offset: u64,
    /// Last key written (for ordering validation).
    last_key: Option<Bytes>,
    /// Number of entries written.
    num_entries: u64,
    /// First key in the SSTable.
    first_key: Option<Bytes>,
    /// Last key in the SSTable.
    last_key_final: Option<Bytes>,
    /// Bloom filter built from ALL keys.
    bloom_filter: BloomFilter,
}

impl<E: Env> SSTableBuilder<E> {
    /// Creates a new SSTable builder.
    pub fn new(env: E, path: &Path, config: SSTableConfig) -> Result<Self> {
        let file = env.open(path, OpenOptions::create_new())?;
        // Start with bloom filter sized for estimated keys (will resize if needed)
        let bloom_filter = BloomFilter::new(1000, &config.bloom_config);
        Ok(Self {
            _env: env,
            file,
            config,
            data_block: BlockBuilder::new(RESTART_INTERVAL),
            index_entries: Vec::new(),
            offset: 0,
            last_key: None,
            num_entries: 0,
            first_key: None,
            last_key_final: None,
            bloom_filter,
        })
    }

    /// Adds a key-value pair to the SSTable.
    ///
    /// Keys must be added in sorted order.
    pub fn add(&mut self, key: &InternalKey, value: &MemtableValue) -> Result<()> {
        let key_bytes = encode_internal_key(key);

        // Validate ordering
        if let Some(ref last) = self.last_key {
            if key_bytes.as_ref() <= last.as_ref() {
                return Err(Error::InvalidArgument(
                    "keys must be added in sorted order".to_string(),
                ));
            }
        }

        // Track first and last keys
        if self.first_key.is_none() {
            self.first_key = Some(key_bytes.clone());
        }
        self.last_key_final = Some(key_bytes.clone());

        // Encode value
        let value_bytes = encode_value(value);

        // Check if we need to flush the current block
        let entry_size = key_bytes.len() + value_bytes.len() + 10; // rough estimate
        if self.data_block.estimated_size() + entry_size > self.config.block_size
            && !self.data_block.is_empty()
        {
            self.flush_data_block()?;
        }

        // Add to current block
        self.data_block.add(&key_bytes, &value_bytes);

        // Add user key to bloom filter (strip 8-byte seq suffix for user-key lookups)
        // This allows may_contain(user_key) checks during reads
        self.bloom_filter.insert(&key.user_key);

        self.last_key = Some(key_bytes);
        self.num_entries += 1;

        Ok(())
    }

    /// Finishes building the SSTable and returns its metadata.
    pub fn finish(mut self) -> Result<SSTableMeta> {
        // Flush any remaining data
        if !self.data_block.is_empty() {
            self.flush_data_block()?;
        }

        // Write index block
        let index_offset = self.offset;
        let index_data = self.build_index_block();
        self.file.write_all_at(&index_data, self.offset)?;
        self.offset += index_data.len() as u64;

        // Write bloom filter
        let bloom_offset = self.offset;
        let bloom_data = self.bloom_filter.to_bytes();
        self.file.write_all_at(&bloom_data, self.offset)?;
        self.offset += bloom_data.len() as u64;

        // Write footer
        let footer = self.build_footer(
            index_offset,
            index_data.len() as u32,
            bloom_offset,
            bloom_data.len() as u32,
        );
        self.file.write_all_at(&footer, self.offset)?;

        // Sync to disk
        self.file.sync()?;

        Ok(SSTableMeta {
            num_entries: self.num_entries,
            file_size: self.offset + FOOTER_SIZE as u64,
            first_key: self.first_key,
            last_key: self.last_key_final,
        })
    }

    fn flush_data_block(&mut self) -> Result<()> {
        let first_key = self
            .data_block
            .first_key()
            .ok_or_else(|| Error::InvalidArgument("empty block".to_string()))?
            .clone();

        let block_data = self.data_block.finish();
        let block_size = block_data.len() as u32;

        // Write block to file
        self.file.write_all_at(&block_data, self.offset)?;

        // Record index entry
        self.index_entries
            .push((first_key, self.offset, block_size));

        self.offset += block_data.len() as u64;
        self.data_block = BlockBuilder::new(RESTART_INTERVAL);

        Ok(())
    }

    fn build_index_block(&self) -> Vec<u8> {
        let mut builder = BlockBuilder::new(1); // No prefix compression for index

        for (key, offset, size) in &self.index_entries {
            // Value is offset (8 bytes) + size (4 bytes)
            let mut value = Vec::with_capacity(12);
            value.extend_from_slice(&offset.to_le_bytes());
            value.extend_from_slice(&size.to_le_bytes());
            builder.add(key, &value);
        }

        builder.finish()
    }

    fn build_footer(
        &self,
        index_offset: u64,
        index_size: u32,
        bloom_offset: u64,
        bloom_size: u32,
    ) -> Vec<u8> {
        let mut footer = Vec::with_capacity(FOOTER_SIZE);

        // Index block handle (offset + size)
        footer.extend_from_slice(&index_offset.to_le_bytes()); // 0-7
        footer.extend_from_slice(&index_size.to_le_bytes()); // 8-11

        // Bloom filter handle (offset + size)
        footer.extend_from_slice(&bloom_offset.to_le_bytes()); // 12-19
        footer.extend_from_slice(&bloom_size.to_le_bytes()); // 20-23

        // Padding to 40 bytes
        footer.resize(40, 0); // 24-39

        // Magic number
        footer.extend_from_slice(&FOOTER_MAGIC.to_le_bytes()); // 40-47

        footer
    }
}

/// Metadata about an SSTable.
#[derive(Debug, Clone)]
pub struct SSTableMeta {
    /// Number of entries in the SSTable.
    pub num_entries: u64,
    /// Total file size in bytes.
    pub file_size: u64,
    /// First key in the SSTable.
    pub first_key: Option<Bytes>,
    /// Last key in the SSTable.
    pub last_key: Option<Bytes>,
}

/// Reader for SSTables.
pub struct SSTableReader<E: Env> {
    _env: E,
    file: E::File,
    _file_size: u64,
    /// Index block data.
    index_block: Block,
    /// Cached index entries for direct access: (first_key, offset, size).
    index_entries: Vec<(Bytes, u64, u32)>,
    /// Learned index for fast block lookup.
    block_index: BlockIndex,
    /// Bloom filter for membership testing (built from ALL keys, stored in SSTable).
    bloom_filter: BloomFilter,
    /// Cached metadata.
    meta: SSTableMeta,
}

impl<E: Env> SSTableReader<E> {
    /// Opens an existing SSTable for reading with default config.
    pub fn open(env: E, path: &Path) -> Result<Self> {
        Self::open_with_config(env, path, &SSTableConfig::default())
    }

    /// Opens an existing SSTable for reading with custom config.
    pub fn open_with_config(env: E, path: &Path, config: &SSTableConfig) -> Result<Self> {
        let file = env.open(path, OpenOptions::read())?;
        let file_size = file.len()?;

        if file_size < FOOTER_SIZE as u64 {
            return Err(Error::Corruption("file too small for footer".to_string()));
        }

        // Read footer
        let mut footer = [0u8; FOOTER_SIZE];
        file.read_exact_at(&mut footer, file_size - FOOTER_SIZE as u64)?;

        // Verify magic
        let magic = u64::from_le_bytes(footer[40..48].try_into().unwrap());
        if magic != FOOTER_MAGIC {
            return Err(Error::Corruption("invalid footer magic".to_string()));
        }

        // Parse index handle
        let index_offset = u64::from_le_bytes(footer[0..8].try_into().unwrap());
        let index_size = u32::from_le_bytes(footer[8..12].try_into().unwrap());

        // Parse bloom filter handle
        let bloom_offset = u64::from_le_bytes(footer[12..20].try_into().unwrap());
        let bloom_size = u32::from_le_bytes(footer[20..24].try_into().unwrap());

        // Read index block
        let mut index_data = vec![0u8; index_size as usize];
        file.read_exact_at(&mut index_data, index_offset)?;
        let index_block = Block::new(index_data)?;

        // Read bloom filter
        let bloom_filter = if bloom_size > 0 {
            let mut bloom_data = vec![0u8; bloom_size as usize];
            file.read_exact_at(&mut bloom_data, bloom_offset)?;
            BloomFilter::from_bytes(&bloom_data)
                .ok_or_else(|| Error::Corruption("invalid bloom filter data".to_string()))?
        } else {
            // Fallback for empty bloom filter
            BloomFilter::new(1, &config.bloom_config)
        };

        // Extract index entries and build learned indexes
        let mut first_key = None;
        let mut last_key = None;
        let mut index_entries = Vec::new();
        let mut block_keys: Vec<u128> = Vec::new();

        for (key, value) in index_block.iter() {
            if first_key.is_none() {
                first_key = Some(key.clone());
            }
            last_key = Some(key.clone());

            let offset = u64::from_le_bytes(value[0..8].try_into().unwrap());
            let size = u32::from_le_bytes(value[8..12].try_into().unwrap());
            index_entries.push((key.clone(), offset, size));

            // Convert key to u128 for the PGM-index (better prefix diversity)
            block_keys.push(key_to_u128(&key));
        }

        // Build PGM-index for block lookups
        let block_index = if config.use_learned_indexes {
            BlockIndex::build(&block_keys, &config.pgm_config)
        } else {
            BlockIndex::BinarySearch {
                num_blocks: block_keys.len(),
            }
        };

        let num_entries = index_entries.len() as u64;

        Ok(Self {
            _env: env,
            file,
            _file_size: file_size,
            index_block,
            index_entries,
            block_index,
            bloom_filter,
            meta: SSTableMeta {
                num_entries,
                file_size,
                first_key,
                last_key,
            },
        })
    }

    /// Returns the SSTable metadata.
    pub fn meta(&self) -> &SSTableMeta {
        &self.meta
    }

    /// Gets a value by key (exact match on full internal key).
    pub fn get(&self, key: &[u8]) -> Result<Option<Bytes>> {
        // Find the block that might contain the key
        let block_handle = self.find_block_for_key(key)?;

        if let Some((offset, size)) = block_handle {
            // Read and search the block
            let mut block_data = vec![0u8; size as usize];
            self.file.read_exact_at(&mut block_data, offset)?;
            let block = Block::new(block_data)?;

            // Search for key in block
            for (k, v) in block.iter() {
                match k.as_ref().cmp(key) {
                    std::cmp::Ordering::Equal => return Ok(Some(v)),
                    std::cmp::Ordering::Greater => return Ok(None),
                    std::cmp::Ordering::Less => continue,
                }
            }
        }

        Ok(None)
    }

    /// Gets a value by user key and sequence number (MVCC-aware lookup).
    ///
    /// Finds the entry with matching user key and highest sequence number <= target_seq.
    /// This is the method that should be used for LSM lookups with MVCC.
    pub fn get_mvcc(&self, user_key: &[u8], target_seq: u64) -> Result<Option<Bytes>> {
        // Keys are stored with inverted seq in big-endian, so lexicographic order gives
        // (user_key ASC, seq DESC) which matches InternalKey ordering.
        // For the same user_key, higher seq entries come first in iteration.

        // Find all blocks that might contain this user key
        let mut result: Option<(u64, Bytes)> = None;

        for (index_key, value) in self.index_block.iter() {
            // Check if this block could contain our user key
            // Index key is first key of block (user_key + inverted_seq_be)
            if index_key.len() < 8 {
                continue;
            }
            let block_user_key = &index_key[..index_key.len() - 8];

            // If block's first user_key is greater than our search key, we can stop
            if block_user_key > user_key {
                break;
            }

            let offset = u64::from_le_bytes(value[0..8].try_into().unwrap());
            let size = u32::from_le_bytes(value[8..12].try_into().unwrap());

            // Read and search this block
            let mut block_data = vec![0u8; size as usize];
            self.file.read_exact_at(&mut block_data, offset)?;
            let block = Block::new(block_data)?;

            for (k, v) in block.iter() {
                if k.len() < 8 {
                    continue;
                }
                let entry_user_key = &k[..k.len() - 8];
                // Decode inverted big-endian sequence
                let inverted_seq = u64::from_be_bytes(k[k.len() - 8..].try_into().unwrap());
                let entry_seq = u64::MAX - inverted_seq;

                if entry_user_key == user_key && entry_seq <= target_seq {
                    // Found a matching entry - keep the one with highest seq
                    // Due to sorted order (seq DESC), the first match for a user_key
                    // has the highest seq, so we can return immediately
                    match &result {
                        None => result = Some((entry_seq, v)),
                        Some((best_seq, _)) if entry_seq > *best_seq => {
                            result = Some((entry_seq, v));
                        }
                        _ => {}
                    }
                } else if entry_user_key > user_key {
                    // Passed our key, can stop searching this block
                    break;
                }
            }
        }

        Ok(result.map(|(_, v)| v))
    }

    /// Returns an iterator over all entries.
    pub fn iter(&self) -> Result<SSTableIterator<E>> {
        SSTableIterator::new(self)
    }

    fn find_block_for_key(&self, key: &[u8]) -> Result<Option<(u64, u32)>> {
        if self.index_entries.is_empty() {
            return Ok(None);
        }

        // Use PGM-index to get the search range
        let (pgm_lo, pgm_hi) = self.block_index.search_bytes(key);
        let pgm_hi = pgm_hi.min(self.index_entries.len());

        // For PGM-predicted range: check if the range actually contains our key
        // If the first key in the range is > our key, PGM prediction is too high
        // If the last key in the range is < our key, PGM prediction is too low
        // In these cases, fall back to full binary search
        let (search_lo, search_hi) = if pgm_lo < pgm_hi {
            // Check if the predicted range is plausible
            let first_in_range = &self.index_entries[pgm_lo].0;
            let last_in_range = &self.index_entries[pgm_hi - 1].0;

            // Expand search range if PGM prediction seems off
            // This handles edge cases where key_to_u128 still loses information
            // (e.g., all keys have same first 16 bytes - rare but possible)
            if first_in_range.as_ref() > key {
                // Key is before the predicted range - need to search earlier
                (0, pgm_hi)
            } else if last_in_range.as_ref() < key && pgm_hi < self.index_entries.len() {
                // Key might be after the predicted range
                (pgm_lo, self.index_entries.len())
            } else {
                // PGM range looks plausible, but expand slightly for safety
                let expanded_lo = pgm_lo.saturating_sub(1);
                let expanded_hi = (pgm_hi + 1).min(self.index_entries.len());
                (expanded_lo, expanded_hi)
            }
        } else {
            // Empty PGM range, search everything
            (0, self.index_entries.len())
        };

        // Binary search within the determined range
        // We want the largest block index where block_first_key <= key
        let search_range = &self.index_entries[search_lo..search_hi];

        let mut result = None;
        let mut lo = 0;
        let mut hi = search_range.len();

        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let (ref block_key, _, _) = search_range[mid];

            if block_key.as_ref() <= key {
                // This block could contain the key
                result = Some((search_range[mid].1, search_range[mid].2));
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }

        Ok(result)
    }

    /// Tests if a key might exist in this SSTable using the Bloom filter.
    ///
    /// Returns `false` if the key definitely does not exist.
    /// Returns `true` if the key might exist (possible false positive).
    #[inline]
    pub fn may_contain(&self, key: &[u8]) -> bool {
        self.bloom_filter.may_contain(key)
    }

    /// Returns true if using learned indexes.
    pub fn uses_learned_indexes(&self) -> bool {
        self.block_index.is_learned()
    }

    /// Returns the memory usage of the learned indexes.
    pub fn index_memory_usage(&self) -> usize {
        self.block_index.memory_usage() + self.bloom_filter.memory_usage()
    }
}

/// Iterator over SSTable entries.
pub struct SSTableIterator<'a, E: Env> {
    reader: &'a SSTableReader<E>,
    /// Iterator over the index block entries.
    index_iter: BlockIterator,
    /// Iterator over current data block (owns its data via Bytes).
    block_iter: Option<BlockIterator>,
}

impl<'a, E: Env> SSTableIterator<'a, E> {
    fn new(reader: &'a SSTableReader<E>) -> Result<Self> {
        let index_iter = reader.index_block.iter();
        Ok(Self {
            reader,
            index_iter,
            block_iter: None,
        })
    }

    fn load_next_block(&mut self) -> Result<bool> {
        if let Some((_, value)) = self.index_iter.next() {
            let offset = u64::from_le_bytes(value[0..8].try_into().unwrap());
            let size = u32::from_le_bytes(value[8..12].try_into().unwrap());

            let mut block_data = vec![0u8; size as usize];
            self.reader.file.read_exact_at(&mut block_data, offset)?;
            let block = Block::new(block_data)?;

            // BlockIterator now owns its data via Bytes (cheap clone)
            self.block_iter = Some(block.iter());
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

impl<'a, E: Env> Iterator for SSTableIterator<'a, E> {
    type Item = Result<(Bytes, Bytes)>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            // Try to get next entry from current block
            if let Some(ref mut iter) = self.block_iter {
                if let Some((k, v)) = iter.next() {
                    return Some(Ok((k, v)));
                }
            }

            // Load next block
            match self.load_next_block() {
                Ok(true) => continue,
                Ok(false) => return None,
                Err(e) => return Some(Err(e)),
            }
        }
    }
}

/// Builder for data blocks with prefix compression.
struct BlockBuilder {
    /// Buffer for block data.
    buffer: Vec<u8>,
    /// Restart points (offsets into buffer).
    restarts: Vec<u32>,
    /// Restart interval.
    restart_interval: usize,
    /// Counter for restart interval.
    counter: usize,
    /// Last key added.
    last_key: Vec<u8>,
    /// First key in the block.
    first_key: Option<Bytes>,
}

impl BlockBuilder {
    fn new(restart_interval: usize) -> Self {
        let restarts = vec![0]; // First entry is always a restart point

        Self {
            buffer: Vec::new(),
            restarts,
            restart_interval,
            counter: 0,
            last_key: Vec::new(),
            first_key: None,
        }
    }

    fn add(&mut self, key: &[u8], value: &[u8]) {
        // Track first key
        if self.first_key.is_none() {
            self.first_key = Some(Bytes::copy_from_slice(key));
        }

        // Calculate shared prefix length
        let shared = if self.counter < self.restart_interval {
            shared_prefix_len(&self.last_key, key)
        } else {
            // New restart point
            self.restarts.push(self.buffer.len() as u32);
            self.counter = 0;
            0
        };

        let unshared = key.len() - shared;

        // Write entry: shared_len, unshared_len, value_len, unshared_key, value
        put_varint(&mut self.buffer, shared as u64);
        put_varint(&mut self.buffer, unshared as u64);
        put_varint(&mut self.buffer, value.len() as u64);
        self.buffer.extend_from_slice(&key[shared..]);
        self.buffer.extend_from_slice(value);

        // Update state
        self.last_key.clear();
        self.last_key.extend_from_slice(key);
        self.counter += 1;
    }

    fn finish(&mut self) -> Vec<u8> {
        // Write restart points
        for &restart in &self.restarts {
            self.buffer.extend_from_slice(&restart.to_le_bytes());
        }

        // Write number of restarts
        self.buffer
            .extend_from_slice(&(self.restarts.len() as u32).to_le_bytes());

        // Compute and write CRC
        let crc = crc32c::crc32c(&self.buffer);
        self.buffer.extend_from_slice(&crc.to_le_bytes());

        std::mem::take(&mut self.buffer)
    }

    fn estimated_size(&self) -> usize {
        self.buffer.len() + self.restarts.len() * 4 + 8
    }

    fn is_empty(&self) -> bool {
        self.first_key.is_none()
    }

    fn first_key(&self) -> Option<&Bytes> {
        self.first_key.as_ref()
    }
}

/// A data block that has been read from disk.
struct Block {
    data: Bytes,
    restarts_offset: usize,
    _num_restarts: usize,
}

impl Block {
    fn new(data: Vec<u8>) -> Result<Self> {
        if data.len() < 8 {
            return Err(Error::Corruption("block too small".to_string()));
        }

        // Verify CRC
        let crc_offset = data.len() - 4;
        let stored_crc = u32::from_le_bytes(data[crc_offset..].try_into().unwrap());
        let computed_crc = crc32c::crc32c(&data[..crc_offset]);

        if stored_crc != computed_crc {
            return Err(Error::Corruption(format!(
                "block CRC mismatch: stored={}, computed={}",
                stored_crc, computed_crc
            )));
        }

        // Parse footer
        let num_restarts_offset = data.len() - 8;
        let num_restarts = u32::from_le_bytes(
            data[num_restarts_offset..num_restarts_offset + 4]
                .try_into()
                .unwrap(),
        ) as usize;

        let restarts_offset = num_restarts_offset - num_restarts * 4;

        Ok(Self {
            data: Bytes::from(data),
            restarts_offset,
            _num_restarts: num_restarts,
        })
    }

    fn iter(&self) -> BlockIterator {
        BlockIterator {
            data: self.data.clone(), // Bytes clones are cheap (Arc-based)
            restarts_offset: self.restarts_offset,
            offset: 0,
            current_key: Vec::new(),
        }
    }
}

/// Iterator over block entries.
///
/// Owns its data via `Bytes` for cheap cloning without lifetime issues.
struct BlockIterator {
    data: Bytes,
    restarts_offset: usize,
    offset: usize,
    current_key: Vec<u8>,
}

impl Iterator for BlockIterator {
    type Item = (Bytes, Bytes);

    fn next(&mut self) -> Option<Self::Item> {
        if self.offset >= self.restarts_offset {
            return None;
        }

        // Parse entry
        let (shared, n1) = get_varint(&self.data[self.offset..])?;
        let (unshared, n2) = get_varint(&self.data[self.offset + n1..])?;
        let (value_len, n3) = get_varint(&self.data[self.offset + n1 + n2..])?;

        let key_start = self.offset + n1 + n2 + n3;
        let key_end = key_start + unshared as usize;
        let value_start = key_end;
        let value_end = value_start + value_len as usize;

        if value_end > self.restarts_offset {
            return None;
        }

        // Build full key
        self.current_key.truncate(shared as usize);
        self.current_key
            .extend_from_slice(&self.data[key_start..key_end]);

        let key = Bytes::copy_from_slice(&self.current_key);
        let value = Bytes::copy_from_slice(&self.data[value_start..value_end]);

        self.offset = value_end;
        Some((key, value))
    }
}

// Helper functions

fn shared_prefix_len(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b.iter()).take_while(|(x, y)| x == y).count()
}

fn put_varint(buf: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        buf.push((value as u8) | 0x80);
        value >>= 7;
    }
    buf.push(value as u8);
}

fn get_varint(data: &[u8]) -> Option<(u64, usize)> {
    let mut value = 0u64;
    let mut shift = 0;

    for (i, &byte) in data.iter().enumerate() {
        value |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            return Some((value, i + 1));
        }
        shift += 7;
        if shift > 63 {
            return None;
        }
    }
    None
}

/// Encodes an internal key to bytes.
///
/// The encoding is designed so that lexicographic byte ordering matches
/// InternalKey ordering (user_key ASC, seq DESC).
///
/// Format: user_key || inverted_seq_be
///
/// The sequence number is inverted (u64::MAX - seq) and stored in big-endian
/// so that higher sequence numbers sort to smaller byte values.
pub fn encode_internal_key(key: &InternalKey) -> Bytes {
    let mut buf = BytesMut::with_capacity(key.user_key.len() + 8);
    buf.put_slice(&key.user_key);
    // Invert seq so higher seq = smaller bytes (for DESC ordering)
    buf.put_u64(u64::MAX - key.seq); // Big-endian
    buf.freeze()
}

/// Decodes an internal key from bytes.
pub fn decode_internal_key(data: &[u8]) -> Option<InternalKey> {
    if data.len() < 8 {
        return None;
    }
    let user_key = Bytes::copy_from_slice(&data[..data.len() - 8]);
    // Reverse the inversion: seq = u64::MAX - stored_value
    let inverted_seq = u64::from_be_bytes(data[data.len() - 8..].try_into().ok()?);
    let seq = u64::MAX - inverted_seq;
    Some(InternalKey::new(user_key, seq))
}

/// Encodes a memtable value to bytes.
pub fn encode_value(value: &MemtableValue) -> Bytes {
    match value {
        MemtableValue::Put(v) => {
            let mut buf = BytesMut::with_capacity(1 + v.len());
            buf.put_u8(1); // Type: Put
            buf.put_slice(v);
            buf.freeze()
        }
        MemtableValue::Delete => Bytes::from_static(&[0]), // Type: Delete
    }
}

/// Decodes a value from bytes.
pub fn decode_value(data: &[u8]) -> Option<MemtableValue> {
    if data.is_empty() {
        return None;
    }
    match data[0] {
        0 => Some(MemtableValue::Delete),
        1 => Some(MemtableValue::Put(Bytes::copy_from_slice(&data[1..]))),
        _ => None,
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
    fn build_and_read_sstable() {
        let env = test_env();
        env.create_dir_all(Path::new("/data")).unwrap();

        // Build SSTable
        let mut builder = SSTableBuilder::new(
            env.clone(),
            Path::new("/data/test.sst"),
            SSTableConfig::default(),
        )
        .unwrap();

        for i in 0..100 {
            let key = InternalKey::new(Bytes::from(format!("key{:05}", i)), i as u64);
            let value = MemtableValue::Put(Bytes::from(format!("value{:05}", i)));
            builder.add(&key, &value).unwrap();
        }

        let meta = builder.finish().unwrap();
        assert_eq!(meta.num_entries, 100);

        // Read SSTable
        let reader = SSTableReader::open(env, Path::new("/data/test.sst")).unwrap();

        // Point lookup
        let key = encode_internal_key(&InternalKey::new(Bytes::from("key00050"), 50));
        let value = reader.get(&key).unwrap();
        assert!(value.is_some());
        let decoded = decode_value(&value.unwrap()).unwrap();
        assert_eq!(decoded, MemtableValue::Put(Bytes::from("value00050")));

        // Non-existent key
        let key = encode_internal_key(&InternalKey::new(Bytes::from("nonexistent"), 0));
        assert!(reader.get(&key).unwrap().is_none());
    }

    #[test]
    fn sstable_iteration() {
        let env = test_env();
        env.create_dir_all(Path::new("/data")).unwrap();

        // Build SSTable
        let mut builder = SSTableBuilder::new(
            env.clone(),
            Path::new("/data/iter.sst"),
            SSTableConfig::default(),
        )
        .unwrap();

        for i in 0..50 {
            let key = InternalKey::new(Bytes::from(format!("k{:03}", i)), i as u64);
            let value = MemtableValue::Put(Bytes::from(format!("v{:03}", i)));
            builder.add(&key, &value).unwrap();
        }

        builder.finish().unwrap();

        // Iterate
        let reader = SSTableReader::open(env, Path::new("/data/iter.sst")).unwrap();
        let entries: Vec<_> = reader.iter().unwrap().collect();

        assert_eq!(entries.len(), 50);
        for (i, result) in entries.iter().enumerate() {
            let (key, _) = result.as_ref().unwrap();
            let decoded = decode_internal_key(key).unwrap();
            assert_eq!(decoded.user_key, Bytes::from(format!("k{:03}", i)));
        }
    }

    #[test]
    fn small_block_size() {
        let env = test_env();
        env.create_dir_all(Path::new("/data")).unwrap();

        // Use small blocks to test multiple blocks
        let config = SSTableConfig {
            block_size: 128,
            ..SSTableConfig::default()
        };
        let mut builder =
            SSTableBuilder::new(env.clone(), Path::new("/data/small.sst"), config).unwrap();

        for i in 0..100 {
            let key = InternalKey::new(Bytes::from(format!("key{:05}", i)), i as u64);
            let value = MemtableValue::Put(Bytes::from(format!("value{:05}", i)));
            builder.add(&key, &value).unwrap();
        }

        builder.finish().unwrap();

        // Read back and verify all entries
        let reader = SSTableReader::open(env, Path::new("/data/small.sst")).unwrap();
        let entries: Vec<_> = reader.iter().unwrap().filter_map(|r| r.ok()).collect();
        assert_eq!(entries.len(), 100);
    }

    #[test]
    fn varint_encoding() {
        // Test varint encoding/decoding
        let test_values = [0u64, 1, 127, 128, 255, 256, 16383, 16384, u64::MAX];

        for &value in &test_values {
            let mut buf = Vec::new();
            put_varint(&mut buf, value);

            let (decoded, _) = get_varint(&buf).unwrap();
            assert_eq!(decoded, value);
        }
    }

    #[test]
    fn prefix_compression() {
        let env = test_env();
        env.create_dir_all(Path::new("/data")).unwrap();

        let mut builder = SSTableBuilder::new(
            env.clone(),
            Path::new("/data/prefix.sst"),
            SSTableConfig::default(),
        )
        .unwrap();

        // Keys with common prefix should compress well
        for i in 0..100 {
            let key =
                InternalKey::new(Bytes::from(format!("common_prefix_key_{:05}", i)), i as u64);
            let value = MemtableValue::Put(Bytes::from("x"));
            builder.add(&key, &value).unwrap();
        }

        let meta = builder.finish().unwrap();

        // File should be relatively small due to prefix compression
        // (hard to assert exact size, but we can check it's reasonable)
        assert!(meta.file_size < 10000);

        // Verify data integrity
        let reader = SSTableReader::open(env, Path::new("/data/prefix.sst")).unwrap();
        for i in 0..100 {
            let key = encode_internal_key(&InternalKey::new(
                Bytes::from(format!("common_prefix_key_{:05}", i)),
                i as u64,
            ));
            let value = reader.get(&key).unwrap();
            assert!(value.is_some());
        }
    }

    #[test]
    fn mvcc_aware_lookup() {
        let env = test_env();
        env.create_dir_all(Path::new("/data")).unwrap();

        // Build SSTable with entries at different sequence numbers
        let mut builder = SSTableBuilder::new(
            env.clone(),
            Path::new("/data/mvcc.sst"),
            SSTableConfig::default(),
        )
        .unwrap();

        // Write multiple keys, each with a specific sequence number
        for i in 0..100 {
            let key = InternalKey::new(Bytes::from(format!("key{:05}", i)), i as u64 + 1);
            let value = MemtableValue::Put(Bytes::from(format!("value{:05}", i)));
            builder.add(&key, &value).unwrap();
        }

        builder.finish().unwrap();

        // Read and test MVCC-aware lookup
        let reader = SSTableReader::open(env, Path::new("/data/mvcc.sst")).unwrap();

        // Look up key00050 with a high target sequence (should find it)
        let value = reader.get_mvcc(b"key00050", 1000).unwrap();
        assert!(value.is_some(), "Should find key00050 with high target seq");
        let decoded = decode_value(&value.unwrap()).unwrap();
        assert_eq!(decoded, MemtableValue::Put(Bytes::from("value00050")));

        // Look up key00050 with exact sequence (should find it)
        let value = reader.get_mvcc(b"key00050", 51).unwrap();
        assert!(value.is_some(), "Should find key00050 with exact seq=51");

        // Look up key00050 with lower sequence (should NOT find it)
        let value = reader.get_mvcc(b"key00050", 50).unwrap();
        assert!(
            value.is_none(),
            "Should NOT find key00050 with seq=50 (entry has seq=51)"
        );

        // Look up non-existent key
        let value = reader.get_mvcc(b"nonexistent", 1000).unwrap();
        assert!(value.is_none(), "Should not find nonexistent key");
    }

    #[test]
    fn mvcc_small_blocks() {
        // Test MVCC lookup with small blocks to ensure we search multiple blocks correctly
        let env = test_env();
        env.create_dir_all(Path::new("/data")).unwrap();

        // Use small blocks
        let config = SSTableConfig {
            block_size: 128,
            ..SSTableConfig::default()
        };
        let mut builder =
            SSTableBuilder::new(env.clone(), Path::new("/data/mvcc_small.sst"), config).unwrap();

        for i in 0..100 {
            let key = InternalKey::new(Bytes::from(format!("key{:05}", i)), i as u64 + 1);
            let value = MemtableValue::Put(Bytes::from(format!("value{:05}", i)));
            builder.add(&key, &value).unwrap();
        }

        builder.finish().unwrap();

        let reader = SSTableReader::open(env, Path::new("/data/mvcc_small.sst")).unwrap();

        // Test lookups across different blocks
        for i in [0, 25, 50, 75, 99] {
            let user_key = format!("key{:05}", i);
            let value = reader.get_mvcc(user_key.as_bytes(), 1000).unwrap();
            assert!(
                value.is_some(),
                "Should find {} with high target seq",
                user_key
            );
            let decoded = decode_value(&value.unwrap()).unwrap();
            assert_eq!(
                decoded,
                MemtableValue::Put(Bytes::from(format!("value{:05}", i)))
            );
        }
    }

    #[test]
    fn learned_index_integration() {
        use learned::bloom::BloomConfig;
        use learned::pgm::PgmConfig;

        let env = test_env();
        env.create_dir_all(Path::new("/data")).unwrap();

        // Use small block size to create many blocks (needed for learned indexes)
        // and low thresholds to trigger learned indexes
        let build_config = SSTableConfig {
            block_size: 64, // Very small blocks = many blocks
            ..SSTableConfig::default()
        };

        let mut builder =
            SSTableBuilder::new(env.clone(), Path::new("/data/learned.sst"), build_config).unwrap();

        // Add entries - small block size will create many blocks
        for i in 0..1000 {
            let key = InternalKey::new(Bytes::from(format!("key{:05}", i)), i as u64);
            let value = MemtableValue::Put(Bytes::from(format!("value{:05}", i)));
            builder.add(&key, &value).unwrap();
        }

        builder.finish().unwrap();

        // Open with learned indexes - use low thresholds
        let learned_config = SSTableConfig {
            use_learned_indexes: true,
            pgm_config: PgmConfig {
                epsilon: 64,
                min_keys: 10, // Low threshold to trigger on our test data
            },
            bloom_config: BloomConfig {
                false_positive_rate: 0.01,
            },
            ..SSTableConfig::default()
        };
        let reader_learned = SSTableReader::open_with_config(
            env.clone(),
            Path::new("/data/learned.sst"),
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
            Path::new("/data/learned.sst"),
            &classical_config,
        )
        .unwrap();

        println!(
            "Number of index entries: {}",
            reader_learned.meta().num_entries
        );

        // Verify both produce same results
        for i in 0..1000 {
            let key = encode_internal_key(&InternalKey::new(
                Bytes::from(format!("key{:05}", i)),
                i as u64,
            ));

            let result_learned = reader_learned.get(&key).unwrap();
            let result_classical = reader_classical.get(&key).unwrap();

            assert_eq!(
                result_learned, result_classical,
                "Mismatch at key {} between learned and classical index",
                i
            );
            assert!(result_learned.is_some());
        }

        // Check non-existent keys
        for i in 1000..1010 {
            let key = encode_internal_key(&InternalKey::new(
                Bytes::from(format!("key{:05}", i)),
                i as u64,
            ));

            assert!(reader_learned.get(&key).unwrap().is_none());
            assert!(reader_classical.get(&key).unwrap().is_none());
        }

        // Verify learned index is actually being used
        assert!(reader_learned.uses_learned_indexes());
        assert!(!reader_classical.uses_learned_indexes());

        // Check memory usage is reasonable
        let learned_mem = reader_learned.index_memory_usage();
        let classical_mem = reader_classical.index_memory_usage();
        println!("Learned index memory: {} bytes", learned_mem);
        println!("Classical index memory: {} bytes", classical_mem);
    }

    #[test]
    fn bloom_filter_integration() {
        use learned::bloom::BloomConfig;
        use learned::pgm::PgmConfig;

        let env = test_env();
        env.create_dir_all(Path::new("/data")).unwrap();

        // Use small blocks so each block has ~1 key, making the Bloom filter
        // effectively contain all keys (since it's built from block first keys)
        let config = SSTableConfig {
            block_size: 64, // Very small to get one key per block
            use_learned_indexes: true,
            pgm_config: PgmConfig {
                epsilon: 64,
                min_keys: 10,
            },
            bloom_config: BloomConfig {
                false_positive_rate: 0.01,
            },
        };

        let mut builder =
            SSTableBuilder::new(env.clone(), Path::new("/data/bloom.sst"), config.clone()).unwrap();

        // Add entries
        for i in 0..500 {
            let key = InternalKey::new(Bytes::from(format!("key{:05}", i)), i as u64);
            let value = MemtableValue::Put(Bytes::from(format!("value{:05}", i)));
            builder.add(&key, &value).unwrap();
        }

        builder.finish().unwrap();

        let reader =
            SSTableReader::open_with_config(env, Path::new("/data/bloom.sst"), &config).unwrap();

        // Note: The Bloom filter is built from USER keys (not internal keys).
        // This allows efficient may_contain checks during reads.

        // Bloom filter should return true for all user keys that exist
        let mut found_count = 0;
        for i in 0..500 {
            let user_key = format!("key{:05}", i);
            if reader.may_contain(user_key.as_bytes()) {
                found_count += 1;
            }
        }

        // All keys should be found (no false negatives)
        println!("Keys found in Bloom filter: {} / 500", found_count);
        assert_eq!(
            found_count, 500,
            "Bloom filter has false negatives: {} / 500",
            found_count
        );

        // Bloom filter should reject most keys outside the range
        let mut false_positives = 0;
        for i in 500..600 {
            let user_key = format!("key{:05}", i);
            if reader.may_contain(user_key.as_bytes()) {
                false_positives += 1;
            }
        }

        println!("False positives: {} / 100", false_positives);
        // Some false positives are expected (bloom filter property)
        assert!(
            false_positives < 20,
            "Too many false positives: {}",
            false_positives
        );
    }
}
