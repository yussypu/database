//! Deterministic workload generator for simulation testing.
//!
//! Generates reproducible sequences of database operations from a seed.
//! All randomness comes from the `SimEnv` RNG, ensuring identical workloads
//! given the same seed.
//!
//! # Operation Distribution (default)
//!
//! - 40% Put (writes)
//! - 40% Get (reads)
//! - 10% Delete
//! - 10% Internal ops (flush, compact)

use bytes::Bytes;
use runtime::Env;

/// Configuration for workload generation.
#[derive(Debug, Clone)]
pub struct WorkloadConfig {
    /// Probability of Put operation (0-100).
    pub put_weight: u32,
    /// Probability of Get operation (0-100).
    pub get_weight: u32,
    /// Probability of Delete operation (0-100).
    pub delete_weight: u32,
    /// Probability of internal operations (0-100).
    pub internal_weight: u32,

    /// Number of distinct keys in the key space.
    pub key_space_size: u64,
    /// Maximum value size in bytes.
    pub max_value_size: usize,
    /// Minimum value size in bytes.
    pub min_value_size: usize,

    /// Key prefix for generated keys.
    pub key_prefix: String,
}

impl Default for WorkloadConfig {
    fn default() -> Self {
        Self {
            put_weight: 40,
            get_weight: 40,
            delete_weight: 10,
            internal_weight: 10,
            key_space_size: 100,
            max_value_size: 1024,
            min_value_size: 10,
            key_prefix: "k".to_string(),
        }
    }
}

impl WorkloadConfig {
    /// Creates a write-heavy workload (80% writes, 10% reads, 10% deletes).
    pub fn write_heavy() -> Self {
        Self {
            put_weight: 80,
            get_weight: 10,
            delete_weight: 10,
            internal_weight: 0,
            ..Default::default()
        }
    }

    /// Creates a read-heavy workload (20% writes, 70% reads, 10% deletes).
    pub fn read_heavy() -> Self {
        Self {
            put_weight: 20,
            get_weight: 70,
            delete_weight: 10,
            internal_weight: 0,
            ..Default::default()
        }
    }

    /// Creates a mixed workload with frequent internal ops.
    pub fn stress() -> Self {
        Self {
            put_weight: 35,
            get_weight: 35,
            delete_weight: 15,
            internal_weight: 15,
            ..Default::default()
        }
    }

    fn total_weight(&self) -> u32 {
        self.put_weight + self.get_weight + self.delete_weight + self.internal_weight
    }
}

/// A generated database operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Operation {
    /// Put a key-value pair.
    Put { key: Bytes, value: Bytes },
    /// Get a key.
    Get { key: Bytes },
    /// Delete a key.
    Delete { key: Bytes },
    /// Flush memtable to SSTable.
    Flush,
    /// Trigger compaction.
    Compact,
    /// Simulate a crash.
    Crash,
}

impl Operation {
    /// Returns true if this operation modifies the database.
    pub fn is_write(&self) -> bool {
        matches!(self, Operation::Put { .. } | Operation::Delete { .. })
    }

    /// Returns the key involved, if any.
    pub fn key(&self) -> Option<&Bytes> {
        match self {
            Operation::Put { key, .. } => Some(key),
            Operation::Get { key } => Some(key),
            Operation::Delete { key } => Some(key),
            _ => None,
        }
    }

    /// Returns a short description of the operation.
    pub fn describe(&self) -> String {
        match self {
            Operation::Put { key, value } => {
                format!(
                    "PUT {} = {} bytes",
                    String::from_utf8_lossy(key),
                    value.len()
                )
            }
            Operation::Get { key } => {
                format!("GET {}", String::from_utf8_lossy(key))
            }
            Operation::Delete { key } => {
                format!("DEL {}", String::from_utf8_lossy(key))
            }
            Operation::Flush => "FLUSH".to_string(),
            Operation::Compact => "COMPACT".to_string(),
            Operation::Crash => "CRASH".to_string(),
        }
    }
}

/// Generates deterministic workloads from environment RNG.
pub struct WorkloadGenerator<E: Env> {
    env: E,
    config: WorkloadConfig,
}

impl<E: Env> WorkloadGenerator<E> {
    /// Creates a new workload generator.
    pub fn new(env: E, config: WorkloadConfig) -> Self {
        Self { env, config }
    }

    /// Generates a single random operation.
    pub fn next_op(&self) -> Operation {
        let total = self.config.total_weight() as u64;
        let roll = self.env.rand_range(total) as u32;

        let put_end = self.config.put_weight;
        let get_end = put_end + self.config.get_weight;
        let delete_end = get_end + self.config.delete_weight;

        if roll < put_end {
            let key = self.random_key();
            let value = self.random_value();
            Operation::Put { key, value }
        } else if roll < get_end {
            let key = self.random_key();
            Operation::Get { key }
        } else if roll < delete_end {
            let key = self.random_key();
            Operation::Delete { key }
        } else {
            // Internal operation
            if self.env.rand_u64() % 2 == 0 {
                Operation::Flush
            } else {
                Operation::Compact
            }
        }
    }

    /// Generates a batch of random operations.
    pub fn generate_batch(&self, count: usize) -> Vec<Operation> {
        (0..count).map(|_| self.next_op()).collect()
    }

    /// Generates a random key from the key space.
    pub fn random_key(&self) -> Bytes {
        let key_num = self.env.rand_range(self.config.key_space_size);
        let key = format!("{}{:05}", self.config.key_prefix, key_num);
        Bytes::from(key)
    }

    /// Generates a specific key by index.
    pub fn key_at(&self, index: u64) -> Bytes {
        let key = format!(
            "{}{:05}",
            self.config.key_prefix,
            index % self.config.key_space_size
        );
        Bytes::from(key)
    }

    /// Generates a random value.
    pub fn random_value(&self) -> Bytes {
        let size_range = self.config.max_value_size - self.config.min_value_size;
        let size =
            self.config.min_value_size + (self.env.rand_range(size_range as u64 + 1) as usize);

        let mut value = vec![0u8; size];
        self.env.fill_random(&mut value);
        Bytes::from(value)
    }

    /// Generates a value with specific content for testing.
    pub fn value_for_key(&self, key: &[u8], cycle: u64) -> Bytes {
        // Create a deterministic but unique value based on key and cycle
        let content = format!(
            "v_{}_{}_{}",
            String::from_utf8_lossy(key),
            cycle,
            self.env.rand_u64() % 10000
        );
        Bytes::from(content)
    }
}

/// A recorded operation with result for history tracking.
#[derive(Debug, Clone)]
pub struct RecordedOp {
    /// The operation that was performed.
    pub op: Operation,
    /// The sequence number when performed.
    pub seq: u64,
    /// Whether the operation succeeded.
    pub success: bool,
    /// The result for Get operations.
    pub get_result: Option<Option<Bytes>>,
}

/// Tracks a sequence of operations for replay.
#[derive(Debug, Clone, Default)]
pub struct OperationLog {
    /// All recorded operations.
    ops: Vec<RecordedOp>,
    /// Current sequence number.
    seq: u64,
}

impl OperationLog {
    /// Creates a new empty operation log.
    pub fn new() -> Self {
        Self {
            ops: Vec::new(),
            seq: 0,
        }
    }

    /// Records a successful write operation.
    pub fn record_write(&mut self, op: Operation) {
        self.ops.push(RecordedOp {
            op,
            seq: self.seq,
            success: true,
            get_result: None,
        });
        self.seq += 1;
    }

    /// Records a get operation with its result.
    pub fn record_get(&mut self, key: Bytes, result: Option<Bytes>) {
        self.ops.push(RecordedOp {
            op: Operation::Get { key },
            seq: self.seq,
            success: true,
            get_result: Some(result),
        });
        self.seq += 1;
    }

    /// Records a failed operation.
    pub fn record_failure(&mut self, op: Operation) {
        self.ops.push(RecordedOp {
            op,
            seq: self.seq,
            success: false,
            get_result: None,
        });
        self.seq += 1;
    }

    /// Returns all recorded operations.
    pub fn ops(&self) -> &[RecordedOp] {
        &self.ops
    }

    /// Returns the number of operations.
    pub fn len(&self) -> usize {
        self.ops.len()
    }

    /// Returns true if no operations have been recorded.
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    /// Returns write operations only.
    pub fn writes(&self) -> impl Iterator<Item = &RecordedOp> {
        self.ops.iter().filter(|op| op.op.is_write() && op.success)
    }

    /// Clears the log.
    pub fn clear(&mut self) {
        self.ops.clear();
        self.seq = 0;
    }
}

/// A batch of operations to execute atomically (for future transaction support).
#[derive(Debug, Clone, Default)]
pub struct OperationBatch {
    ops: Vec<Operation>,
}

impl OperationBatch {
    /// Creates a new empty batch.
    pub fn new() -> Self {
        Self { ops: Vec::new() }
    }

    /// Adds a put operation.
    pub fn put(&mut self, key: Bytes, value: Bytes) {
        self.ops.push(Operation::Put { key, value });
    }

    /// Adds a delete operation.
    pub fn delete(&mut self, key: Bytes) {
        self.ops.push(Operation::Delete { key });
    }

    /// Returns the operations in this batch.
    pub fn ops(&self) -> &[Operation] {
        &self.ops
    }

    /// Returns the number of operations.
    pub fn len(&self) -> usize {
        self.ops.len()
    }

    /// Returns true if the batch is empty.
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use runtime::{SimEnv, SimEnvConfig};

    fn test_env(seed: u64) -> SimEnv {
        SimEnv::new(SimEnvConfig::with_seed(seed))
    }

    #[test]
    fn workload_deterministic() {
        let env1 = test_env(42);
        let env2 = test_env(42);

        let gen1 = WorkloadGenerator::new(env1, WorkloadConfig::default());
        let gen2 = WorkloadGenerator::new(env2, WorkloadConfig::default());

        let ops1: Vec<_> = (0..100).map(|_| gen1.next_op()).collect();
        let ops2: Vec<_> = (0..100).map(|_| gen2.next_op()).collect();

        assert_eq!(ops1, ops2, "Same seed must produce identical workloads");
    }

    #[test]
    fn workload_different_seeds() {
        let env1 = test_env(42);
        let env2 = test_env(43);

        let gen1 = WorkloadGenerator::new(env1, WorkloadConfig::default());
        let gen2 = WorkloadGenerator::new(env2, WorkloadConfig::default());

        let ops1: Vec<_> = (0..100).map(|_| gen1.next_op()).collect();
        let ops2: Vec<_> = (0..100).map(|_| gen2.next_op()).collect();

        assert_ne!(
            ops1, ops2,
            "Different seeds should produce different workloads"
        );
    }

    #[test]
    fn workload_distribution() {
        let env = test_env(42);
        let gen = WorkloadGenerator::new(env, WorkloadConfig::default());

        let ops: Vec<_> = (0..1000).map(|_| gen.next_op()).collect();

        let puts = ops
            .iter()
            .filter(|op| matches!(op, Operation::Put { .. }))
            .count();
        let gets = ops
            .iter()
            .filter(|op| matches!(op, Operation::Get { .. }))
            .count();
        let deletes = ops
            .iter()
            .filter(|op| matches!(op, Operation::Delete { .. }))
            .count();

        // With 40/40/10/10 distribution, we expect roughly:
        // 400 puts, 400 gets, 100 deletes, 100 internal
        // Allow 15% variance
        assert!(puts > 300 && puts < 500, "Expected ~400 puts, got {}", puts);
        assert!(gets > 300 && gets < 500, "Expected ~400 gets, got {}", gets);
        assert!(
            deletes > 50 && deletes < 150,
            "Expected ~100 deletes, got {}",
            deletes
        );
    }

    #[test]
    fn key_generation() {
        let env = test_env(42);
        let config = WorkloadConfig {
            key_space_size: 10,
            key_prefix: "test_".to_string(),
            ..Default::default()
        };
        let gen = WorkloadGenerator::new(env, config);

        // All generated keys should be within the key space
        for _ in 0..100 {
            let key = gen.random_key();
            let key_str = String::from_utf8_lossy(&key);
            assert!(key_str.starts_with("test_"));
            let num: u64 = key_str[5..].parse().unwrap();
            assert!(num < 10);
        }
    }

    #[test]
    fn value_size_bounds() {
        let env = test_env(42);
        let config = WorkloadConfig {
            min_value_size: 50,
            max_value_size: 100,
            ..Default::default()
        };
        let gen = WorkloadGenerator::new(env, config);

        for _ in 0..100 {
            let value = gen.random_value();
            assert!(value.len() >= 50);
            assert!(value.len() <= 100);
        }
    }

    #[test]
    fn operation_log() {
        let mut log = OperationLog::new();

        log.record_write(Operation::Put {
            key: Bytes::from("key1"),
            value: Bytes::from("value1"),
        });
        log.record_get(Bytes::from("key1"), Some(Bytes::from("value1")));
        log.record_write(Operation::Delete {
            key: Bytes::from("key1"),
        });

        assert_eq!(log.len(), 3);
        assert_eq!(log.writes().count(), 2);
    }
}
