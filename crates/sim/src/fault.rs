//! Deterministic fault injection for simulation testing.
//!
//! All faults are deterministic based on the simulation seed. The fault
//! injector hooks into the simulation environment to inject failures at
//! controlled points.
//!
//! # Supported Faults
//!
//! - **Crash**: Drop in-memory state, restart from disk
//! - **Partial write**: Truncate last N bytes of a write
//! - **Disk full**: Writes return ENOSPC after threshold
//! - **Slow disk**: Writes take 100× normal latency
//! - **Bit flip**: Single bit corruption in data
//!
//! # Usage
//!
//! The fault injector is configured with probabilities for each fault type.
//! During simulation, it deterministically decides whether to inject faults
//! based on the RNG state.

use runtime::{Duration, Env, SimEnv};

/// Configuration for fault injection.
#[derive(Debug, Clone)]
pub struct FaultConfig {
    /// Probability of crash after an operation (0.0 - 1.0).
    pub crash_probability: f64,
    /// Probability of partial write (0.0 - 1.0).
    pub partial_write_probability: f64,
    /// Enable disk full simulation after N bytes written.
    pub disk_full_after_bytes: Option<u64>,
    /// Slow disk factor (1.0 = normal, 100.0 = 100x slower).
    pub slow_disk_factor: f64,
    /// Probability of bit flip corruption (0.0 - 1.0).
    pub bit_flip_probability: f64,
    /// Minimum operations between crashes.
    pub min_ops_between_crashes: u64,
    /// Maximum crash depth (nested crash recovery cycles).
    pub max_crash_depth: u32,
}

impl Default for FaultConfig {
    fn default() -> Self {
        Self {
            crash_probability: 0.0,
            partial_write_probability: 0.0,
            disk_full_after_bytes: None,
            slow_disk_factor: 1.0,
            bit_flip_probability: 0.0,
            min_ops_between_crashes: 10,
            max_crash_depth: 5,
        }
    }
}

impl FaultConfig {
    /// Creates a config for stress testing with moderate fault rates.
    pub fn stress() -> Self {
        Self {
            crash_probability: 0.05, // 5% chance per operation
            partial_write_probability: 0.01,
            disk_full_after_bytes: None,
            slow_disk_factor: 1.0,
            bit_flip_probability: 0.0,
            min_ops_between_crashes: 5,
            max_crash_depth: 3,
        }
    }

    /// Creates a config for aggressive crash testing.
    pub fn crash_heavy() -> Self {
        Self {
            crash_probability: 0.15, // 15% chance per operation
            partial_write_probability: 0.02,
            disk_full_after_bytes: None,
            slow_disk_factor: 1.0,
            bit_flip_probability: 0.0,
            min_ops_between_crashes: 3,
            max_crash_depth: 5,
        }
    }

    /// Creates a config with no faults (for baseline testing).
    pub fn none() -> Self {
        Self::default()
    }

    /// Builder: sets crash probability.
    pub fn with_crash_probability(mut self, p: f64) -> Self {
        self.crash_probability = p.clamp(0.0, 1.0);
        self
    }

    /// Builder: sets partial write probability.
    pub fn with_partial_write_probability(mut self, p: f64) -> Self {
        self.partial_write_probability = p.clamp(0.0, 1.0);
        self
    }

    /// Builder: sets disk full threshold.
    pub fn with_disk_full_after(mut self, bytes: u64) -> Self {
        self.disk_full_after_bytes = Some(bytes);
        self
    }

    /// Builder: sets slow disk factor.
    pub fn with_slow_disk(mut self, factor: f64) -> Self {
        self.slow_disk_factor = factor.max(1.0);
        self
    }

    /// Builder: sets bit flip probability.
    pub fn with_bit_flip_probability(mut self, p: f64) -> Self {
        self.bit_flip_probability = p.clamp(0.0, 1.0);
        self
    }
}

/// A fault that was injected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fault {
    /// System crash - all unsynced data lost.
    Crash,
    /// Partial write - last N bytes truncated.
    PartialWrite { truncated_bytes: usize },
    /// Disk full - write failed.
    DiskFull,
    /// Bit flip - data corrupted.
    BitFlip { byte_offset: usize, bit: u8 },
}

impl Fault {
    /// Returns a description of the fault.
    pub fn describe(&self) -> String {
        match self {
            Fault::Crash => "CRASH".to_string(),
            Fault::PartialWrite { truncated_bytes } => {
                format!("PARTIAL_WRITE(truncated {} bytes)", truncated_bytes)
            }
            Fault::DiskFull => "DISK_FULL".to_string(),
            Fault::BitFlip { byte_offset, bit } => {
                format!("BIT_FLIP(offset={}, bit={})", byte_offset, bit)
            }
        }
    }
}

/// Statistics about injected faults.
#[derive(Debug, Clone, Default)]
pub struct FaultStats {
    /// Total crashes injected.
    pub crashes: u64,
    /// Total partial writes.
    pub partial_writes: u64,
    /// Total disk full errors.
    pub disk_full_errors: u64,
    /// Total bit flips.
    pub bit_flips: u64,
    /// Total operations checked for faults.
    pub operations_checked: u64,
}

impl FaultStats {
    /// Returns the total number of faults injected.
    pub fn total_faults(&self) -> u64 {
        self.crashes + self.partial_writes + self.disk_full_errors + self.bit_flips
    }
}

/// Deterministic fault injector.
///
/// Uses the simulation environment's RNG to decide when to inject faults.
pub struct FaultInjector {
    config: FaultConfig,
    stats: FaultStats,
    ops_since_last_crash: u64,
    total_bytes_written: u64,
    current_crash_depth: u32,
    /// History of injected faults for debugging.
    fault_history: Vec<(u64, Fault)>,
}

impl FaultInjector {
    /// Creates a new fault injector with the given configuration.
    pub fn new(config: FaultConfig) -> Self {
        Self {
            config,
            stats: FaultStats::default(),
            ops_since_last_crash: 0,
            total_bytes_written: 0,
            current_crash_depth: 0,
            fault_history: Vec::new(),
        }
    }

    /// Returns the current configuration.
    pub fn config(&self) -> &FaultConfig {
        &self.config
    }

    /// Returns current statistics.
    pub fn stats(&self) -> &FaultStats {
        &self.stats
    }

    /// Returns the fault history.
    pub fn fault_history(&self) -> &[(u64, Fault)] {
        &self.fault_history
    }

    /// Checks if a crash should be injected after an operation.
    ///
    /// Returns Some(Fault::Crash) if a crash should happen, None otherwise.
    pub fn maybe_crash(&mut self, env: &SimEnv) -> Option<Fault> {
        self.stats.operations_checked += 1;
        self.ops_since_last_crash += 1;

        // Don't crash if we're already in a nested recovery
        if self.current_crash_depth >= self.config.max_crash_depth {
            return None;
        }

        // Don't crash if we haven't done enough ops since last crash
        // min_ops_between_crashes means at least that many ops must complete without crash
        if self.ops_since_last_crash <= self.config.min_ops_between_crashes {
            return None;
        }

        // Check probability
        if self.config.crash_probability > 0.0 {
            let roll = env.rand_f64();
            if roll < self.config.crash_probability {
                self.stats.crashes += 1;
                self.ops_since_last_crash = 0;
                let fault = Fault::Crash;
                self.fault_history
                    .push((self.stats.operations_checked, fault.clone()));
                return Some(fault);
            }
        }

        None
    }

    /// Checks if a partial write should occur.
    ///
    /// Returns the number of bytes to truncate, or 0 for no truncation.
    pub fn maybe_partial_write(&mut self, env: &SimEnv, write_size: usize) -> usize {
        if self.config.partial_write_probability > 0.0 && write_size > 0 {
            let roll = env.rand_f64();
            if roll < self.config.partial_write_probability {
                // Truncate between 1 byte and the full write
                let truncate = 1 + (env.rand_range(write_size as u64) as usize);
                self.stats.partial_writes += 1;
                let fault = Fault::PartialWrite {
                    truncated_bytes: truncate,
                };
                self.fault_history
                    .push((self.stats.operations_checked, fault));
                return truncate;
            }
        }
        0
    }

    /// Checks if disk full should be simulated.
    ///
    /// Call this before each write with the write size.
    pub fn check_disk_full(&mut self, write_size: u64) -> bool {
        if let Some(threshold) = self.config.disk_full_after_bytes {
            if self.total_bytes_written + write_size > threshold {
                self.stats.disk_full_errors += 1;
                self.fault_history
                    .push((self.stats.operations_checked, Fault::DiskFull));
                return true;
            }
        }
        false
    }

    /// Records bytes written (for disk full tracking).
    pub fn record_write(&mut self, bytes: u64) {
        self.total_bytes_written += bytes;
    }

    /// Applies a random bit flip to data.
    ///
    /// Returns the bit flip fault if one was applied.
    pub fn maybe_bit_flip(&mut self, env: &SimEnv, data: &mut [u8]) -> Option<Fault> {
        if self.config.bit_flip_probability > 0.0 && !data.is_empty() {
            let roll = env.rand_f64();
            if roll < self.config.bit_flip_probability {
                let byte_offset = env.rand_range(data.len() as u64) as usize;
                let bit = env.rand_range(8) as u8;
                data[byte_offset] ^= 1 << bit;

                self.stats.bit_flips += 1;
                let fault = Fault::BitFlip { byte_offset, bit };
                self.fault_history
                    .push((self.stats.operations_checked, fault.clone()));
                return Some(fault);
            }
        }
        None
    }

    /// Simulates slow disk by sleeping.
    pub fn maybe_slow_disk(&self, env: &SimEnv, base_duration: Duration) {
        if self.config.slow_disk_factor > 1.0 {
            let slow_nanos =
                (base_duration.as_nanos() as f64 * (self.config.slow_disk_factor - 1.0)) as u64;
            env.sleep(Duration::from_nanos(slow_nanos));
        }
    }

    /// Called when entering crash recovery.
    pub fn enter_recovery(&mut self) {
        self.current_crash_depth += 1;
    }

    /// Called when exiting crash recovery.
    pub fn exit_recovery(&mut self) {
        if self.current_crash_depth > 0 {
            self.current_crash_depth -= 1;
        }
    }

    /// Resets the disk full counter.
    pub fn reset_disk_full(&mut self) {
        self.total_bytes_written = 0;
    }

    /// Clears fault history.
    pub fn clear_history(&mut self) {
        self.fault_history.clear();
    }
}

/// A schedule of predetermined faults for replay.
///
/// This allows replaying a specific sequence of faults for debugging.
#[derive(Debug, Clone, Default)]
pub struct FaultSchedule {
    /// Faults to inject at specific operation numbers.
    faults: Vec<(u64, Fault)>,
    /// Current position in the schedule.
    position: usize,
}

impl FaultSchedule {
    /// Creates a new empty schedule.
    pub fn new() -> Self {
        Self {
            faults: Vec::new(),
            position: 0,
        }
    }

    /// Creates a schedule from a list of (operation_number, fault) pairs.
    pub fn from_faults(faults: Vec<(u64, Fault)>) -> Self {
        Self {
            faults,
            position: 0,
        }
    }

    /// Adds a fault to the schedule.
    pub fn add(&mut self, op_num: u64, fault: Fault) {
        self.faults.push((op_num, fault));
        self.faults.sort_by_key(|(n, _)| *n);
    }

    /// Checks if a fault should be injected at the given operation number.
    pub fn check(&mut self, op_num: u64) -> Option<&Fault> {
        while self.position < self.faults.len() {
            let (target_op, fault) = &self.faults[self.position];
            match (*target_op).cmp(&op_num) {
                std::cmp::Ordering::Less => {
                    self.position += 1;
                }
                std::cmp::Ordering::Equal => {
                    self.position += 1;
                    return Some(fault);
                }
                std::cmp::Ordering::Greater => {
                    break;
                }
            }
        }
        None
    }

    /// Resets the schedule to the beginning.
    pub fn reset(&mut self) {
        self.position = 0;
    }

    /// Returns all faults in the schedule.
    pub fn faults(&self) -> &[(u64, Fault)] {
        &self.faults
    }
}

/// Combines probabilistic and scheduled fault injection.
pub struct CombinedFaultInjector {
    /// Random fault injector.
    pub random: FaultInjector,
    /// Scheduled faults.
    pub schedule: FaultSchedule,
    /// Current operation number.
    op_num: u64,
}

impl CombinedFaultInjector {
    /// Creates a combined injector.
    pub fn new(config: FaultConfig, schedule: FaultSchedule) -> Self {
        Self {
            random: FaultInjector::new(config),
            schedule,
            op_num: 0,
        }
    }

    /// Checks for any fault (scheduled takes priority).
    pub fn check_fault(&mut self, env: &SimEnv) -> Option<Fault> {
        self.op_num += 1;

        // Check scheduled faults first
        if let Some(fault) = self.schedule.check(self.op_num) {
            return Some(fault.clone());
        }

        // Then check random faults
        self.random.maybe_crash(env)
    }

    /// Returns the current operation number.
    pub fn op_num(&self) -> u64 {
        self.op_num
    }

    /// Resets for a new run.
    pub fn reset(&mut self) {
        self.op_num = 0;
        self.schedule.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use runtime::SimEnvConfig;

    fn test_env(seed: u64) -> SimEnv {
        SimEnv::new(SimEnvConfig::with_seed(seed))
    }

    #[test]
    fn fault_injector_deterministic() {
        let config = FaultConfig {
            crash_probability: 0.5,
            min_ops_between_crashes: 0,
            ..Default::default()
        };

        let env1 = test_env(42);
        let env2 = test_env(42);

        let mut inj1 = FaultInjector::new(config.clone());
        let mut inj2 = FaultInjector::new(config);

        let crashes1: Vec<_> = (0..100)
            .map(|_| inj1.maybe_crash(&env1).is_some())
            .collect();
        let crashes2: Vec<_> = (0..100)
            .map(|_| inj2.maybe_crash(&env2).is_some())
            .collect();

        assert_eq!(
            crashes1, crashes2,
            "Same seed must produce identical faults"
        );
    }

    #[test]
    fn fault_schedule() {
        let mut schedule = FaultSchedule::new();
        schedule.add(5, Fault::Crash);
        schedule.add(10, Fault::DiskFull);

        assert!(schedule.check(1).is_none());
        assert!(schedule.check(4).is_none());
        assert_eq!(schedule.check(5), Some(&Fault::Crash));
        assert!(schedule.check(6).is_none());
        assert!(schedule.check(9).is_none());
        assert_eq!(schedule.check(10), Some(&Fault::DiskFull));
        assert!(schedule.check(11).is_none());
    }

    #[test]
    fn min_ops_between_crashes() {
        let config = FaultConfig {
            crash_probability: 1.0, // Always want to crash
            min_ops_between_crashes: 5,
            ..Default::default()
        };

        let env = test_env(42);
        let mut inj = FaultInjector::new(config);

        // First 5 ops should not crash
        for _ in 0..5 {
            assert!(inj.maybe_crash(&env).is_none());
        }

        // 6th op should crash
        assert!(inj.maybe_crash(&env).is_some());

        // Next 5 should not crash
        for _ in 0..5 {
            assert!(inj.maybe_crash(&env).is_none());
        }
    }

    #[test]
    fn bit_flip() {
        let config = FaultConfig {
            bit_flip_probability: 1.0,
            ..Default::default()
        };

        let env = test_env(42);
        let mut inj = FaultInjector::new(config);

        let mut data = vec![0u8; 100];
        let original = data.clone();

        let fault = inj.maybe_bit_flip(&env, &mut data);
        assert!(fault.is_some());
        assert_ne!(data, original, "Bit flip should modify data");

        // Exactly one bit should be different
        let diff_bits: u32 = data
            .iter()
            .zip(original.iter())
            .map(|(a, b)| (a ^ b).count_ones())
            .sum();
        assert_eq!(diff_bits, 1, "Exactly one bit should be flipped");
    }

    #[test]
    fn disk_full_tracking() {
        let config = FaultConfig {
            disk_full_after_bytes: Some(1000),
            ..Default::default()
        };

        let mut inj = FaultInjector::new(config);

        assert!(!inj.check_disk_full(500));
        inj.record_write(500);

        assert!(!inj.check_disk_full(400));
        inj.record_write(400);

        // Next write would exceed threshold
        assert!(inj.check_disk_full(200));
    }

    #[test]
    fn crash_depth_limit() {
        let config = FaultConfig {
            crash_probability: 1.0,
            min_ops_between_crashes: 0,
            max_crash_depth: 2,
            ..Default::default()
        };

        let env = test_env(42);
        let mut inj = FaultInjector::new(config);

        // First crash
        assert!(inj.maybe_crash(&env).is_some());
        inj.enter_recovery();

        // Second crash during recovery
        assert!(inj.maybe_crash(&env).is_some());
        inj.enter_recovery();

        // Third crash should be prevented
        assert!(inj.maybe_crash(&env).is_none());

        // Exit one level
        inj.exit_recovery();

        // Now crash should work again
        assert!(inj.maybe_crash(&env).is_some());
    }
}
