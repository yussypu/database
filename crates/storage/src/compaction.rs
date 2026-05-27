//! Leveled compaction implementation.
//!
//! Compaction merges SSTables to maintain read performance and
//! reclaim space from deleted/overwritten keys.
//!
//! # Level Structure
//!
//! - **L0**: Newly flushed SSTables. Files may overlap. Size limit: 4 files.
//! - **L1+**: Files are non-overlapping within each level.
//!   - Each level is 10× larger than the previous.
//!   - Default size ratio: 10
//!
//! # Compaction Triggers
//!
//! - L0 compaction triggers when L0 has more than `l0_compaction_trigger` files.
//! - Ln compaction triggers when level size exceeds `base_level_size * size_ratio^n`.
//!
//! # Compaction Process
//!
//! - **L0 → L1**: Merge all overlapping L0 files with overlapping L1 files.
//! - **Ln → Ln+1**: Pick one file from Ln, merge with overlapping files in Ln+1.

use crate::error::{Error, Result};
use crate::sstable::{
    decode_internal_key, decode_value, SSTableBuilder, SSTableConfig, SSTableReader,
};
use bytes::Bytes;
use runtime::{Env, PathBuf};
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashSet};

/// Configuration for compaction.
#[derive(Debug, Clone)]
pub struct CompactionConfig {
    /// Number of L0 files that trigger compaction.
    pub l0_compaction_trigger: usize,
    /// Base size for L1 in bytes.
    pub base_level_size: u64,
    /// Size ratio between levels.
    pub size_ratio: u64,
    /// Maximum number of levels.
    pub max_levels: usize,
    /// SSTable configuration.
    pub sstable_config: SSTableConfig,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            l0_compaction_trigger: 4,
            base_level_size: 64 * 1024 * 1024, // 64 MB
            size_ratio: 10,
            max_levels: 7,
            sstable_config: SSTableConfig::default(),
        }
    }
}

/// Represents an SSTable file in the manifest.
#[derive(Debug, Clone)]
pub struct FileMetadata {
    /// Unique file number.
    pub file_num: u64,
    /// File size in bytes.
    pub file_size: u64,
    /// Smallest key in the file.
    pub smallest_key: Bytes,
    /// Largest key in the file.
    pub largest_key: Bytes,
}

impl FileMetadata {
    /// Checks if this file's key range overlaps with another.
    pub fn overlaps(&self, other: &FileMetadata) -> bool {
        !(self.largest_key < other.smallest_key || self.smallest_key > other.largest_key)
    }

    /// Checks if this file's key range overlaps with a key range.
    pub fn overlaps_range(&self, smallest: &[u8], largest: &[u8]) -> bool {
        !(self.largest_key.as_ref() < smallest || self.smallest_key.as_ref() > largest)
    }
}

/// Version represents a consistent view of the database state.
#[derive(Debug, Clone)]
pub struct Version {
    /// Files at each level. Level 0 is index 0.
    pub levels: Vec<Vec<FileMetadata>>,
}

impl Version {
    /// Creates a new empty version with the given number of levels.
    pub fn new(num_levels: usize) -> Self {
        Self {
            levels: vec![Vec::new(); num_levels],
        }
    }

    /// Returns the total size of a level in bytes.
    pub fn level_size(&self, level: usize) -> u64 {
        self.levels
            .get(level)
            .map(|files| files.iter().map(|f| f.file_size).sum())
            .unwrap_or(0)
    }

    /// Returns files at L0 that overlap with the given key range.
    pub fn get_overlapping_l0(&self, smallest: &[u8], largest: &[u8]) -> Vec<&FileMetadata> {
        self.levels[0]
            .iter()
            .filter(|f| f.overlaps_range(smallest, largest))
            .collect()
    }

    /// Returns files at the given level that overlap with the key range.
    pub fn get_overlapping(
        &self,
        level: usize,
        smallest: &[u8],
        largest: &[u8],
    ) -> Vec<&FileMetadata> {
        if level == 0 {
            return self.get_overlapping_l0(smallest, largest);
        }

        // For L1+, files are sorted and non-overlapping
        // Use binary search to find the range
        let files = &self.levels[level];
        if files.is_empty() {
            return Vec::new();
        }

        // Find first file that might overlap
        let start = files.partition_point(|f| f.largest_key.as_ref() < smallest);
        // Find last file that might overlap
        let end = files.partition_point(|f| f.smallest_key.as_ref() <= largest);

        files[start..end].iter().collect()
    }
}

/// Describes a compaction task.
#[derive(Debug)]
pub struct CompactionTask {
    /// Level being compacted from.
    pub level: usize,
    /// Files from the source level.
    pub input_files: Vec<FileMetadata>,
    /// Files from the target level (level + 1).
    pub output_level_files: Vec<FileMetadata>,
}

/// Picks the next compaction to run.
pub struct CompactionPicker {
    config: CompactionConfig,
}

impl CompactionPicker {
    pub fn new(config: CompactionConfig) -> Self {
        Self { config }
    }

    /// Picks the next compaction task, if any.
    pub fn pick_compaction(&self, version: &Version) -> Option<CompactionTask> {
        // Check L0 first
        if version.levels[0].len() >= self.config.l0_compaction_trigger {
            return self.pick_l0_compaction(version);
        }

        // Check other levels
        for level in 1..version.levels.len() - 1 {
            let level_size = version.level_size(level);
            let target_size = self.target_level_size(level);

            if level_size > target_size {
                return self.pick_level_compaction(version, level);
            }
        }

        None
    }

    fn target_level_size(&self, level: usize) -> u64 {
        if level == 0 {
            return 0; // L0 uses file count, not size
        }
        self.config.base_level_size * self.config.size_ratio.pow(level as u32 - 1)
    }

    fn pick_l0_compaction(&self, version: &Version) -> Option<CompactionTask> {
        if version.levels[0].is_empty() {
            return None;
        }

        // For L0, compact all files
        let input_files: Vec<_> = version.levels[0].clone();

        // Find smallest and largest keys across all L0 files
        let smallest = input_files.iter().map(|f| f.smallest_key.as_ref()).min()?;
        let largest = input_files.iter().map(|f| f.largest_key.as_ref()).max()?;

        // Get overlapping files from L1
        let output_level_files: Vec<_> = version
            .get_overlapping(1, smallest, largest)
            .into_iter()
            .cloned()
            .collect();

        Some(CompactionTask {
            level: 0,
            input_files,
            output_level_files,
        })
    }

    fn pick_level_compaction(&self, version: &Version, level: usize) -> Option<CompactionTask> {
        let files = &version.levels[level];
        if files.is_empty() {
            return None;
        }

        // Pick the file with the largest size (simple heuristic)
        // In a real implementation, we'd use a more sophisticated policy
        let file = files.iter().max_by_key(|f| f.file_size)?.clone();

        // Get overlapping files from level + 1
        let output_level_files: Vec<_> = version
            .get_overlapping(
                level + 1,
                file.smallest_key.as_ref(),
                file.largest_key.as_ref(),
            )
            .into_iter()
            .cloned()
            .collect();

        Some(CompactionTask {
            level,
            input_files: vec![file],
            output_level_files,
        })
    }
}

/// Result of a compaction.
#[derive(Debug)]
pub struct CompactionResult {
    /// Files that were removed (merged).
    pub removed_files: Vec<u64>,
    /// New files that were created.
    pub new_files: Vec<FileMetadata>,
    /// Target level for the new files.
    pub target_level: usize,
}

/// Entry for the merge iterator.
struct MergeEntry {
    key: Bytes,
    value: Bytes,
    /// Index of the source iterator.
    source_idx: usize,
}

impl PartialEq for MergeEntry {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}

impl Eq for MergeEntry {}

impl PartialOrd for MergeEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for MergeEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        // Min-heap: smaller keys first
        // For equal keys, prefer lower source_idx (newer data)
        match other.key.cmp(&self.key) {
            Ordering::Equal => other.source_idx.cmp(&self.source_idx),
            ord => ord,
        }
    }
}

/// Merging iterator over multiple SSTable iterators.
pub struct MergeIterator<I> {
    iters: Vec<Option<I>>,
    heap: BinaryHeap<MergeEntry>,
    last_user_key: Option<Bytes>,
    /// GC watermark: if Some, apply version GC during merge.
    /// Versions with commit_ts <= watermark are candidates for GC.
    gc_watermark: Option<u64>,
    /// Buffer for collecting versions of the same key during GC.
    version_buffer: Vec<(u64, Bytes, Bytes)>, // (seq, key, value)
}

impl<I> MergeIterator<I>
where
    I: Iterator<Item = Result<(Bytes, Bytes)>>,
{
    /// Creates a new merge iterator.
    pub fn new(iters: Vec<I>) -> Self {
        let iters: Vec<_> = iters.into_iter().map(Some).collect();
        Self {
            iters,
            heap: BinaryHeap::new(),
            last_user_key: None,
            gc_watermark: None,
            version_buffer: Vec::new(),
        }
    }

    /// Creates a new merge iterator with GC enabled.
    ///
    /// With GC enabled, for each key:
    /// - Keep all versions with commit_ts > watermark (active transactions need them)
    /// - Keep the newest version with commit_ts <= watermark (for new transactions)
    /// - Discard older versions with commit_ts <= watermark
    pub fn new_with_gc(iters: Vec<I>, watermark: u64) -> Self {
        let iters: Vec<_> = iters.into_iter().map(Some).collect();
        Self {
            iters,
            heap: BinaryHeap::new(),
            last_user_key: None,
            gc_watermark: Some(watermark),
            version_buffer: Vec::new(),
        }
    }

    /// Initializes the heap with the first entry from each iterator.
    fn init(&mut self) -> Result<()> {
        for (idx, iter_opt) in self.iters.iter_mut().enumerate() {
            if let Some(iter) = iter_opt {
                if let Some(result) = iter.next() {
                    let (key, value) = result?;
                    self.heap.push(MergeEntry {
                        key,
                        value,
                        source_idx: idx,
                    });
                }
            }
        }
        Ok(())
    }

    /// Gets the next unique entry (deduplicated by user key).
    ///
    /// When GC is disabled, keeps only the newest version per key.
    /// When GC is enabled, applies version GC per ADR-027:
    /// - Keep all versions with commit_ts > watermark
    /// - Keep newest version with commit_ts <= watermark
    /// - Discard older versions with commit_ts <= watermark
    pub fn next_entry(&mut self) -> Result<Option<(Bytes, Bytes)>> {
        // If we have buffered versions from GC, return them first
        if !self.version_buffer.is_empty() {
            let (_, key, value) = self.version_buffer.remove(0);
            return Ok(Some((key, value)));
        }

        loop {
            let entry = match self.heap.pop() {
                Some(e) => e,
                None => return Ok(None),
            };

            // Advance the source iterator
            if let Some(Some(iter)) = self.iters.get_mut(entry.source_idx) {
                if let Some(result) = iter.next() {
                    let (key, value) = result?;
                    self.heap.push(MergeEntry {
                        key,
                        value,
                        source_idx: entry.source_idx,
                    });
                }
            }

            // Extract user key and sequence number
            let (user_key, seq) = if entry.key.len() >= 8 {
                let seq_bytes = &entry.key[entry.key.len() - 8..];
                // Sequence is stored inverted (u64::MAX - seq) for sorting
                let inverted_seq = u64::from_be_bytes(seq_bytes.try_into().unwrap_or([0; 8]));
                let seq = u64::MAX - inverted_seq;
                (entry.key.slice(0..entry.key.len() - 8), seq)
            } else {
                (entry.key.clone(), 0)
            };

            // Check if this is a new user key
            let is_new_key = match &self.last_user_key {
                Some(last) => user_key != *last,
                None => true,
            };

            if is_new_key {
                // Process the previous key's versions if GC is enabled
                if self.gc_watermark.is_some() && !self.version_buffer.is_empty() {
                    // Apply GC to buffered versions and prepare to return them
                    self.apply_gc_to_buffer();
                    // Put current entry back in heap and return buffered
                    self.heap.push(MergeEntry {
                        key: entry.key,
                        value: entry.value,
                        source_idx: entry.source_idx,
                    });
                    self.last_user_key = Some(user_key);
                    if !self.version_buffer.is_empty() {
                        let (_, key, value) = self.version_buffer.remove(0);
                        return Ok(Some((key, value)));
                    }
                    continue;
                }

                self.last_user_key = Some(user_key.clone());

                // If GC is enabled, buffer this version
                if self.gc_watermark.is_some() {
                    self.version_buffer.push((seq, entry.key, entry.value));
                    continue;
                }

                // No GC: return immediately
                return Ok(Some((entry.key, entry.value)));
            } else {
                // Same user key as before
                if self.gc_watermark.is_some() {
                    // Buffer for GC processing
                    self.version_buffer.push((seq, entry.key, entry.value));
                }
                // Without GC, skip duplicates (already returned newest)
                continue;
            }
        }
    }

    /// Apply GC rules to the version buffer.
    ///
    /// Per ADR-027:
    /// - Keep all versions with commit_ts > watermark
    /// - Keep newest version with commit_ts <= watermark
    /// - Discard older versions with commit_ts <= watermark
    /// - Tombstones are kept unconditionally (Phase 6 will handle them)
    fn apply_gc_to_buffer(&mut self) {
        let watermark = match self.gc_watermark {
            Some(w) => w,
            None => return,
        };

        if self.version_buffer.is_empty() {
            return;
        }

        // Sort by seq descending (newest first)
        self.version_buffer.sort_by(|a, b| b.0.cmp(&a.0));

        let mut kept = Vec::new();
        let mut found_below_watermark = false;

        for (seq, key, value) in self.version_buffer.drain(..) {
            if seq > watermark {
                // Keep all versions above watermark
                kept.push((seq, key, value));
            } else if !found_below_watermark {
                // Keep the newest version at or below watermark
                kept.push((seq, key, value));
                found_below_watermark = true;
            }
            // Else: discard (older version below watermark)
        }

        // Sort back to seq descending for output order
        kept.sort_by(|a, b| b.0.cmp(&a.0));
        self.version_buffer = kept;
    }

    /// Flush any remaining buffered versions (call at end of iteration).
    pub fn flush_buffer(&mut self) -> Vec<(Bytes, Bytes)> {
        if self.gc_watermark.is_some() {
            self.apply_gc_to_buffer();
        }
        self.version_buffer
            .drain(..)
            .map(|(_, key, value)| (key, value))
            .collect()
    }
}

/// Executes a compaction task.
pub struct CompactionExecutor<E: Env> {
    env: E,
    base_path: PathBuf,
    config: CompactionConfig,
}

impl<E: Env + Clone> CompactionExecutor<E> {
    pub fn new(env: E, base_path: PathBuf, config: CompactionConfig) -> Self {
        Self {
            env,
            base_path,
            config,
        }
    }

    /// Executes a compaction task and returns the result.
    pub fn execute(
        &self,
        task: &CompactionTask,
        next_file_num: &mut u64,
    ) -> Result<CompactionResult> {
        // Collect all file numbers being removed
        let mut removed_files: Vec<u64> = task.input_files.iter().map(|f| f.file_num).collect();
        removed_files.extend(task.output_level_files.iter().map(|f| f.file_num));

        // Open all input files first (keep readers alive)
        let mut readers = Vec::new();

        for file in &task.input_files {
            let path = self.file_path(file.file_num);
            let reader = SSTableReader::open(self.env.clone(), &path)?;
            readers.push(reader);
        }

        for file in &task.output_level_files {
            let path = self.file_path(file.file_num);
            let reader = SSTableReader::open(self.env.clone(), &path)?;
            readers.push(reader);
        }

        // Create iterators from the readers (they borrow from readers)
        let iters: Vec<_> = readers
            .iter()
            .map(|r| r.iter())
            .collect::<Result<Vec<_>>>()?;

        // Create merge iterator
        let mut merge_iter = MergeIterator::new(iters);
        merge_iter.init()?;

        // Write output files
        let target_level = task.level + 1;
        let mut new_files = Vec::new();
        let mut current_builder: Option<SSTableBuilder<E>> = None;
        let mut current_file_num = 0u64;
        let mut current_first_key: Option<Bytes> = None;
        let mut current_last_key: Option<Bytes> = None;
        let mut entries_in_current = 0usize;

        // Target size per output file (aim for ~64MB per file in L1+)
        let target_file_size = self.config.base_level_size / 10;

        while let Some((key, value)) = merge_iter.next_entry()? {
            // Check if we need to start a new file
            if current_builder.is_none() {
                current_file_num = *next_file_num;
                *next_file_num += 1;
                let path = self.file_path(current_file_num);
                current_builder = Some(SSTableBuilder::new(
                    self.env.clone(),
                    &path,
                    self.config.sstable_config.clone(),
                )?);
                current_first_key = Some(key.clone());
                entries_in_current = 0;
            }

            // Decode and re-encode to ensure proper format
            let internal_key = decode_internal_key(&key)
                .ok_or_else(|| Error::Corruption("invalid internal key".to_string()))?;
            let memtable_value = decode_value(&value)
                .ok_or_else(|| Error::Corruption("invalid value".to_string()))?;

            current_builder
                .as_mut()
                .unwrap()
                .add(&internal_key, &memtable_value)?;
            current_last_key = Some(key.clone());
            entries_in_current += 1;

            // Check if we should finish this file
            // We use a simple heuristic: finish after target_file_size / 100 entries
            // (assuming ~100 bytes per entry on average)
            let estimated_size = entries_in_current * 100;
            if estimated_size >= target_file_size as usize && entries_in_current > 0 {
                let meta = current_builder.take().unwrap().finish()?;
                new_files.push(FileMetadata {
                    file_num: current_file_num,
                    file_size: meta.file_size,
                    smallest_key: current_first_key.take().unwrap(),
                    largest_key: current_last_key.take().unwrap(),
                });
            }
        }

        // Finish the last file if there's one in progress
        if let Some(builder) = current_builder {
            let meta = builder.finish()?;
            new_files.push(FileMetadata {
                file_num: current_file_num,
                file_size: meta.file_size,
                smallest_key: current_first_key.unwrap(),
                largest_key: current_last_key.unwrap(),
            });
        }

        Ok(CompactionResult {
            removed_files,
            new_files,
            target_level,
        })
    }

    /// Executes a compaction task with version GC.
    ///
    /// Per ADR-027, for each key:
    /// - Keep all versions with commit_ts > watermark
    /// - Keep newest version with commit_ts <= watermark
    /// - Discard older versions with commit_ts <= watermark
    /// - Tombstones are kept unconditionally
    pub fn execute_with_gc(
        &self,
        task: &CompactionTask,
        next_file_num: &mut u64,
        watermark: u64,
    ) -> Result<CompactionResult> {
        // Collect all file numbers being removed
        let mut removed_files: Vec<u64> = task.input_files.iter().map(|f| f.file_num).collect();
        removed_files.extend(task.output_level_files.iter().map(|f| f.file_num));

        // Open all input files
        let mut readers = Vec::new();

        for file in &task.input_files {
            let path = self.file_path(file.file_num);
            let reader = SSTableReader::open(self.env.clone(), &path)?;
            readers.push(reader);
        }

        for file in &task.output_level_files {
            let path = self.file_path(file.file_num);
            let reader = SSTableReader::open(self.env.clone(), &path)?;
            readers.push(reader);
        }

        // Create iterators
        let iters: Vec<_> = readers
            .iter()
            .map(|r| r.iter())
            .collect::<Result<Vec<_>>>()?;

        // Create merge iterator WITH GC
        let mut merge_iter = MergeIterator::new_with_gc(iters, watermark);
        merge_iter.init()?;

        // Write output files
        let target_level = task.level + 1;
        let mut new_files = Vec::new();
        let mut current_builder: Option<SSTableBuilder<E>> = None;
        let mut current_file_num = 0u64;
        let mut current_first_key: Option<Bytes> = None;
        let mut current_last_key: Option<Bytes> = None;
        let mut entries_in_current = 0usize;

        let target_file_size = self.config.base_level_size / 10;

        // Process entries from merge iterator
        while let Some((key, value)) = merge_iter.next_entry()? {
            if current_builder.is_none() {
                current_file_num = *next_file_num;
                *next_file_num += 1;
                let path = self.file_path(current_file_num);
                current_builder = Some(SSTableBuilder::new(
                    self.env.clone(),
                    &path,
                    self.config.sstable_config.clone(),
                )?);
                current_first_key = Some(key.clone());
                entries_in_current = 0;
            }

            let internal_key = decode_internal_key(&key)
                .ok_or_else(|| Error::Corruption("invalid internal key".to_string()))?;
            let memtable_value = decode_value(&value)
                .ok_or_else(|| Error::Corruption("invalid value".to_string()))?;

            current_builder
                .as_mut()
                .unwrap()
                .add(&internal_key, &memtable_value)?;
            current_last_key = Some(key.clone());
            entries_in_current += 1;

            let estimated_size = entries_in_current * 100;
            if estimated_size >= target_file_size as usize && entries_in_current > 0 {
                let meta = current_builder.take().unwrap().finish()?;
                new_files.push(FileMetadata {
                    file_num: current_file_num,
                    file_size: meta.file_size,
                    smallest_key: current_first_key.take().unwrap(),
                    largest_key: current_last_key.take().unwrap(),
                });
            }
        }

        // Flush remaining buffered versions
        for (key, value) in merge_iter.flush_buffer() {
            if current_builder.is_none() {
                current_file_num = *next_file_num;
                *next_file_num += 1;
                let path = self.file_path(current_file_num);
                current_builder = Some(SSTableBuilder::new(
                    self.env.clone(),
                    &path,
                    self.config.sstable_config.clone(),
                )?);
                current_first_key = Some(key.clone());
                entries_in_current = 0;
            }

            let internal_key = decode_internal_key(&key)
                .ok_or_else(|| Error::Corruption("invalid internal key".to_string()))?;
            let memtable_value = decode_value(&value)
                .ok_or_else(|| Error::Corruption("invalid value".to_string()))?;

            current_builder
                .as_mut()
                .unwrap()
                .add(&internal_key, &memtable_value)?;
            current_last_key = Some(key.clone());
            entries_in_current += 1;
        }

        // Finish the last file
        if let Some(builder) = current_builder {
            let meta = builder.finish()?;
            new_files.push(FileMetadata {
                file_num: current_file_num,
                file_size: meta.file_size,
                smallest_key: current_first_key.unwrap(),
                largest_key: current_last_key.unwrap(),
            });
        }

        Ok(CompactionResult {
            removed_files,
            new_files,
            target_level,
        })
    }

    fn file_path(&self, file_num: u64) -> PathBuf {
        self.base_path.join(format!("{:06}.sst", file_num))
    }
}

/// Applies a compaction result to create a new version.
pub fn apply_compaction(
    version: &Version,
    task: &CompactionTask,
    result: &CompactionResult,
) -> Version {
    let mut new_version = version.clone();

    // Create set of removed file numbers for quick lookup
    let removed: HashSet<u64> = result.removed_files.iter().copied().collect();

    // Remove files from source level
    new_version.levels[task.level].retain(|f| !removed.contains(&f.file_num));

    // Remove files from target level
    if result.target_level < new_version.levels.len() {
        new_version.levels[result.target_level].retain(|f| !removed.contains(&f.file_num));
    }

    // Add new files to target level
    if result.target_level < new_version.levels.len() {
        for file in &result.new_files {
            new_version.levels[result.target_level].push(file.clone());
        }

        // Sort files at target level by smallest key (for L1+)
        if result.target_level > 0 {
            new_version.levels[result.target_level]
                .sort_by(|a, b| a.smallest_key.cmp(&b.smallest_key));
        }
    }

    new_version
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memtable::{InternalKey, MemtableValue};
    use crate::sstable::encode_internal_key;
    use runtime::{Path, SimEnv, SimEnvConfig};

    fn test_env() -> SimEnv {
        SimEnv::new(SimEnvConfig::with_seed(42))
    }

    fn create_test_sstable<E: Env>(
        env: E,
        path: &Path,
        start: u64,
        end: u64,
    ) -> Result<FileMetadata> {
        let mut builder = SSTableBuilder::new(env, path, SSTableConfig::default())?;

        for i in start..end {
            let key = InternalKey::new(Bytes::from(format!("key{:05}", i)), i);
            let value = MemtableValue::Put(Bytes::from(format!("value{:05}", i)));
            builder.add(&key, &value)?;
        }

        let meta = builder.finish()?;

        let smallest_key = encode_internal_key(&InternalKey::new(
            Bytes::from(format!("key{:05}", start)),
            start,
        ));
        let largest_key = encode_internal_key(&InternalKey::new(
            Bytes::from(format!("key{:05}", end - 1)),
            end - 1,
        ));

        Ok(FileMetadata {
            file_num: 0,
            file_size: meta.file_size,
            smallest_key,
            largest_key,
        })
    }

    #[test]
    fn version_level_size() {
        let mut version = Version::new(7);
        version.levels[0].push(FileMetadata {
            file_num: 1,
            file_size: 1000,
            smallest_key: Bytes::from("a"),
            largest_key: Bytes::from("b"),
        });
        version.levels[0].push(FileMetadata {
            file_num: 2,
            file_size: 2000,
            smallest_key: Bytes::from("c"),
            largest_key: Bytes::from("d"),
        });

        assert_eq!(version.level_size(0), 3000);
        assert_eq!(version.level_size(1), 0);
    }

    #[test]
    fn version_get_overlapping() {
        let mut version = Version::new(7);

        // L0 files (can overlap)
        version.levels[0].push(FileMetadata {
            file_num: 1,
            file_size: 1000,
            smallest_key: Bytes::from("a"),
            largest_key: Bytes::from("m"),
        });
        version.levels[0].push(FileMetadata {
            file_num: 2,
            file_size: 1000,
            smallest_key: Bytes::from("g"),
            largest_key: Bytes::from("z"),
        });

        // Check overlap in L0
        let overlapping = version.get_overlapping_l0(b"f", b"h");
        assert_eq!(overlapping.len(), 2);

        // L1 files (non-overlapping, sorted)
        version.levels[1].push(FileMetadata {
            file_num: 3,
            file_size: 1000,
            smallest_key: Bytes::from("a"),
            largest_key: Bytes::from("f"),
        });
        version.levels[1].push(FileMetadata {
            file_num: 4,
            file_size: 1000,
            smallest_key: Bytes::from("g"),
            largest_key: Bytes::from("m"),
        });
        version.levels[1].push(FileMetadata {
            file_num: 5,
            file_size: 1000,
            smallest_key: Bytes::from("n"),
            largest_key: Bytes::from("z"),
        });

        // Check overlap in L1
        let overlapping = version.get_overlapping(1, b"e", b"h");
        assert_eq!(overlapping.len(), 2);
        assert_eq!(overlapping[0].file_num, 3);
        assert_eq!(overlapping[1].file_num, 4);
    }

    #[test]
    fn compaction_picker_l0_trigger() {
        let config = CompactionConfig {
            l0_compaction_trigger: 4,
            ..Default::default()
        };
        let picker = CompactionPicker::new(config);

        let mut version = Version::new(7);

        // Add 3 L0 files - should not trigger
        for i in 0..3 {
            version.levels[0].push(FileMetadata {
                file_num: i,
                file_size: 1000,
                smallest_key: Bytes::from(format!("{}", i)),
                largest_key: Bytes::from(format!("{}z", i)),
            });
        }

        assert!(picker.pick_compaction(&version).is_none());

        // Add 4th file - should trigger
        version.levels[0].push(FileMetadata {
            file_num: 4,
            file_size: 1000,
            smallest_key: Bytes::from("4"),
            largest_key: Bytes::from("4z"),
        });

        let task = picker.pick_compaction(&version);
        assert!(task.is_some());
        assert_eq!(task.unwrap().level, 0);
    }

    #[test]
    fn merge_iterator_deduplication() {
        // Create properly encoded keys using encode_internal_key
        // With inverted seq encoding, higher seq sorts to smaller bytes (comes first)
        // This is correct for MVCC: newer versions should win over older ones

        // iter1 has newer versions (higher seq numbers)
        let key1_v2 = encode_internal_key(&InternalKey::new(Bytes::from("key00001"), 10)); // newer
        let key3_v4 = encode_internal_key(&InternalKey::new(Bytes::from("key00003"), 12));

        // iter2 has older versions (lower seq numbers)
        let key1_v1 = encode_internal_key(&InternalKey::new(Bytes::from("key00001"), 5)); // older
        let key2_v3 = encode_internal_key(&InternalKey::new(Bytes::from("key00002"), 8));

        let iter1 = vec![
            Ok((key1_v2, Bytes::from("v2"))),
            Ok((key3_v4, Bytes::from("v4"))),
        ]
        .into_iter();

        let iter2 = vec![
            Ok((key1_v1, Bytes::from("v1"))),
            Ok((key2_v3, Bytes::from("v3"))),
        ]
        .into_iter();

        let mut merge = MergeIterator::new(vec![iter1, iter2]);
        merge.init().unwrap();

        // With inverted seq encoding (higher seq = smaller bytes):
        // key00001+seq10 sorts BEFORE key00001+seq5
        // So the newer version (v2) comes first and wins deduplication
        let (key, value) = merge.next_entry().unwrap().unwrap();
        assert!(key.starts_with(b"key00001"));
        // Newer version (higher seq=10, v2) wins
        assert_eq!(value, Bytes::from("v2"));

        // Should get key00002
        let (key, value) = merge.next_entry().unwrap().unwrap();
        assert!(key.starts_with(b"key00002"));
        assert_eq!(value, Bytes::from("v3"));

        // Should get key00003
        let (key, value) = merge.next_entry().unwrap().unwrap();
        assert!(key.starts_with(b"key00003"));
        assert_eq!(value, Bytes::from("v4"));

        // Should be done
        assert!(merge.next_entry().unwrap().is_none());
    }

    #[test]
    fn compaction_execution() {
        let env = test_env();
        env.create_dir_all(Path::new("/data")).unwrap();

        // Create two L0 SSTables with overlapping keys
        let mut file1 =
            create_test_sstable(env.clone(), Path::new("/data/000001.sst"), 0, 50).unwrap();
        file1.file_num = 1;

        let mut file2 =
            create_test_sstable(env.clone(), Path::new("/data/000002.sst"), 25, 75).unwrap();
        file2.file_num = 2;

        let task = CompactionTask {
            level: 0,
            input_files: vec![file1, file2],
            output_level_files: vec![],
        };

        let executor = CompactionExecutor::new(
            env.clone(),
            PathBuf::from("/data"),
            CompactionConfig::default(),
        );

        let mut next_file_num = 3;
        let result = executor.execute(&task, &mut next_file_num).unwrap();

        // Should have removed 2 files
        assert_eq!(result.removed_files.len(), 2);

        // Should have created at least 1 new file
        assert!(!result.new_files.is_empty());

        // Target level should be L1
        assert_eq!(result.target_level, 1);

        // Verify the output file is readable
        for file in &result.new_files {
            let path = PathBuf::from("/data").join(format!("{:06}.sst", file.file_num));
            let reader = SSTableReader::open(env.clone(), &path).unwrap();

            // Verify we can iterate over entries
            let count = reader.iter().unwrap().filter(|r| r.is_ok()).count();
            assert!(count > 0);
        }
    }

    #[test]
    fn apply_compaction_updates_version() {
        let mut version = Version::new(7);

        // Add files to L0
        version.levels[0].push(FileMetadata {
            file_num: 1,
            file_size: 1000,
            smallest_key: Bytes::from("a"),
            largest_key: Bytes::from("m"),
        });
        version.levels[0].push(FileMetadata {
            file_num: 2,
            file_size: 1000,
            smallest_key: Bytes::from("n"),
            largest_key: Bytes::from("z"),
        });

        let task = CompactionTask {
            level: 0,
            input_files: version.levels[0].clone(),
            output_level_files: vec![],
        };

        let result = CompactionResult {
            removed_files: vec![1, 2],
            new_files: vec![FileMetadata {
                file_num: 3,
                file_size: 2000,
                smallest_key: Bytes::from("a"),
                largest_key: Bytes::from("z"),
            }],
            target_level: 1,
        };

        let new_version = apply_compaction(&version, &task, &result);

        // L0 should be empty
        assert!(new_version.levels[0].is_empty());

        // L1 should have the new file
        assert_eq!(new_version.levels[1].len(), 1);
        assert_eq!(new_version.levels[1][0].file_num, 3);
    }

    // ===== Version GC tests for Phase 3.6 =====

    #[test]
    fn version_gc_removes_old_versions() {
        // Create entries for the same key with different sequence numbers
        // Watermark = 10
        // Versions: seq=5, seq=8, seq=12
        // Expected: keep seq=12 (above), keep seq=8 (newest below), drop seq=5

        let key_seq5 = encode_internal_key(&InternalKey::new(Bytes::from("key00001"), 5));
        let key_seq8 = encode_internal_key(&InternalKey::new(Bytes::from("key00001"), 8));
        let key_seq12 = encode_internal_key(&InternalKey::new(Bytes::from("key00001"), 12));

        let iter = vec![
            Ok((key_seq12.clone(), Bytes::from("v12"))),
            Ok((key_seq8.clone(), Bytes::from("v8"))),
            Ok((key_seq5.clone(), Bytes::from("v5"))),
        ]
        .into_iter();

        let mut merge = MergeIterator::new_with_gc(vec![iter], 10);
        merge.init().unwrap();

        let mut results = Vec::new();
        while let Some((key, value)) = merge.next_entry().unwrap() {
            results.push((key, value));
        }
        // Flush remaining
        for (key, value) in merge.flush_buffer() {
            results.push((key, value));
        }

        // Should have 2 versions: seq=12 and seq=8
        assert_eq!(
            results.len(),
            2,
            "Should keep 2 versions, got {:?}",
            results
        );

        // Verify seq=12 is kept (above watermark)
        assert_eq!(results[0].0, key_seq12);
        assert_eq!(results[0].1, Bytes::from("v12"));

        // Verify seq=8 is kept (newest below watermark)
        assert_eq!(results[1].0, key_seq8);
        assert_eq!(results[1].1, Bytes::from("v8"));
    }

    #[test]
    fn version_gc_keeps_newest_below_watermark() {
        // All versions below watermark - should keep only the newest
        // Watermark = 20
        // Versions: seq=5, seq=10, seq=15
        // Expected: keep only seq=15

        let key_seq5 = encode_internal_key(&InternalKey::new(Bytes::from("key00001"), 5));
        let key_seq10 = encode_internal_key(&InternalKey::new(Bytes::from("key00001"), 10));
        let key_seq15 = encode_internal_key(&InternalKey::new(Bytes::from("key00001"), 15));

        let iter = vec![
            Ok((key_seq15.clone(), Bytes::from("v15"))),
            Ok((key_seq10.clone(), Bytes::from("v10"))),
            Ok((key_seq5.clone(), Bytes::from("v5"))),
        ]
        .into_iter();

        let mut merge = MergeIterator::new_with_gc(vec![iter], 20);
        merge.init().unwrap();

        let mut results = Vec::new();
        while let Some((key, value)) = merge.next_entry().unwrap() {
            results.push((key, value));
        }
        for (key, value) in merge.flush_buffer() {
            results.push((key, value));
        }

        // Should have 1 version: seq=15 (newest below watermark)
        assert_eq!(results.len(), 1, "Should keep 1 version, got {:?}", results);
        assert_eq!(results[0].0, key_seq15);
        assert_eq!(results[0].1, Bytes::from("v15"));
    }

    #[test]
    fn version_gc_preserves_active_txn_snapshot() {
        // Versions above watermark must be preserved for active transactions
        // Watermark = 5
        // Versions: seq=3, seq=7, seq=10
        // Expected: keep seq=7 and seq=10 (above), keep seq=3 (newest at/below)

        let key_seq3 = encode_internal_key(&InternalKey::new(Bytes::from("key00001"), 3));
        let key_seq7 = encode_internal_key(&InternalKey::new(Bytes::from("key00001"), 7));
        let key_seq10 = encode_internal_key(&InternalKey::new(Bytes::from("key00001"), 10));

        let iter = vec![
            Ok((key_seq10.clone(), Bytes::from("v10"))),
            Ok((key_seq7.clone(), Bytes::from("v7"))),
            Ok((key_seq3.clone(), Bytes::from("v3"))),
        ]
        .into_iter();

        let mut merge = MergeIterator::new_with_gc(vec![iter], 5);
        merge.init().unwrap();

        let mut results = Vec::new();
        while let Some((key, value)) = merge.next_entry().unwrap() {
            results.push((key, value));
        }
        for (key, value) in merge.flush_buffer() {
            results.push((key, value));
        }

        // Should have 3 versions: seq=10, seq=7 (above watermark), seq=3 (newest at/below)
        assert_eq!(
            results.len(),
            3,
            "Should keep 3 versions, got {:?}",
            results
        );

        // Verify order (newest first)
        assert_eq!(results[0].0, key_seq10);
        assert_eq!(results[1].0, key_seq7);
        assert_eq!(results[2].0, key_seq3);
    }
}
