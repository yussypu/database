//! Classical Bloom filter implementation.
//!
//! This module provides a standard Bloom filter for membership testing.
//!
//! **Note:** Learned bloom filters (per Mitzenmacher 2018) are deferred to
//! Phase 2b. See ADR-008 in DECISIONS.md for rationale.
//!
//! # Usage
//!
//! ```rust
//! use learned::bloom::{BloomFilter, BloomConfig};
//!
//! let config = BloomConfig::default();
//! let mut filter = BloomFilter::new(1000, &config);
//!
//! filter.insert(b"key1");
//! filter.insert(b"key2");
//!
//! assert!(filter.may_contain(b"key1"));
//! assert!(filter.may_contain(b"key2"));
//! // may_contain can have false positives but never false negatives
//! ```

/// Configuration for Bloom filters.
#[derive(Debug, Clone)]
pub struct BloomConfig {
    /// Target false positive rate (e.g., 0.01 for 1%).
    pub false_positive_rate: f64,
}

impl Default for BloomConfig {
    fn default() -> Self {
        Self {
            false_positive_rate: 0.01,
        }
    }
}

/// A standard Bloom filter for membership testing.
///
/// Uses multiple hash functions to set bits in a bit array.
/// False positives are possible; false negatives are not.
#[derive(Debug, Clone)]
pub struct BloomFilter {
    /// Bit array.
    bits: Vec<u64>,
    /// Number of bits in the filter.
    num_bits: usize,
    /// Number of hash functions.
    num_hashes: u32,
}

impl BloomFilter {
    /// Creates a new Bloom filter sized for the expected number of keys.
    pub fn new(expected_keys: usize, config: &BloomConfig) -> Self {
        let (num_bits, num_hashes) = optimal_params(expected_keys, config.false_positive_rate);

        Self {
            bits: vec![0; (num_bits + 63) / 64],
            num_bits,
            num_hashes,
        }
    }

    /// Creates a Bloom filter with explicit parameters.
    pub fn with_params(num_bits: usize, num_hashes: u32) -> Self {
        Self {
            bits: vec![0; (num_bits + 63) / 64],
            num_bits,
            num_hashes,
        }
    }

    /// Deserializes a Bloom filter from raw bytes.
    ///
    /// Format: [num_bits: u32][num_hashes: u32][bits...]
    pub fn from_bytes(data: &[u8]) -> Option<Self> {
        if data.len() < 8 {
            return None;
        }

        let num_bits = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
        let num_hashes = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);

        let expected_words = (num_bits + 63) / 64;
        let expected_bytes = 8 + expected_words * 8;

        if data.len() < expected_bytes {
            return None;
        }

        let mut bits = Vec::with_capacity(expected_words);
        for i in 0..expected_words {
            let offset = 8 + i * 8;
            let word = u64::from_le_bytes([
                data[offset],
                data[offset + 1],
                data[offset + 2],
                data[offset + 3],
                data[offset + 4],
                data[offset + 5],
                data[offset + 6],
                data[offset + 7],
            ]);
            bits.push(word);
        }

        Some(Self {
            bits,
            num_bits,
            num_hashes,
        })
    }

    /// Serializes the Bloom filter to bytes.
    ///
    /// Format: [num_bits: u32][num_hashes: u32][bits...]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut result = Vec::with_capacity(8 + self.bits.len() * 8);
        result.extend_from_slice(&(self.num_bits as u32).to_le_bytes());
        result.extend_from_slice(&self.num_hashes.to_le_bytes());
        for &word in &self.bits {
            result.extend_from_slice(&word.to_le_bytes());
        }
        result
    }

    /// Inserts a key into the filter.
    pub fn insert(&mut self, key: &[u8]) {
        let (h1, h2) = hash_key(key);

        for i in 0..self.num_hashes {
            let bit_idx = self.get_bit_index(h1, h2, i);
            self.set_bit(bit_idx);
        }
    }

    /// Tests if a key might be in the filter.
    ///
    /// Returns `true` if the key might be present (possible false positive).
    /// Returns `false` if the key is definitely not present.
    pub fn may_contain(&self, key: &[u8]) -> bool {
        let (h1, h2) = hash_key(key);

        for i in 0..self.num_hashes {
            let bit_idx = self.get_bit_index(h1, h2, i);
            if !self.get_bit(bit_idx) {
                return false;
            }
        }

        true
    }

    /// Returns the number of bits in the filter.
    pub fn num_bits(&self) -> usize {
        self.num_bits
    }

    /// Returns the number of hash functions.
    pub fn num_hashes(&self) -> u32 {
        self.num_hashes
    }

    /// Returns the serialized size in bytes.
    pub fn serialized_size(&self) -> usize {
        8 + self.bits.len() * 8
    }

    /// Returns the memory usage in bytes.
    pub fn memory_usage(&self) -> usize {
        self.bits.len() * 8 + std::mem::size_of::<Self>()
    }

    /// Returns the approximate fill ratio (fraction of bits set).
    pub fn fill_ratio(&self) -> f64 {
        let set_bits: usize = self.bits.iter().map(|w| w.count_ones() as usize).sum();
        set_bits as f64 / self.num_bits as f64
    }

    #[inline]
    fn get_bit_index(&self, h1: u64, h2: u64, i: u32) -> usize {
        // Double hashing: h(i) = h1 + i*h2
        let hash = h1.wrapping_add((i as u64).wrapping_mul(h2));
        (hash % self.num_bits as u64) as usize
    }

    #[inline]
    fn set_bit(&mut self, idx: usize) {
        let word = idx / 64;
        let bit = idx % 64;
        self.bits[word] |= 1 << bit;
    }

    #[inline]
    fn get_bit(&self, idx: usize) -> bool {
        let word = idx / 64;
        let bit = idx % 64;
        (self.bits[word] >> bit) & 1 == 1
    }
}

/// Computes optimal Bloom filter parameters.
fn optimal_params(n: usize, fpr: f64) -> (usize, u32) {
    let n = n.max(1) as f64;

    // Optimal number of bits: m = -n * ln(p) / (ln(2)^2)
    let m = (-n * fpr.ln() / (2.0_f64.ln().powi(2))).ceil() as usize;
    let m = m.max(64); // Minimum 64 bits

    // Optimal number of hashes: k = (m/n) * ln(2)
    let k = ((m as f64 / n) * 2.0_f64.ln()).round() as u32;
    let k = k.clamp(1, 30); // Reasonable bounds

    (m, k)
}

/// Computes two hash values for double hashing.
fn hash_key(key: &[u8]) -> (u64, u64) {
    // Use xxHash-style mixing for speed
    let mut h1 = 0xcbf29ce484222325u64; // FNV offset
    let mut h2 = 0x100000001b3u64; // FNV prime

    for &byte in key {
        h1 ^= byte as u64;
        h1 = h1.wrapping_mul(0x100000001b3);

        h2 = h2.wrapping_add(byte as u64);
        h2 = h2.rotate_left(13);
        h2 ^= h2 >> 7;
    }

    // Additional mixing
    h1 ^= h1 >> 33;
    h1 = h1.wrapping_mul(0xff51afd7ed558ccd);
    h1 ^= h1 >> 33;

    h2 ^= h2 >> 29;
    h2 = h2.wrapping_mul(0x94d049bb133111eb);
    h2 ^= h2 >> 29;

    (h1, h2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bloom_basic() {
        let config = BloomConfig {
            false_positive_rate: 0.01,
        };

        let mut bf = BloomFilter::new(1000, &config);

        // Insert some keys
        for i in 0..1000 {
            let key = format!("key{:05}", i);
            bf.insert(key.as_bytes());
        }

        // Check inserted keys
        for i in 0..1000 {
            let key = format!("key{:05}", i);
            assert!(bf.may_contain(key.as_bytes()), "Should contain {}", key);
        }

        // Check non-existent keys (some false positives expected)
        let mut false_positives = 0;
        for i in 1000..2000 {
            let key = format!("key{:05}", i);
            if bf.may_contain(key.as_bytes()) {
                false_positives += 1;
            }
        }

        // False positive rate should be roughly as configured
        let fpr = false_positives as f64 / 1000.0;
        assert!(
            fpr < 0.05,
            "False positive rate {} too high (expected ~0.01)",
            fpr
        );
    }

    #[test]
    fn bloom_empty() {
        let config = BloomConfig::default();
        let bf = BloomFilter::new(0, &config);
        assert!(!bf.may_contain(b"anything"));
    }

    #[test]
    fn bloom_serialization() {
        let config = BloomConfig::default();
        let mut bf = BloomFilter::new(100, &config);

        // Insert some keys
        for i in 0..100 {
            let key = format!("key{:03}", i);
            bf.insert(key.as_bytes());
        }

        // Serialize
        let bytes = bf.to_bytes();
        assert_eq!(bytes.len(), bf.serialized_size());

        // Deserialize
        let bf2 = BloomFilter::from_bytes(&bytes).unwrap();
        assert_eq!(bf.num_bits(), bf2.num_bits());
        assert_eq!(bf.num_hashes(), bf2.num_hashes());

        // Check all keys still match
        for i in 0..100 {
            let key = format!("key{:03}", i);
            assert!(bf2.may_contain(key.as_bytes()));
        }
    }

    #[test]
    fn hash_distribution() {
        // Verify hash function produces different values
        let mut hashes = std::collections::HashSet::new();

        for i in 0..1000 {
            let key = format!("test_key_{}", i);
            let (h1, h2) = hash_key(key.as_bytes());
            hashes.insert(h1);
            hashes.insert(h2);
        }

        // Should have good distribution
        assert!(hashes.len() > 1900, "Hash distribution too narrow");
    }

    #[test]
    fn optimal_params_sanity() {
        // Small set
        let (bits, hashes) = optimal_params(100, 0.01);
        assert!(bits > 0);
        assert!(hashes > 0 && hashes <= 30);

        // Large set
        let (bits2, hashes2) = optimal_params(1_000_000, 0.001);
        assert!(bits2 > bits);
        assert!(hashes2 > 0);

        // Very small FPR should need more bits
        let (bits3, _) = optimal_params(1000, 0.0001);
        let (bits4, _) = optimal_params(1000, 0.1);
        assert!(bits3 > bits4);
    }
}
