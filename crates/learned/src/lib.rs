//! Learned indexes for cracked-db.
//!
//! This crate provides learned data structures that replace classical
//! indexes with machine learning models:
//!
//! - **PGM-index**: Piecewise Geometric Model index for sorted data.
//!   Provides O(log(segments) + log(ε)) lookup time with space proportional
//!   to n/ε where n is the number of keys.
//!
//! - **Bloom filters**: Classical Bloom filters for membership testing.
//!   Learned bloom filters (per Mitzenmacher 2018) are deferred to Phase 2b.
//!
//! The PGM-index has a classical fallback for A/B testing and edge cases.
//!
//! # Usage
//!
//! ```rust
//! use learned::pgm::{BlockIndex, PgmConfig};
//! use learned::bloom::{BloomFilter, BloomConfig};
//!
//! // Build a PGM-index for block lookups (uses u128 for better prefix diversity)
//! let keys: Vec<u128> = (0..1000).collect();
//! let index = BlockIndex::build(&keys, &PgmConfig::default());
//!
//! // Search returns a narrow range to binary search
//! let (lo, hi) = index.search(500u128);
//! assert!(lo <= 500 && 500 < hi);
//!
//! // Build a Bloom filter for membership testing
//! let config = BloomConfig::default();
//! let mut bloom = BloomFilter::new(1000, &config);
//! for &key in &keys {
//!     bloom.insert(&(key as u64).to_be_bytes());
//! }
//! assert!(bloom.may_contain(&500u64.to_be_bytes()));
//! ```
//!
//! # References
//!
//! - Ferragina & Vinciguerra, "The PGM-index" (VLDB 2020)

pub mod bloom;
pub mod pgm;

// Re-export main types for convenience
pub use bloom::{BloomConfig, BloomFilter};
pub use pgm::{BlockIndex, PgmConfig, PgmIndex};

/// Re-export runtime for convenience.
pub use runtime;
