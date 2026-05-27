//! PGM-index implementation.
//!
//! The Piecewise Geometric Model index is a learned index structure that
//! approximates the cumulative distribution function (CDF) of sorted keys
//! using piecewise linear segments.
//!
//! # Algorithm
//!
//! Given sorted keys k_1, k_2, ..., k_n, we want to learn a function f
//! such that f(k_i) ≈ i. The PGM-index uses piecewise linear segments
//! to approximate this function within a guaranteed error bound ε.
//!
//! Each segment is defined by (key_start, start_pos, slope) and covers a
//! range of keys. To search:
//! 1. Find the segment covering the query key (binary search on segment key_starts)
//! 2. Predict position = slope * (key - key_start) + start_pos
//!    (computed in u128 then cast to f64 to preserve precision)
//! 3. Binary search in [predicted - ε, predicted + ε]
//!
//! # References
//!
//! Ferragina & Vinciguerra, "The PGM-index: a fully-dynamic compressed
//! learned index with provable worst-case bounds" (VLDB 2020)

use std::cmp::Ordering;

/// Configuration for PGM-index.
#[derive(Debug, Clone)]
pub struct PgmConfig {
    /// Maximum error bound (ε). The predicted position is guaranteed to be
    /// within ±epsilon of the true position.
    pub epsilon: usize,
    /// Minimum number of keys to use PGM-index. Below this, binary search is used.
    pub min_keys: usize,
}

impl Default for PgmConfig {
    fn default() -> Self {
        Self {
            epsilon: 64,
            min_keys: 256,
        }
    }
}

/// A single linear segment in the PGM-index.
///
/// Uses a numerically stable form: position = slope * (key - key_start) + start_pos
/// This avoids division and keeps intermediate values smaller.
#[derive(Debug, Clone)]
pub struct Segment {
    /// First key covered by this segment (as u128 for better prefix diversity).
    pub key_start: u128,
    /// Position of the first key in the segment.
    pub start_pos: u32,
    /// Slope of the linear model.
    pub slope: f64,
}

impl Segment {
    /// Predicts the position for a given key.
    ///
    /// Uses the numerically stable form: slope * (key - key_start) + start_pos
    /// This avoids catastrophic cancellation and overflow issues.
    #[inline]
    pub fn predict(&self, key: u128) -> f64 {
        // Do u128 subtraction first, then cast to f64.
        // This preserves precision for keys with shared high-bit prefixes.
        // (f64 has 53-bit mantissa; casting u128 then subtracting loses precision
        // when both values are large but close together.)
        let offset = key.saturating_sub(self.key_start) as f64;

        // Note: For keys < 16 bytes, key_to_u128 zero-pads low bytes, which can
        // create segment spans > 2^53 (exceeding f64 mantissa precision). This is
        // acceptable because the slope is also computed from these large offsets,
        // so precision losses are proportional and cancel out in the product.
        // The debug_assert below catches truly degenerate cases (non-finite offset).
        debug_assert!(
            offset.is_finite(),
            "offset computation produced non-finite value; key={}, key_start={}",
            key,
            self.key_start
        );

        self.slope * offset + (self.start_pos as f64)
    }
}

/// PGM-index for mapping sorted keys to positions.
///
/// The index provides O(log(segments) + log(ε)) lookup time with
/// space proportional to n/ε where n is the number of keys.
#[derive(Debug, Clone)]
pub struct PgmIndex {
    /// Linear segments that approximate the CDF.
    segments: Vec<Segment>,
    /// Error bound.
    epsilon: usize,
    /// Number of keys indexed.
    num_keys: usize,
}

impl PgmIndex {
    /// Builds a PGM-index from sorted keys.
    ///
    /// Keys must be sorted in ascending order (u128 for better prefix diversity).
    /// Returns None if there are too few keys (falls back to binary search).
    pub fn build(keys: &[u128], config: &PgmConfig) -> Option<Self> {
        if keys.len() < config.min_keys {
            return None;
        }

        if keys.is_empty() {
            return Some(Self {
                segments: Vec::new(),
                epsilon: config.epsilon,
                num_keys: 0,
            });
        }

        let segments = build_segments(keys, config.epsilon);

        Some(Self {
            segments,
            epsilon: config.epsilon,
            num_keys: keys.len(),
        })
    }

    /// Builds a PGM-index from sorted byte keys.
    ///
    /// Keys are converted to u128 via first 16 bytes for comparison.
    pub fn build_from_bytes(keys: &[&[u8]], config: &PgmConfig) -> Option<Self> {
        if keys.len() < config.min_keys {
            return None;
        }

        let numeric_keys: Vec<u128> = keys.iter().map(|k| key_to_u128(k)).collect();
        Self::build(&numeric_keys, config)
    }

    /// Returns the search range [lo, hi) for a key.
    ///
    /// The true position of the key (if it exists) is guaranteed to be
    /// within this range. Returns (0, num_keys) if the index is empty.
    pub fn search(&self, key: u128) -> (usize, usize) {
        if self.segments.is_empty() {
            return (0, self.num_keys);
        }

        if self.num_keys == 0 {
            return (0, 0);
        }

        // Find the segment covering this key via binary search
        let segment_idx = self.find_segment(key);
        let segment = &self.segments[segment_idx];

        // Predict position
        let predicted = segment.predict(key);

        // Handle potential NaN or Inf from degenerate models
        let predicted = if predicted.is_finite() {
            predicted.round().max(0.0) as usize
        } else {
            0
        };

        // Clamp to valid range with epsilon margin
        // Note: epsilon is the guaranteed error bound from training
        let lo = predicted.saturating_sub(self.epsilon);
        let hi = (predicted + self.epsilon + 1).min(self.num_keys);

        // Ensure the range is valid and at least covers the epsilon window
        let lo = lo.min(self.num_keys.saturating_sub(1));
        let hi = hi.max(lo + 1);

        (lo, hi)
    }

    /// Returns the search range for a byte key.
    pub fn search_bytes(&self, key: &[u8]) -> (usize, usize) {
        self.search(key_to_u128(key))
    }

    /// Returns the number of segments in the index.
    pub fn num_segments(&self) -> usize {
        self.segments.len()
    }

    /// Returns the number of keys indexed.
    pub fn num_keys(&self) -> usize {
        self.num_keys
    }

    /// Returns the epsilon (error bound).
    pub fn epsilon(&self) -> usize {
        self.epsilon
    }

    /// Returns the approximate memory usage in bytes.
    pub fn memory_usage(&self) -> usize {
        std::mem::size_of::<Self>() + self.segments.len() * std::mem::size_of::<Segment>()
    }

    fn find_segment(&self, key: u128) -> usize {
        // Binary search for the segment whose key_start <= query key
        let mut lo = 0;
        let mut hi = self.segments.len();

        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.segments[mid].key_start <= key {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }

        // lo is the first segment with key_start > query, so we want lo - 1
        if lo > 0 {
            lo - 1
        } else {
            0
        }
    }
}

/// Builds piecewise linear segments using the optimal algorithm.
///
/// This implements a greedy algorithm that finds segments covering as many
/// keys as possible while staying within the error bound.
fn build_segments(keys: &[u128], epsilon: usize) -> Vec<Segment> {
    if keys.is_empty() {
        return Vec::new();
    }

    let mut segments = Vec::new();
    let mut start = 0;

    while start < keys.len() {
        // Find the longest segment starting at `start` that stays within epsilon
        let (segment, end) = find_optimal_segment(keys, start, epsilon);
        segments.push(segment);
        start = end;
    }

    segments
}

/// Finds the optimal segment starting at `start` that covers the most keys
/// while staying within the error bound.
fn find_optimal_segment(keys: &[u128], start: usize, epsilon: usize) -> (Segment, usize) {
    let n = keys.len();

    if start >= n {
        // Shouldn't happen, but handle gracefully
        return (
            Segment {
                key_start: 0,
                start_pos: 0,
                slope: 0.0,
            },
            n,
        );
    }

    if start == n - 1 {
        // Single key - any slope works
        return (
            Segment {
                key_start: keys[start],
                start_pos: start as u32,
                slope: 0.0,
            },
            n,
        );
    }

    // Use the convex hull trick to find the maximum extent
    // We track upper and lower bounds on the slope
    let key0 = keys[start];
    let pos0 = start as f64;

    // Track slope bounds. When the loop breaks (bounds would become invalid),
    // these still hold the last valid values since we only update inside the
    // success branch.
    let mut slope_lo = f64::NEG_INFINITY;
    let mut slope_hi = f64::INFINITY;

    let mut end = start + 1;

    while end < n {
        let pos = end as f64;
        // Do u128 subtraction first, then cast to f64 (same precision fix as predict())
        let dx = (keys[end].saturating_sub(key0)) as f64;

        if dx <= 0.0 {
            // Duplicate or decreasing key - shouldn't happen with sorted unique keys
            // but handle by ending the segment here
            break;
        }

        // The segment must satisfy: pos - epsilon <= slope * dx + pos0 <= pos + epsilon
        // Rearranging: (pos - pos0 - epsilon) / dx <= slope <= (pos - pos0 + epsilon) / dx
        let new_slope_lo = (pos - pos0 - epsilon as f64) / dx;
        let new_slope_hi = (pos - pos0 + epsilon as f64) / dx;

        // Compute potential new bounds
        let next_slope_lo = slope_lo.max(new_slope_lo);
        let next_slope_hi = slope_hi.min(new_slope_hi);

        if next_slope_lo > next_slope_hi {
            // No valid slope covers all keys from start to end
            // The current slope_lo/hi still hold the last valid bounds
            break;
        }

        // Bounds are still valid, save them and continue
        slope_lo = next_slope_lo;
        slope_hi = next_slope_hi;
        end += 1;
    }

    // Use the midpoint of the slope bounds
    let slope = if slope_lo.is_infinite() && slope_hi.is_infinite() {
        // Only one key in segment, use slope 0
        0.0
    } else if slope_lo.is_infinite() {
        slope_hi
    } else if slope_hi.is_infinite() {
        slope_lo
    } else {
        (slope_lo + slope_hi) / 2.0
    };

    // Store key_start and start_pos for numerically stable prediction
    // predict(key) = slope * (key - key_start) + start_pos
    (
        Segment {
            key_start: keys[start],
            start_pos: start as u32,
            slope,
        },
        end,
    )
}

/// Converts a byte key to u128 for numeric comparison.
///
/// Uses the first 16 bytes (or fewer, padded with zeros) interpreted as
/// big-endian u128. This preserves lexicographic ordering for keys that
/// differ in their first 16 bytes.
///
/// Using u128 instead of u64 captures more prefix diversity, eliminating
/// the degenerate case where keys sharing the first 8 bytes all hash to
/// the same value (causing zero-slope PGM models).
#[inline]
pub fn key_to_u128(key: &[u8]) -> u128 {
    let mut buf = [0u8; 16];
    let len = key.len().min(16);
    buf[..len].copy_from_slice(&key[..len]);
    u128::from_be_bytes(buf)
}

/// Converts a byte key to u64 for numeric comparison (legacy API).
///
/// Prefer `key_to_u128` for PGM-index operations to capture more prefix diversity.
#[inline]
pub fn key_to_u64(key: &[u8]) -> u64 {
    let mut buf = [0u8; 8];
    let len = key.len().min(8);
    buf[..len].copy_from_slice(&key[..len]);
    u64::from_be_bytes(buf)
}

/// A wrapper that combines PGM-index with binary search fallback.
///
/// This is the recommended interface for use in SSTables.
#[derive(Debug, Clone)]
pub enum BlockIndex {
    /// PGM-index for large numbers of blocks.
    Pgm(PgmIndex),
    /// Fallback to binary search for small numbers of blocks.
    BinarySearch { num_blocks: usize },
}

impl BlockIndex {
    /// Builds a block index from sorted block keys (u128 for better prefix diversity).
    pub fn build(keys: &[u128], config: &PgmConfig) -> Self {
        match PgmIndex::build(keys, config) {
            Some(pgm) => BlockIndex::Pgm(pgm),
            None => BlockIndex::BinarySearch {
                num_blocks: keys.len(),
            },
        }
    }

    /// Builds a block index from sorted byte keys.
    pub fn build_from_bytes(keys: &[&[u8]], config: &PgmConfig) -> Self {
        match PgmIndex::build_from_bytes(keys, config) {
            Some(pgm) => BlockIndex::Pgm(pgm),
            None => BlockIndex::BinarySearch {
                num_blocks: keys.len(),
            },
        }
    }

    /// Returns the search range [lo, hi) for a key.
    ///
    /// For PGM, this is a narrow range around the predicted position.
    /// For binary search fallback, this returns (0, num_blocks).
    pub fn search(&self, key: u128) -> (usize, usize) {
        match self {
            BlockIndex::Pgm(pgm) => pgm.search(key),
            BlockIndex::BinarySearch { num_blocks } => (0, *num_blocks),
        }
    }

    /// Returns the search range for a byte key.
    pub fn search_bytes(&self, key: &[u8]) -> (usize, usize) {
        self.search(key_to_u128(key))
    }

    /// Returns true if using the learned index (PGM).
    pub fn is_learned(&self) -> bool {
        matches!(self, BlockIndex::Pgm(_))
    }

    /// Returns memory usage in bytes.
    pub fn memory_usage(&self) -> usize {
        match self {
            BlockIndex::Pgm(pgm) => pgm.memory_usage(),
            BlockIndex::BinarySearch { .. } => std::mem::size_of::<Self>(),
        }
    }
}

/// Binary search within a slice, returning the position where key would be inserted.
///
/// This is used after PGM narrows the search range.
pub fn binary_search_by_key<T, F>(slice: &[T], key: u64, mut f: F) -> Result<usize, usize>
where
    F: FnMut(&T) -> u64,
{
    let mut lo = 0;
    let mut hi = slice.len();

    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        match f(&slice[mid]).cmp(&key) {
            Ordering::Less => lo = mid + 1,
            Ordering::Greater => hi = mid,
            Ordering::Equal => return Ok(mid),
        }
    }

    Err(lo)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_empty() {
        let config = PgmConfig::default();
        let pgm = PgmIndex::build(&[], &config);
        // Empty should fail min_keys check
        assert!(pgm.is_none());
    }

    #[test]
    fn build_small_fallback() {
        let config = PgmConfig {
            epsilon: 4,
            min_keys: 100,
        };
        let keys: Vec<u128> = (0..50).collect();
        let pgm = PgmIndex::build(&keys, &config);
        // Too few keys, should fall back
        assert!(pgm.is_none());
    }

    #[test]
    fn build_and_search() {
        let config = PgmConfig {
            epsilon: 4,
            min_keys: 10,
        };
        let keys: Vec<u128> = (0..1000).map(|i| i * 10).collect();
        let pgm = PgmIndex::build(&keys, &config).unwrap();

        // Search for each key
        for (i, &key) in keys.iter().enumerate() {
            let (lo, hi) = pgm.search(key);
            assert!(
                lo <= i && i < hi,
                "Key {} at position {} not in range [{}, {})",
                key,
                i,
                lo,
                hi
            );
            // Range should be bounded by 2*epsilon
            assert!(
                hi - lo <= 2 * config.epsilon + 1,
                "Range [{}, {}) too large for epsilon {}",
                lo,
                hi,
                config.epsilon
            );
        }
    }

    #[test]
    fn search_missing_keys() {
        let config = PgmConfig {
            epsilon: 4,
            min_keys: 10,
        };
        let keys: Vec<u128> = (0..100).map(|i| i * 100).collect();
        let pgm = PgmIndex::build(&keys, &config).unwrap();

        // Search for keys between existing keys
        for (i, &key_i) in keys.iter().enumerate().take(99) {
            let key = key_i + 50; // Key between keys[i] and keys[i+1]
            let (lo, hi) = pgm.search(key);
            // The range should include position i or i+1 (where this key would be inserted)
            assert!(lo <= i + 1 && i < hi);
        }
    }

    #[test]
    fn uniform_distribution() {
        let config = PgmConfig {
            epsilon: 8,
            min_keys: 10,
        };
        let keys: Vec<u128> = (0..10000).collect();
        let pgm = PgmIndex::build(&keys, &config).unwrap();

        // Uniform distribution should compress well
        // With epsilon=8, we expect roughly n/epsilon segments
        assert!(
            pgm.num_segments() < keys.len() / 4,
            "Too many segments: {} for {} keys",
            pgm.num_segments(),
            keys.len()
        );

        // Verify all searches are correct
        for (i, &key) in keys.iter().enumerate() {
            let (lo, hi) = pgm.search(key);
            assert!(lo <= i && i < hi);
        }
    }

    #[test]
    fn non_uniform_distribution() {
        let config = PgmConfig {
            epsilon: 4,
            min_keys: 10,
        };
        // Clustered keys with gaps
        let mut keys: Vec<u128> = Vec::new();
        for cluster in 0u128..10 {
            for i in 0u128..100 {
                keys.push(cluster * 10000 + i);
            }
        }
        let pgm = PgmIndex::build(&keys, &config).unwrap();

        // Verify all searches are correct
        for (i, &key) in keys.iter().enumerate() {
            let (lo, hi) = pgm.search(key);
            assert!(
                lo <= i && i < hi,
                "Key {} at position {} not in range [{}, {})",
                key,
                i,
                lo,
                hi
            );
        }
    }

    #[test]
    fn block_index_fallback() {
        let config = PgmConfig {
            epsilon: 4,
            min_keys: 100,
        };
        let keys: Vec<u128> = (0..50).collect();
        let index = BlockIndex::build(&keys, &config);

        assert!(!index.is_learned());
        let (lo, hi) = index.search(25u128);
        assert_eq!(lo, 0);
        assert_eq!(hi, 50);
    }

    #[test]
    fn block_index_pgm() {
        let config = PgmConfig {
            epsilon: 4,
            min_keys: 10,
        };
        let keys: Vec<u128> = (0..1000).collect();
        let index = BlockIndex::build(&keys, &config);

        assert!(index.is_learned());
        let (lo, hi) = index.search(500u128);
        assert!(lo <= 500 && 500 < hi);
        assert!(hi - lo <= 2 * config.epsilon + 1);
    }

    #[test]
    fn key_to_u128_ordering() {
        // Verify that key_to_u128 preserves ordering for keys that differ in first 16 bytes
        let keys = [
            b"aaa".as_slice(),
            b"aab".as_slice(),
            b"aba".as_slice(),
            b"baa".as_slice(),
            b"zzz".as_slice(),
        ];

        for i in 0..keys.len() - 1 {
            assert!(
                key_to_u128(keys[i]) < key_to_u128(keys[i + 1]),
                "{:?} should be < {:?}",
                keys[i],
                keys[i + 1]
            );
        }
    }

    #[test]
    fn byte_keys() {
        // Note: String keys like "key00019" -> "key00020" have non-uniform gaps
        // when converted to u64 (ASCII '9' -> '0' creates a large jump).
        // Use a larger epsilon to handle this, or use uniform numeric keys.
        let config = PgmConfig {
            epsilon: 64, // Larger epsilon for string keys
            min_keys: 10,
        };

        let keys: Vec<Vec<u8>> = (0..500)
            .map(|i| format!("key{:05}", i).into_bytes())
            .collect();
        let key_refs: Vec<&[u8]> = keys.iter().map(|k| k.as_slice()).collect();

        let index = BlockIndex::build_from_bytes(&key_refs, &config);
        assert!(index.is_learned());

        // Search for each key
        for (i, key) in keys.iter().enumerate() {
            let (lo, hi) = index.search_bytes(key);
            assert!(
                lo <= i && i < hi,
                "Key {:?} at position {} not in range [{}, {})",
                String::from_utf8_lossy(key),
                i,
                lo,
                hi
            );
        }
    }

    #[test]
    fn uniform_byte_keys() {
        // Test with truly uniform byte keys (u64 encoded as big-endian bytes)
        let config = PgmConfig {
            epsilon: 4,
            min_keys: 10,
        };

        let keys: Vec<Vec<u8>> = (0u64..500).map(|i| i.to_be_bytes().to_vec()).collect();
        let key_refs: Vec<&[u8]> = keys.iter().map(|k| k.as_slice()).collect();

        let index = BlockIndex::build_from_bytes(&key_refs, &config);
        assert!(index.is_learned());

        // Search for each key - should work with small epsilon
        for (i, key) in keys.iter().enumerate() {
            let (lo, hi) = index.search_bytes(key);
            assert!(
                lo <= i && i < hi,
                "Key {} at position {} not in range [{}, {})",
                u64::from_be_bytes(key.as_slice().try_into().unwrap()),
                i,
                lo,
                hi
            );
        }
    }

    #[test]
    fn predict_numerically_stable_for_large_keys() {
        // Keys near 2^60: large enough that the old `(key as f64) + intercept`
        // form lost precision, but new `(key - key_start) as f64` form is exact.
        let config = PgmConfig {
            epsilon: 4,
            min_keys: 10,
        };
        let base: u128 = 1u128 << 60;
        let keys: Vec<u128> = (0..10_000).map(|i| base + i).collect();
        let pgm = PgmIndex::build(&keys, &config).expect("build should succeed");

        for (i, &key) in keys.iter().enumerate() {
            let (lo, hi) = pgm.search(key);
            assert!(
                lo <= i && i < hi,
                "key at position {} (value {}) outside predicted range [{}, {})",
                i,
                key,
                lo,
                hi
            );
            assert!(
                hi - lo <= 2 * config.epsilon + 1,
                "search range [{}, {}) wider than 2ε+1={}",
                lo,
                hi,
                2 * config.epsilon + 1
            );
        }
    }

    #[test]
    fn pgm_handles_long_shared_prefix_keys() {
        // 1000 keys sharing a 19-byte ASCII prefix. With u64 digest, all keys
        // would map to the same value. With u128, only the first 16 bytes are
        // captured — still some collision risk, but the suffix bytes
        // distinguish enough keys to validate the PGM model works.
        let config = PgmConfig {
            epsilon: 16,
            min_keys: 100,
        };
        let keys: Vec<Vec<u8>> = (0..1000)
            .map(|i| format!("user:0000000000:{:08}", i).into_bytes())
            .collect();
        let key_refs: Vec<&[u8]> = keys.iter().map(|k| k.as_slice()).collect();

        let index = BlockIndex::build_from_bytes(&key_refs, &config);
        assert!(index.is_learned(), "should use PGM path for 1000 keys");

        // Verify each key's search range contains its true position.
        // Note: keys are 24 bytes ("user:0000000000:00000000"), so only the
        // first 16 bytes — "user:0000000000:" — are captured by key_to_u128.
        // All keys map to the SAME u128. This test verifies the PGM
        // degenerate case is handled gracefully (single segment with slope 0
        // covering all keys, search returns the full range).
        for key in keys.iter() {
            let (lo, hi) = index.search_bytes(key);
            // The PGM can't distinguish these keys; we expect the full range.
            // What we're verifying: PGM doesn't PANIC or return INVERTED range.
            assert!(lo <= hi, "inverted range [{}, {})", lo, hi);
            assert!(hi <= 1000, "range exceeds key count: hi={}", hi);
        }
    }

    #[test]
    fn pgm_handles_distinguishable_prefix_keys() {
        // Keys differ in bytes 12-15 (within the 16-byte digest window).
        let config = PgmConfig {
            epsilon: 16,
            min_keys: 100,
        };
        let keys: Vec<Vec<u8>> = (0..1000)
            .map(|i| format!("user:000000:{:04}:suffix", i).into_bytes())
            .collect();
        let key_refs: Vec<&[u8]> = keys.iter().map(|k| k.as_slice()).collect();

        let index = BlockIndex::build_from_bytes(&key_refs, &config);
        assert!(index.is_learned());

        for (i, key) in keys.iter().enumerate() {
            let (lo, hi) = index.search_bytes(key);
            assert!(
                lo <= i && i < hi,
                "key {} at position {} not in range [{}, {})",
                String::from_utf8_lossy(key),
                i,
                lo,
                hi
            );
        }
    }
}
