//! YCSB workload generators.
//!
//! Implements the six core YCSB workloads (A-F) with configurable key/value
//! sizes and Zipfian key distribution.

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

/// YCSB workload types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum YcsbWorkload {
    /// Workload A: 50% read, 50% update, Zipfian
    A,
    /// Workload B: 95% read, 5% update, Zipfian
    B,
    /// Workload C: 100% read, Zipfian
    C,
    /// Workload D: 95% read latest, 5% insert
    D,
    /// Workload E: 95% short scan, 5% insert, Zipfian
    E,
    /// Workload F: 50% read, 50% read-modify-write, Zipfian
    F,
}

impl std::fmt::Display for YcsbWorkload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            YcsbWorkload::A => write!(f, "ycsb_a"),
            YcsbWorkload::B => write!(f, "ycsb_b"),
            YcsbWorkload::C => write!(f, "ycsb_c"),
            YcsbWorkload::D => write!(f, "ycsb_d"),
            YcsbWorkload::E => write!(f, "ycsb_e"),
            YcsbWorkload::F => write!(f, "ycsb_f"),
        }
    }
}

impl std::str::FromStr for YcsbWorkload {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "a" | "ycsb_a" => Ok(YcsbWorkload::A),
            "b" | "ycsb_b" => Ok(YcsbWorkload::B),
            "c" | "ycsb_c" => Ok(YcsbWorkload::C),
            "d" | "ycsb_d" => Ok(YcsbWorkload::D),
            "e" | "ycsb_e" => Ok(YcsbWorkload::E),
            "f" | "ycsb_f" => Ok(YcsbWorkload::F),
            _ => Err(format!("unknown YCSB workload: {}", s)),
        }
    }
}

/// YCSB workload configuration.
#[derive(Debug, Clone)]
pub struct YcsbConfig {
    /// Which workload to run.
    pub workload: YcsbWorkload,
    /// Total number of records to load.
    pub record_count: u64,
    /// Number of operations to run during measurement.
    pub operation_count: u64,
    /// Key size in bytes.
    pub key_size: usize,
    /// Value size in bytes.
    pub value_size: usize,
    /// Random seed for reproducibility.
    pub seed: u64,
    /// Average scan length for workload E.
    pub scan_length: u32,
}

impl Default for YcsbConfig {
    fn default() -> Self {
        Self {
            workload: YcsbWorkload::A,
            record_count: 10000,
            operation_count: 10000,
            key_size: 16,
            value_size: 100,
            seed: 0xDEADBEEF,
            scan_length: 100,
        }
    }
}

/// An operation in a YCSB workload.
#[derive(Debug, Clone)]
pub enum Operation {
    /// Read a key.
    Read(Vec<u8>),
    /// Update a key with a new value.
    Update(Vec<u8>, Vec<u8>),
    /// Insert a new key-value pair.
    Insert(Vec<u8>, Vec<u8>),
    /// Scan starting from key for a given length.
    Scan(Vec<u8>, u32),
    /// Read-modify-write: read key, modify, write back.
    ReadModifyWrite(Vec<u8>),
}

/// YCSB workload generator.
pub struct YcsbGenerator {
    config: YcsbConfig,
    rng: StdRng,
    zipf: ScrambledZipfian,
    latest: LatestGenerator,
    next_insert_key: u64,
}

impl YcsbGenerator {
    /// Creates a new YCSB generator with the given configuration.
    pub fn new(config: YcsbConfig) -> Self {
        let rng = StdRng::seed_from_u64(config.seed);
        let zipf = ScrambledZipfian::new(config.record_count, 0.99);
        let latest = LatestGenerator::new(config.record_count);
        let next_insert_key = config.record_count;

        Self {
            config,
            rng,
            zipf,
            latest,
            next_insert_key,
        }
    }

    /// Returns an iterator over the initial load keys.
    pub fn load_keys(&self) -> impl Iterator<Item = (Vec<u8>, Vec<u8>)> + '_ {
        (0..self.config.record_count).map(move |i| {
            let key = self.format_key(i);
            let value = self.random_value_from_seed(i);
            (key, value)
        })
    }

    /// Generates the next operation.
    pub fn next_op(&mut self) -> Operation {
        match self.config.workload {
            YcsbWorkload::A => self.next_op_a(),
            YcsbWorkload::B => self.next_op_b(),
            YcsbWorkload::C => self.next_op_c(),
            YcsbWorkload::D => self.next_op_d(),
            YcsbWorkload::E => self.next_op_e(),
            YcsbWorkload::F => self.next_op_f(),
        }
    }

    // Workload A: 50% read, 50% update
    fn next_op_a(&mut self) -> Operation {
        let key_num = self.zipf.next(&mut self.rng);
        let key = self.format_key(key_num);

        if self.rng.gen_bool(0.5) {
            Operation::Read(key)
        } else {
            let value = self.random_value();
            Operation::Update(key, value)
        }
    }

    // Workload B: 95% read, 5% update
    fn next_op_b(&mut self) -> Operation {
        let key_num = self.zipf.next(&mut self.rng);
        let key = self.format_key(key_num);

        if self.rng.gen_bool(0.95) {
            Operation::Read(key)
        } else {
            let value = self.random_value();
            Operation::Update(key, value)
        }
    }

    // Workload C: 100% read
    fn next_op_c(&mut self) -> Operation {
        let key_num = self.zipf.next(&mut self.rng);
        let key = self.format_key(key_num);
        Operation::Read(key)
    }

    // Workload D: 95% read latest, 5% insert
    fn next_op_d(&mut self) -> Operation {
        if self.rng.gen_bool(0.95) {
            let key_num = self.latest.next(&mut self.rng);
            let key = self.format_key(key_num);
            Operation::Read(key)
        } else {
            let key_num = self.next_insert_key;
            self.next_insert_key += 1;
            self.latest.acknowledge_insert();
            let key = self.format_key(key_num);
            let value = self.random_value();
            Operation::Insert(key, value)
        }
    }

    // Workload E: 95% short scan, 5% insert
    fn next_op_e(&mut self) -> Operation {
        if self.rng.gen_bool(0.95) {
            let key_num = self.zipf.next(&mut self.rng);
            let key = self.format_key(key_num);
            // Scan length varies around average
            let length = self.rng.gen_range(1..=self.config.scan_length * 2);
            Operation::Scan(key, length)
        } else {
            let key_num = self.next_insert_key;
            self.next_insert_key += 1;
            let key = self.format_key(key_num);
            let value = self.random_value();
            Operation::Insert(key, value)
        }
    }

    // Workload F: 50% read, 50% read-modify-write
    fn next_op_f(&mut self) -> Operation {
        let key_num = self.zipf.next(&mut self.rng);
        let key = self.format_key(key_num);

        if self.rng.gen_bool(0.5) {
            Operation::Read(key)
        } else {
            Operation::ReadModifyWrite(key)
        }
    }

    /// Formats a key number as bytes.
    fn format_key(&self, key_num: u64) -> Vec<u8> {
        // Zero-pad the key number to ensure consistent ordering
        let s = format!("user{:0width$}", key_num, width = self.config.key_size - 4);
        let mut key = s.into_bytes();
        key.truncate(self.config.key_size);
        while key.len() < self.config.key_size {
            key.push(b'0');
        }
        key
    }

    /// Generates a random value.
    fn random_value(&mut self) -> Vec<u8> {
        let mut value = vec![0u8; self.config.value_size];
        self.rng.fill(&mut value[..]);
        value
    }

    /// Generates a deterministic value from seed (for initial load).
    fn random_value_from_seed(&self, seed: u64) -> Vec<u8> {
        let mut rng = StdRng::seed_from_u64(self.config.seed.wrapping_add(seed));
        let mut value = vec![0u8; self.config.value_size];
        rng.fill(&mut value[..]);
        value
    }

    /// Generates a new value for read-modify-write operations.
    pub fn generate_value(&mut self) -> Vec<u8> {
        self.random_value()
    }
}

/// Scrambled Zipfian distribution.
///
/// Uses the "scrambled" approach from the YCSB paper to avoid clustering
/// popular keys at low indices, which would give LSMs an unfair locality advantage.
struct ScrambledZipfian {
    num_items: u64,
    #[allow(dead_code)]
    theta: f64,
    zetan: f64,
    #[allow(dead_code)]
    zeta2theta: f64,
    alpha: f64,
    eta: f64,
}

impl ScrambledZipfian {
    fn new(num_items: u64, theta: f64) -> Self {
        let zetan = Self::zeta(num_items, theta);
        let zeta2theta = Self::zeta(2, theta);

        let alpha = 1.0 / (1.0 - theta);
        let eta = (1.0 - (2.0 / num_items as f64).powf(1.0 - theta)) / (1.0 - zeta2theta / zetan);

        Self {
            num_items,
            theta,
            zetan,
            zeta2theta,
            alpha,
            eta,
        }
    }

    fn zeta(n: u64, theta: f64) -> f64 {
        let mut sum = 0.0;
        for i in 1..=n {
            sum += 1.0 / (i as f64).powf(theta);
        }
        sum
    }

    fn next(&self, rng: &mut StdRng) -> u64 {
        let u: f64 = rng.gen();
        let uz = u * self.zetan;

        let rank = if uz < 1.0 {
            0
        } else if uz < 1.0 + 0.5_f64.powf(self.theta) {
            1
        } else {
            let spread =
                (self.num_items as f64 * (self.eta * u - self.eta + 1.0).powf(self.alpha)) as u64;
            spread.min(self.num_items - 1)
        };

        // Scramble to distribute hot keys
        self.scramble(rank)
    }

    fn scramble(&self, rank: u64) -> u64 {
        // FNV-1a hash to scramble
        const FNV_OFFSET: u64 = 0xcbf29ce484222325;
        const FNV_PRIME: u64 = 0x100000001b3;

        let mut hash = FNV_OFFSET;
        for byte in rank.to_le_bytes() {
            hash ^= byte as u64;
            hash = hash.wrapping_mul(FNV_PRIME);
        }
        hash % self.num_items
    }
}

/// Latest distribution for workload D.
///
/// Favors recently inserted keys.
struct LatestGenerator {
    max_key: u64,
}

impl LatestGenerator {
    fn new(initial_count: u64) -> Self {
        Self {
            max_key: initial_count.saturating_sub(1),
        }
    }

    fn acknowledge_insert(&mut self) {
        self.max_key += 1;
    }

    fn next(&self, rng: &mut StdRng) -> u64 {
        if self.max_key == 0 {
            return 0;
        }

        // Use exponential distribution to favor recent keys
        let u: f64 = rng.gen();
        // Scale so that 90% of accesses hit the most recent 10% of keys
        let scale = 0.1;
        let offset = (-u.ln() * scale * self.max_key as f64) as u64;

        self.max_key.saturating_sub(offset.min(self.max_key))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn ycsb_zipfian_distribution_skews_correctly() {
        let config = YcsbConfig {
            workload: YcsbWorkload::A,
            record_count: 10000,
            operation_count: 100000,
            seed: 42,
            ..Default::default()
        };

        let mut gen = YcsbGenerator::new(config);
        let mut counts: HashMap<u64, u64> = HashMap::new();

        // Generate 100k operations
        for _ in 0..100000 {
            let op = gen.next_op();
            let key = match op {
                Operation::Read(k) | Operation::Update(k, _) => k,
                _ => continue,
            };
            // Extract key number
            let key_str = String::from_utf8_lossy(&key);
            if let Some(num_str) = key_str.strip_prefix("user") {
                if let Ok(num) = num_str.trim_start_matches('0').parse::<u64>() {
                    *counts.entry(num).or_insert(0) += 1;
                } else if num_str.chars().all(|c| c == '0') {
                    *counts.entry(0).or_insert(0) += 1;
                }
            }
        }

        // Sort by frequency
        let mut freq: Vec<_> = counts.into_iter().collect();
        freq.sort_by(|a, b| b.1.cmp(&a.1));

        // Top 1% of keys (100 keys) should account for >30% of ops
        let top_1_percent = 100;
        let total_ops: u64 = freq.iter().map(|(_, c)| c).sum();
        let top_1_percent_ops: u64 = freq.iter().take(top_1_percent).map(|(_, c)| c).sum();
        let top_1_percent_ratio = top_1_percent_ops as f64 / total_ops as f64;

        assert!(
            top_1_percent_ratio > 0.30,
            "top 1% should account for >30% of ops, got {:.2}%",
            top_1_percent_ratio * 100.0
        );
    }

    #[test]
    fn ycsb_workload_a_50_50_split() {
        let config = YcsbConfig {
            workload: YcsbWorkload::A,
            record_count: 1000,
            operation_count: 10000,
            seed: 42,
            ..Default::default()
        };

        let mut gen = YcsbGenerator::new(config);
        let mut reads = 0;
        let mut updates = 0;

        for _ in 0..10000 {
            match gen.next_op() {
                Operation::Read(_) => reads += 1,
                Operation::Update(_, _) => updates += 1,
                _ => {}
            }
        }

        let total = reads + updates;
        let read_ratio = reads as f64 / total as f64;

        // Should be within 2% of 50/50
        assert!(
            (read_ratio - 0.5).abs() < 0.02,
            "workload A should be ~50/50, got {:.2}% reads",
            read_ratio * 100.0
        );
    }

    #[test]
    fn ycsb_load_keys_generates_correct_count() {
        let config = YcsbConfig {
            record_count: 100,
            key_size: 16,
            value_size: 50,
            ..Default::default()
        };

        let gen = YcsbGenerator::new(config);
        let keys: Vec<_> = gen.load_keys().collect();

        assert_eq!(keys.len(), 100);
        for (key, value) in &keys {
            assert_eq!(key.len(), 16);
            assert_eq!(value.len(), 50);
        }
    }
}
