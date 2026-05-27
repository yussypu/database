//! Write-Ahead Log implementation.
//!
//! The WAL provides durability by ensuring all writes are persisted to disk
//! before being acknowledged. It uses CRC32C checksums for integrity.
//!
//! # Record Format
//!
//! Each record has the following format:
//! ```text
//! +----------+----------+----------+----------------+
//! | CRC (4B) | Len (4B) | Type (1B)| Payload (Len B)|
//! +----------+----------+----------+----------------+
//! ```
//!
//! - CRC: CRC32C checksum of Type + Payload
//! - Len: Length of payload in bytes (u32 little-endian)
//! - Type: Record type (Full, First, Middle, Last)
//! - Payload: The actual data
//!
//! # Segment Files
//!
//! The WAL is split into segment files. Each segment has a maximum size.
//! When a segment is full, a new one is created. Old segments can be
//! garbage collected after their data is flushed to SSTables.
//!
//! # TLA+ Spec Reference
//!
//! See `specs/Storage.tla` for the formal specification of WAL behavior.
//! Key invariant: after recovery, durable state equals committed state at crash time.

use crate::error::{Error, Result};
use runtime::{Env, File, OpenOptions, Path, PathBuf};

/// Size of the record header (CRC + Length + Type).
const HEADER_SIZE: usize = 9;

/// Default maximum segment size (64 MB).
pub const DEFAULT_SEGMENT_SIZE: u64 = 64 * 1024 * 1024;

/// Record types for chunked records.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordType {
    /// A complete record that fits in one chunk.
    Full = 1,
    /// First chunk of a record that spans multiple chunks.
    First = 2,
    /// Middle chunk of a record that spans multiple chunks.
    Middle = 3,
    /// Last chunk of a record that spans multiple chunks.
    Last = 4,
}

impl RecordType {
    fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(RecordType::Full),
            2 => Some(RecordType::First),
            3 => Some(RecordType::Middle),
            4 => Some(RecordType::Last),
            _ => None,
        }
    }
}

/// A single WAL record.
#[derive(Debug, Clone)]
pub struct WalRecord {
    /// The record payload.
    pub data: Vec<u8>,
}

impl WalRecord {
    /// Creates a new WAL record with the given data.
    pub fn new(data: Vec<u8>) -> Self {
        Self { data }
    }
}

/// Configuration for the WAL.
#[derive(Debug, Clone)]
pub struct WalConfig {
    /// Maximum size of each segment file in bytes.
    pub segment_size: u64,
    /// Whether to sync after each write (slower but safer).
    pub sync_on_write: bool,
}

impl Default for WalConfig {
    fn default() -> Self {
        Self {
            segment_size: DEFAULT_SEGMENT_SIZE,
            sync_on_write: false,
        }
    }
}

/// Write-Ahead Log writer.
///
/// Handles appending records to the WAL with proper checksumming and
/// segment rotation.
pub struct WalWriter<E: Env> {
    env: E,
    dir: PathBuf,
    config: WalConfig,
    /// Current segment file.
    current_segment: Option<E::File>,
    /// Current segment number.
    current_segment_num: u64,
    /// Current write position in the segment.
    current_offset: u64,
    /// Buffer for building records.
    write_buffer: Vec<u8>,
}

impl<E: Env> WalWriter<E> {
    /// Creates a new WAL writer.
    ///
    /// If the directory contains existing WAL segments, this will continue
    /// from the highest segment number.
    pub fn new(env: E, dir: &Path, config: WalConfig) -> Result<Self> {
        // Ensure directory exists
        if !env.exists(dir) {
            env.create_dir_all(dir)?;
        }

        // Find existing segments
        let segments = list_segments(&env, dir)?;
        let (segment_num, offset) = if let Some(&last) = segments.last() {
            // Open the last segment and find its size
            let path = segment_path(dir, last);
            if env.exists(&path) {
                let file = env.open(&path, OpenOptions::read())?;
                let len = file.len()?;
                (last, len)
            } else {
                (last + 1, 0)
            }
        } else {
            (0, 0)
        };

        let mut writer = Self {
            env,
            dir: dir.to_path_buf(),
            config,
            current_segment: None,
            current_segment_num: segment_num,
            current_offset: offset,
            write_buffer: Vec::with_capacity(4096),
        };

        // Open or create current segment
        writer.ensure_segment()?;

        Ok(writer)
    }

    /// Appends a record to the WAL.
    ///
    /// Returns the (segment_num, offset) where the record was written.
    pub fn append(&mut self, record: &WalRecord) -> Result<(u64, u64)> {
        let start_segment = self.current_segment_num;
        let start_offset = self.current_offset;

        let data = &record.data;

        if data.is_empty() {
            // Write a single empty Full record
            self.write_chunk(RecordType::Full, &[])?;
        } else if data.len() + HEADER_SIZE <= self.remaining_in_segment() as usize {
            // Fits in current segment as a single record
            self.write_chunk(RecordType::Full, data)?;
        } else {
            // Need to split across chunks
            let mut offset = 0;
            let mut first = true;

            while offset < data.len() {
                let remaining = self.remaining_in_segment() as usize;
                if remaining < HEADER_SIZE {
                    // Not enough space for even a header, rotate
                    self.rotate_segment()?;
                    continue;
                }

                let available = remaining - HEADER_SIZE;
                let chunk_size = (data.len() - offset).min(available);
                let chunk = &data[offset..offset + chunk_size];
                let is_last = offset + chunk_size >= data.len();

                let record_type = if first && is_last {
                    RecordType::Full
                } else if first {
                    RecordType::First
                } else if is_last {
                    RecordType::Last
                } else {
                    RecordType::Middle
                };

                self.write_chunk(record_type, chunk)?;
                offset += chunk_size;
                first = false;
            }
        }

        Ok((start_segment, start_offset))
    }

    /// Syncs the WAL to durable storage.
    ///
    /// After this returns, all previously written records are guaranteed
    /// to survive a crash.
    pub fn sync(&mut self) -> Result<()> {
        if let Some(ref file) = self.current_segment {
            file.sync()?;
        }
        Ok(())
    }

    /// Returns the current segment number.
    pub fn current_segment(&self) -> u64 {
        self.current_segment_num
    }

    /// Returns the current offset in the current segment.
    pub fn current_offset(&self) -> u64 {
        self.current_offset
    }

    fn remaining_in_segment(&self) -> u64 {
        self.config.segment_size.saturating_sub(self.current_offset)
    }

    fn ensure_segment(&mut self) -> Result<()> {
        if self.current_segment.is_none() {
            let path = segment_path(&self.dir, self.current_segment_num);
            let file = self.env.open(&path, OpenOptions::write())?;
            self.current_segment = Some(file);
        }
        Ok(())
    }

    fn rotate_segment(&mut self) -> Result<()> {
        // Sync and close current segment
        if let Some(ref file) = self.current_segment {
            file.sync()?;
        }
        self.current_segment = None;

        // Start new segment
        self.current_segment_num += 1;
        self.current_offset = 0;
        self.ensure_segment()?;

        Ok(())
    }

    fn write_chunk(&mut self, record_type: RecordType, data: &[u8]) -> Result<()> {
        self.ensure_segment()?;

        // Check if we need to rotate
        let record_size = HEADER_SIZE + data.len();
        if self.current_offset + record_size as u64 > self.config.segment_size {
            self.rotate_segment()?;
        }

        // Build the record
        self.write_buffer.clear();

        // Compute CRC of type + data
        let crc = {
            let mut hasher = crc32c::crc32c(&[record_type as u8]);
            hasher = crc32c::crc32c_append(hasher, data);
            hasher
        };

        // Write header: CRC (4) + Length (4) + Type (1)
        self.write_buffer.extend_from_slice(&crc.to_le_bytes());
        self.write_buffer
            .extend_from_slice(&(data.len() as u32).to_le_bytes());
        self.write_buffer.push(record_type as u8);

        // Write payload
        self.write_buffer.extend_from_slice(data);

        // Write to file
        let file = self.current_segment.as_ref().unwrap();
        file.write_all_at(&self.write_buffer, self.current_offset)?;

        self.current_offset += self.write_buffer.len() as u64;

        if self.config.sync_on_write {
            file.sync()?;
        }

        Ok(())
    }
}

/// WAL reader for recovery.
///
/// Reads records from WAL segments in order, handling chunked records
/// and detecting corruption.
pub struct WalReader<E: Env> {
    env: E,
    dir: PathBuf,
    /// List of segment numbers to read.
    segments: Vec<u64>,
    /// Current segment index.
    current_segment_idx: usize,
    /// Current file handle.
    current_file: Option<E::File>,
    /// Current read position.
    current_offset: u64,
    /// Current segment size.
    current_segment_size: u64,
    /// Buffer for reading headers.
    header_buf: [u8; HEADER_SIZE],
    /// Buffer for accumulating chunked records.
    record_buf: Vec<u8>,
}

impl<E: Env> WalReader<E> {
    /// Creates a new WAL reader starting from the given segment.
    pub fn new(env: E, dir: &Path, start_segment: u64) -> Result<Self> {
        let segments = list_segments(&env, dir)?;
        let segments: Vec<u64> = segments
            .into_iter()
            .filter(|&s| s >= start_segment)
            .collect();

        Ok(Self {
            env,
            dir: dir.to_path_buf(),
            segments,
            current_segment_idx: 0,
            current_file: None,
            current_offset: 0,
            current_segment_size: 0,
            header_buf: [0u8; HEADER_SIZE],
            record_buf: Vec::new(),
        })
    }

    /// Creates a reader that reads all segments.
    pub fn new_from_start(env: E, dir: &Path) -> Result<Self> {
        Self::new(env, dir, 0)
    }

    /// Reads the next record from the WAL.
    ///
    /// Returns `None` at EOF. Returns an error if corruption is detected
    /// (the caller should truncate at this point during recovery).
    pub fn read_record(&mut self) -> Result<Option<WalRecord>> {
        self.record_buf.clear();

        loop {
            // Ensure we have a segment open
            if self.current_file.is_none() {
                if self.current_segment_idx >= self.segments.len() {
                    return Ok(None);
                }

                let segment_num = self.segments[self.current_segment_idx];
                let path = segment_path(&self.dir, segment_num);
                let file = self.env.open(&path, OpenOptions::read())?;
                self.current_segment_size = file.len()?;
                self.current_file = Some(file);
                self.current_offset = 0;
            }

            // Check if we've reached the end of the segment
            if self.current_offset + HEADER_SIZE as u64 > self.current_segment_size {
                // Move to next segment
                self.current_file = None;
                self.current_segment_idx += 1;

                // If we're accumulating chunks and there are no more segments, it's an error
                if !self.record_buf.is_empty() && self.current_segment_idx >= self.segments.len() {
                    return Err(Error::Corruption(
                        "incomplete chunked record at end of WAL".to_string(),
                    ));
                }
                // Otherwise continue to next segment (might have more chunks there)
                continue;
            }

            // Read header
            let file = self.current_file.as_ref().unwrap();
            match file.read_exact_at(&mut self.header_buf, self.current_offset) {
                Ok(()) => {}
                Err(_) => {
                    // Partial header read - EOF or corruption
                    if self.record_buf.is_empty() {
                        // Clean EOF
                        self.current_file = None;
                        self.current_segment_idx += 1;
                        continue;
                    } else {
                        return Err(Error::Corruption(
                            "incomplete header in chunked record".to_string(),
                        ));
                    }
                }
            }

            // Parse header
            let crc = u32::from_le_bytes(self.header_buf[0..4].try_into().unwrap());
            let len = u32::from_le_bytes(self.header_buf[4..8].try_into().unwrap()) as usize;
            let record_type = match RecordType::from_u8(self.header_buf[8]) {
                Some(t) => t,
                None => {
                    return Err(Error::Corruption(format!(
                        "invalid record type: {}",
                        self.header_buf[8]
                    )));
                }
            };

            // Check if we have enough data
            if self.current_offset + HEADER_SIZE as u64 + len as u64 > self.current_segment_size {
                return Err(Error::Corruption(
                    "record extends past segment end".to_string(),
                ));
            }

            // Read payload
            let mut payload = vec![0u8; len];
            if len > 0 {
                file.read_exact_at(&mut payload, self.current_offset + HEADER_SIZE as u64)?;
            }

            // Verify CRC
            let expected_crc = {
                let mut hasher = crc32c::crc32c(&[record_type as u8]);
                hasher = crc32c::crc32c_append(hasher, &payload);
                hasher
            };

            if crc != expected_crc {
                return Err(Error::Corruption(format!(
                    "CRC mismatch: expected {}, got {}",
                    expected_crc, crc
                )));
            }

            // Advance position
            self.current_offset += HEADER_SIZE as u64 + len as u64;

            // Handle record type
            match record_type {
                RecordType::Full => {
                    if !self.record_buf.is_empty() {
                        return Err(Error::Corruption(
                            "Full record while accumulating chunks".to_string(),
                        ));
                    }
                    return Ok(Some(WalRecord::new(payload)));
                }
                RecordType::First => {
                    if !self.record_buf.is_empty() {
                        return Err(Error::Corruption(
                            "First record while accumulating chunks".to_string(),
                        ));
                    }
                    self.record_buf.extend_from_slice(&payload);
                }
                RecordType::Middle => {
                    if self.record_buf.is_empty() {
                        return Err(Error::Corruption("Middle record without First".to_string()));
                    }
                    self.record_buf.extend_from_slice(&payload);
                }
                RecordType::Last => {
                    if self.record_buf.is_empty() {
                        return Err(Error::Corruption("Last record without First".to_string()));
                    }
                    self.record_buf.extend_from_slice(&payload);
                    let data = std::mem::take(&mut self.record_buf);
                    return Ok(Some(WalRecord::new(data)));
                }
            }
        }
    }

    /// Returns the current position as (segment_num, offset).
    pub fn position(&self) -> (u64, u64) {
        if self.current_segment_idx < self.segments.len() {
            (self.segments[self.current_segment_idx], self.current_offset)
        } else if !self.segments.is_empty() {
            (
                self.segments[self.segments.len() - 1],
                self.current_segment_size,
            )
        } else {
            (0, 0)
        }
    }
}

/// Lists WAL segment numbers in a directory, sorted ascending.
fn list_segments<E: Env>(env: &E, dir: &Path) -> Result<Vec<u64>> {
    if !env.exists(dir) {
        return Ok(Vec::new());
    }

    let entries = env.list_dir(dir)?;
    let mut segments = Vec::new();

    for entry in entries {
        if let Some(name) = entry.file_name() {
            if let Some(name_str) = name.to_str() {
                if let Some(num_str) = name_str.strip_suffix(".wal") {
                    if let Ok(num) = num_str.parse::<u64>() {
                        segments.push(num);
                    }
                }
            }
        }
    }

    segments.sort();
    Ok(segments)
}

/// Returns the path to a WAL segment file.
fn segment_path(dir: &Path, segment_num: u64) -> PathBuf {
    dir.join(format!("{:010}.wal", segment_num))
}

/// Truncates a WAL segment at the given offset.
///
/// Used during recovery to remove corrupted data at the end of a segment.
pub fn truncate_segment<E: Env>(env: &E, dir: &Path, segment_num: u64, offset: u64) -> Result<()> {
    let path = segment_path(dir, segment_num);
    let file = env.open(&path, OpenOptions::write())?;
    file.truncate(offset)?;
    file.sync()?;
    Ok(())
}

/// Deletes WAL segments up to (but not including) the given segment number.
///
/// Called after data has been flushed to SSTables.
pub fn delete_segments_before<E: Env>(env: &E, dir: &Path, before_segment: u64) -> Result<()> {
    let segments = list_segments(env, dir)?;
    for segment_num in segments {
        if segment_num < before_segment {
            let path = segment_path(dir, segment_num);
            env.remove(&path)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use runtime::{SimEnv, SimEnvConfig};

    fn test_env() -> SimEnv {
        SimEnv::new(SimEnvConfig::with_seed(42))
    }

    #[test]
    fn write_and_read_single_record() {
        let env = test_env();
        let dir = Path::new("/wal");

        // Write
        let mut writer = WalWriter::new(env.clone(), dir, WalConfig::default()).unwrap();
        let record = WalRecord::new(b"hello world".to_vec());
        writer.append(&record).unwrap();
        writer.sync().unwrap();

        // Read
        let mut reader = WalReader::new_from_start(env, dir).unwrap();
        let read_record = reader.read_record().unwrap().unwrap();
        assert_eq!(read_record.data, b"hello world");

        // Should be EOF
        assert!(reader.read_record().unwrap().is_none());
    }

    #[test]
    fn write_and_read_multiple_records() {
        let env = test_env();
        let dir = Path::new("/wal");

        // Write multiple records
        let mut writer = WalWriter::new(env.clone(), dir, WalConfig::default()).unwrap();
        for i in 0..100 {
            let record = WalRecord::new(format!("record {}", i).into_bytes());
            writer.append(&record).unwrap();
        }
        writer.sync().unwrap();

        // Read them back
        let mut reader = WalReader::new_from_start(env, dir).unwrap();
        for i in 0..100 {
            let record = reader.read_record().unwrap().unwrap();
            assert_eq!(record.data, format!("record {}", i).into_bytes());
        }
        assert!(reader.read_record().unwrap().is_none());
    }

    #[test]
    fn segment_rotation() {
        let env = test_env();
        let dir = Path::new("/wal");

        // Use a small segment size to force rotation
        let config = WalConfig {
            segment_size: 256,
            sync_on_write: false,
        };

        let mut writer = WalWriter::new(env.clone(), dir, config).unwrap();

        // Write enough data to span multiple segments
        for i in 0..50 {
            let data = format!("record {:05}", i); // Fixed length for predictability
            let record = WalRecord::new(data.into_bytes());
            writer.append(&record).unwrap();
        }
        writer.sync().unwrap();

        // Should have created multiple segments
        assert!(writer.current_segment() > 0);

        // Read them back
        let mut reader = WalReader::new_from_start(env, dir).unwrap();
        for i in 0..50 {
            let record = reader.read_record().unwrap().unwrap();
            assert_eq!(record.data, format!("record {:05}", i).into_bytes());
        }
        assert!(reader.read_record().unwrap().is_none());
    }

    #[test]
    fn crash_recovery_truncates_partial_write() {
        let env = test_env();
        let dir = Path::new("/wal");

        // Write some records and sync
        let mut writer = WalWriter::new(env.clone(), dir, WalConfig::default()).unwrap();
        for i in 0..5 {
            let record = WalRecord::new(format!("committed {}", i).into_bytes());
            writer.append(&record).unwrap();
        }
        writer.sync().unwrap();
        let _synced_offset = writer.current_offset();

        // Write more records but don't sync
        for i in 0..3 {
            let record = WalRecord::new(format!("uncommitted {}", i).into_bytes());
            writer.append(&record).unwrap();
        }

        // Simulate crash
        drop(writer);
        env.simulate_crash();

        // Recovery should only see committed records
        let mut reader = WalReader::new_from_start(env, dir).unwrap();
        for i in 0..5 {
            let record = reader.read_record().unwrap().unwrap();
            assert_eq!(record.data, format!("committed {}", i).into_bytes());
        }

        // Should be EOF (uncommitted records lost)
        assert!(reader.read_record().unwrap().is_none());
    }

    #[test]
    fn detects_corrupted_crc() {
        let env = test_env();
        let dir = Path::new("/wal");

        // Write a record
        let mut writer = WalWriter::new(env.clone(), dir, WalConfig::default()).unwrap();
        let record = WalRecord::new(b"hello".to_vec());
        writer.append(&record).unwrap();
        writer.sync().unwrap();

        // Corrupt the CRC (first 4 bytes)
        let segment_path = dir.join("0000000000.wal");
        let file = env.open(&segment_path, OpenOptions::write()).unwrap();
        file.write_all_at(&[0xFF, 0xFF, 0xFF, 0xFF], 0).unwrap();
        file.sync().unwrap();

        // Reading should fail with corruption error
        let mut reader = WalReader::new_from_start(env, dir).unwrap();
        let result = reader.read_record();
        assert!(matches!(result, Err(Error::Corruption(_))));
    }

    #[test]
    fn empty_record() {
        let env = test_env();
        let dir = Path::new("/wal");

        // Write an empty record
        let mut writer = WalWriter::new(env.clone(), dir, WalConfig::default()).unwrap();
        let record = WalRecord::new(vec![]);
        writer.append(&record).unwrap();
        writer.sync().unwrap();

        // Read it back
        let mut reader = WalReader::new_from_start(env, dir).unwrap();
        let read_record = reader.read_record().unwrap().unwrap();
        assert!(read_record.data.is_empty());
    }

    #[test]
    fn large_record_spans_segments() {
        let env = test_env();
        let dir = Path::new("/wal");

        // Use a small segment size
        let config = WalConfig {
            segment_size: 64,
            sync_on_write: false,
        };

        let mut writer = WalWriter::new(env.clone(), dir, config).unwrap();

        // Write a record larger than segment size
        let large_data: Vec<u8> = (0..200).map(|i| (i % 256) as u8).collect();
        let record = WalRecord::new(large_data.clone());
        writer.append(&record).unwrap();
        writer.sync().unwrap();

        // Should have created multiple segments
        assert!(writer.current_segment() > 0);

        // Read it back
        let mut reader = WalReader::new_from_start(env, dir).unwrap();
        let read_record = reader.read_record().unwrap().unwrap();
        assert_eq!(read_record.data, large_data);
    }
}
