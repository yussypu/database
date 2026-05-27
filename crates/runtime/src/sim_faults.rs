//! Deterministic fault injection infrastructure for simulation testing.
//!
//! This module provides [`FaultConfig`] for configuring probabilistic faults
//! and [`FaultEvent`] for recording when faults fire during simulation.
//!
//! # Fault Types
//!
//! - **PartialWrite**: Write only K < N bytes, simulating torn writes
//! - **DiskFull**: Return ENOSPC when total bytes exceed threshold
//! - **SlowWrite**: Advance simulated time before returning from write
//! - **ClockSkew**: Return skewed time readings from now()
//! - **ProcessPause**: Extend sleep duration beyond requested
//!
//! Note: Crash remains explicit via `SimEnv::simulate_crash()` and is not
//! probabilistically triggered by FaultConfig.
//!
//! # Determinism
//!
//! All fault decisions use `Env::rand_f64()` from the deterministic RNG.
//! Same seed + same FaultConfig = same fault sequence.

use crate::{Duration, PathBuf};

/// Configuration for probabilistic fault injection.
///
/// All probabilities are in [0.0, 1.0]. A probability of 0.0 disables the fault.
/// All fields default to disabled (probabilities = 0.0, thresholds = 0).
#[derive(Debug, Clone)]
pub struct FaultConfig {
    // ========================================
    // Partial Write Fault
    // ========================================
    /// Probability (0.0-1.0) that a write only partially completes.
    /// The fault truncates the last N bytes of a write where N is seed-derived.
    pub partial_write_prob: f64,

    // ========================================
    // Disk Full Fault
    // ========================================
    /// Threshold (in bytes) after which all writes return ENOSPC.
    /// 0 means disabled (never trigger).
    pub disk_full_threshold: u64,

    // ========================================
    // Slow Write Fault
    // ========================================
    /// Probability (0.0-1.0) that a write returns slowly.
    /// "Slowly" means advancing simulated time by slow_write_duration before returning.
    pub slow_write_prob: f64,
    /// Duration to delay slow writes.
    pub slow_write_duration: Duration,

    // ========================================
    // Clock Skew Fault
    // ========================================
    /// Probability (0.0-1.0) that env.now() returns a time-skewed value.
    /// Skew amount is seed-derived, can be forward OR backward, bounded by clock_skew_max.
    pub clock_skew_prob: f64,
    /// Maximum magnitude of clock skew (applies to both forward and backward).
    pub clock_skew_max: Duration,

    // ========================================
    // Process Pause Fault
    // ========================================
    /// Probability (0.0-1.0) that env.sleep() takes much longer than requested.
    /// Simulates stop-the-world pauses.
    pub process_pause_prob: f64,
    /// Additional duration added to sleep when process pause occurs.
    pub process_pause_duration: Duration,
}

impl Default for FaultConfig {
    fn default() -> Self {
        Self {
            partial_write_prob: 0.0,
            disk_full_threshold: 0,
            slow_write_prob: 0.0,
            slow_write_duration: Duration::from_millis(0),
            clock_skew_prob: 0.0,
            clock_skew_max: Duration::from_millis(0),
            process_pause_prob: 0.0,
            process_pause_duration: Duration::from_millis(0),
        }
    }
}

impl FaultConfig {
    /// Returns true if all faults are disabled.
    pub fn all_disabled(&self) -> bool {
        self.partial_write_prob == 0.0
            && self.disk_full_threshold == 0
            && self.slow_write_prob == 0.0
            && self.clock_skew_prob == 0.0
            && self.process_pause_prob == 0.0
    }

    /// Creates a config with partial write faults enabled.
    pub fn with_partial_writes(prob: f64) -> Self {
        Self {
            partial_write_prob: prob,
            ..Default::default()
        }
    }

    /// Creates a config with disk full fault enabled.
    pub fn with_disk_full(threshold_bytes: u64) -> Self {
        Self {
            disk_full_threshold: threshold_bytes,
            ..Default::default()
        }
    }

    /// Creates a config with slow write faults enabled.
    pub fn with_slow_writes(prob: f64, duration: Duration) -> Self {
        Self {
            slow_write_prob: prob,
            slow_write_duration: duration,
            ..Default::default()
        }
    }

    /// Creates a config with clock skew faults enabled.
    pub fn with_clock_skew(prob: f64, max_skew: Duration) -> Self {
        Self {
            clock_skew_prob: prob,
            clock_skew_max: max_skew,
            ..Default::default()
        }
    }

    /// Creates a config with process pause faults enabled.
    pub fn with_process_pause(prob: f64, pause_duration: Duration) -> Self {
        Self {
            process_pause_prob: prob,
            process_pause_duration: pause_duration,
            ..Default::default()
        }
    }
}

/// A recorded fault event for replay/inspection.
///
/// Each time a fault fires during simulation, a FaultEvent is logged.
/// The event log is deterministic given the same seed and FaultConfig.
#[derive(Debug, Clone, PartialEq)]
pub enum FaultEvent {
    /// A write was partially completed (torn write simulation).
    PartialWrite {
        /// Path of the file being written.
        path: PathBuf,
        /// Number of bytes the caller intended to write.
        intended_bytes: usize,
        /// Number of bytes actually written.
        actual_bytes: usize,
        /// Simulated time when the fault occurred.
        at_sim_time: Duration,
    },

    /// A write failed with ENOSPC.
    DiskFull {
        /// Path of the file being written.
        path: PathBuf,
        /// Number of bytes attempted.
        attempted_bytes: usize,
        /// Simulated time when the fault occurred.
        at_sim_time: Duration,
    },

    /// A write was delayed.
    SlowWrite {
        /// Path of the file being written.
        path: PathBuf,
        /// Number of bytes written.
        bytes: usize,
        /// Duration of the delay.
        delay: Duration,
        /// Simulated time when the fault occurred.
        at_sim_time: Duration,
    },

    /// A clock reading was skewed.
    ClockSkew {
        /// Amount of skew applied (positive = forward, negative encoded as is_forward=false).
        skew_magnitude: Duration,
        /// True if skew was forward in time, false if backward.
        is_forward: bool,
        /// Simulated time when the fault occurred (the true time, not the skewed one).
        at_sim_time: Duration,
    },

    /// A sleep was extended beyond the requested duration.
    ProcessPause {
        /// Duration the caller requested.
        requested: Duration,
        /// Actual duration slept (requested + pause).
        actual: Duration,
        /// Simulated time when the fault occurred.
        at_sim_time: Duration,
    },
}

impl FaultEvent {
    /// Returns a string identifier for the fault type.
    pub fn fault_type(&self) -> &'static str {
        match self {
            FaultEvent::PartialWrite { .. } => "partial_write",
            FaultEvent::DiskFull { .. } => "disk_full",
            FaultEvent::SlowWrite { .. } => "slow_write",
            FaultEvent::ClockSkew { .. } => "clock_skew",
            FaultEvent::ProcessPause { .. } => "process_pause",
        }
    }

    /// Returns the simulated time when the fault occurred.
    pub fn at_sim_time(&self) -> Duration {
        match self {
            FaultEvent::PartialWrite { at_sim_time, .. } => *at_sim_time,
            FaultEvent::DiskFull { at_sim_time, .. } => *at_sim_time,
            FaultEvent::SlowWrite { at_sim_time, .. } => *at_sim_time,
            FaultEvent::ClockSkew { at_sim_time, .. } => *at_sim_time,
            FaultEvent::ProcessPause { at_sim_time, .. } => *at_sim_time,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fault_config_defaults_all_disabled() {
        let config = FaultConfig::default();
        assert!(config.all_disabled());
        assert_eq!(config.partial_write_prob, 0.0);
        assert_eq!(config.disk_full_threshold, 0);
        assert_eq!(config.slow_write_prob, 0.0);
        assert_eq!(config.clock_skew_prob, 0.0);
        assert_eq!(config.process_pause_prob, 0.0);
    }

    #[test]
    fn fault_config_with_partial_writes_not_all_disabled() {
        let config = FaultConfig::with_partial_writes(0.5);
        assert!(!config.all_disabled());
    }

    #[test]
    fn fault_config_with_disk_full_not_all_disabled() {
        let config = FaultConfig::with_disk_full(1024);
        assert!(!config.all_disabled());
    }

    #[test]
    fn fault_event_type_names() {
        let event = FaultEvent::PartialWrite {
            path: PathBuf::from("/test"),
            intended_bytes: 100,
            actual_bytes: 50,
            at_sim_time: Duration::from_secs(1),
        };
        assert_eq!(event.fault_type(), "partial_write");

        let event = FaultEvent::DiskFull {
            path: PathBuf::from("/test"),
            attempted_bytes: 100,
            at_sim_time: Duration::from_secs(1),
        };
        assert_eq!(event.fault_type(), "disk_full");
    }
}
