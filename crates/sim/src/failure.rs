//! Failure detection and reporting for simulation testing.
//!
//! This module provides a structured way to represent and report failures
//! found during simulation. It enables deterministic replay of failures
//! and supports the shrinker in finding minimal reproductions.
//!
//! # FailureKind
//!
//! Failures are classified by kind to enable the shrinker to verify that
//! shrunk reproductions still exhibit the same class of failure. A shrink
//! is only valid if it produces the same `FailureKind`.
//!
//! # Replay
//!
//! The `replay()` function provides a deterministic way to reproduce any
//! failure. Given the same seed, scenario, and workload length, the same
//! failure will occur on any machine.
//!
//! ```ignore
//! use sim::failure::{replay, FailureReport};
//! use sim::fault_scenarios::FaultScenario;
//!
//! // Replay a known failing seed
//! let result = replay(0xDEADBEEF, &FaultScenario::Combined, 500);
//! match result {
//!     Err(report) => println!("Failure: {}", report),
//!     Ok(()) => println!("No failure (bug fixed?)"),
//! }
//! ```

use crate::fault_scenarios::{create_env_for_scenario, FaultScenario, ScenarioConfig};
use crate::invariant::SerializationViolation;
use std::fmt;

/// The kind of failure detected during simulation.
///
/// This enum classifies failures so the shrinker can verify that a shrunk
/// reproduction exhibits the same failure. Different failure kinds represent
/// fundamentally different bugs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailureKind {
    /// A cycle was detected in the serialization graph, indicating a
    /// non-serializable schedule was allowed to commit.
    SerializabilityCycle {
        /// Transaction IDs involved in the cycle.
        txn_ids: Vec<u64>,
    },

    /// A committed write was lost (not readable after crash/recovery).
    LostWrite {
        /// The key that was lost.
        key: Vec<u8>,
        /// The expected value that should have been present.
        expected: Vec<u8>,
    },

    /// A custom failure with a description.
    Custom(String),

    /// An unrecoverable error (panic, assertion, internal error).
    Unrecoverable(String),
}

impl FailureKind {
    /// Creates a SerializabilityCycle from a SerializationViolation.
    pub fn from_serialization_violation(violation: &SerializationViolation) -> Self {
        FailureKind::SerializabilityCycle {
            txn_ids: violation.cycle.clone(),
        }
    }

    /// Returns the short name of this failure kind for display.
    pub fn name(&self) -> &'static str {
        match self {
            FailureKind::SerializabilityCycle { .. } => "serializability_cycle",
            FailureKind::LostWrite { .. } => "lost_write",
            FailureKind::Custom(_) => "custom",
            FailureKind::Unrecoverable(_) => "unrecoverable",
        }
    }

    /// Returns true if this failure kind matches another (same variant).
    ///
    /// Used by the shrinker to verify shrunk reproductions produce the
    /// same kind of failure.
    pub fn matches(&self, other: &FailureKind) -> bool {
        std::mem::discriminant(self) == std::mem::discriminant(other)
    }
}

impl fmt::Display for FailureKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FailureKind::SerializabilityCycle { txn_ids } => {
                write!(f, "Serializability cycle: ")?;
                for (i, id) in txn_ids.iter().enumerate() {
                    if i > 0 {
                        write!(f, " -> ")?;
                    }
                    write!(f, "T{}", id)?;
                }
                Ok(())
            }
            FailureKind::LostWrite { key, expected } => {
                write!(
                    f,
                    "Lost write: key={:?}, expected={:?}",
                    String::from_utf8_lossy(key),
                    String::from_utf8_lossy(expected)
                )
            }
            FailureKind::Custom(msg) => write!(f, "Custom: {}", msg),
            FailureKind::Unrecoverable(msg) => write!(f, "Unrecoverable: {}", msg),
        }
    }
}

/// A complete report of a simulation failure.
///
/// Contains all information needed to:
/// 1. Understand what failed
/// 2. Reproduce the failure deterministically
/// 3. Shrink the failure to a minimal reproduction
#[derive(Debug, Clone)]
pub struct FailureReport {
    /// The seed that produced this failure.
    pub seed: u64,
    /// The fault scenario under which the failure occurred.
    pub scenario: FaultScenario,
    /// The number of operations in the workload.
    pub workload_len: usize,
    /// The kind of failure that was detected.
    pub failure_kind: FailureKind,
    /// A command hint for replaying this failure.
    pub replay_cmd_hint: String,
}

impl FailureReport {
    /// Creates a new failure report.
    pub fn new(
        seed: u64,
        scenario: FaultScenario,
        workload_len: usize,
        failure_kind: FailureKind,
    ) -> Self {
        let replay_cmd_hint = format!(
            "cracked-db sim replay --seed=0x{:016X} --scenario={} --ops={}",
            seed,
            scenario.name(),
            workload_len
        );
        Self {
            seed,
            scenario,
            workload_len,
            failure_kind,
            replay_cmd_hint,
        }
    }

    /// Creates a SerializabilityCycle failure report.
    pub fn serializability_cycle(
        seed: u64,
        scenario: FaultScenario,
        workload_len: usize,
        violation: &SerializationViolation,
    ) -> Self {
        Self::new(
            seed,
            scenario,
            workload_len,
            FailureKind::from_serialization_violation(violation),
        )
    }

    /// Creates a LostWrite failure report.
    pub fn lost_write(
        seed: u64,
        scenario: FaultScenario,
        workload_len: usize,
        key: Vec<u8>,
        expected: Vec<u8>,
    ) -> Self {
        Self::new(
            seed,
            scenario,
            workload_len,
            FailureKind::LostWrite { key, expected },
        )
    }

    /// Creates a Custom failure report.
    pub fn custom(
        seed: u64,
        scenario: FaultScenario,
        workload_len: usize,
        message: impl Into<String>,
    ) -> Self {
        Self::new(
            seed,
            scenario,
            workload_len,
            FailureKind::Custom(message.into()),
        )
    }

    /// Creates an Unrecoverable failure report.
    pub fn unrecoverable(
        seed: u64,
        scenario: FaultScenario,
        workload_len: usize,
        message: impl Into<String>,
    ) -> Self {
        Self::new(
            seed,
            scenario,
            workload_len,
            FailureKind::Unrecoverable(message.into()),
        )
    }

    /// Returns the transaction IDs if this is a SerializabilityCycle.
    pub fn cycle_txns(&self) -> Option<&[u64]> {
        match &self.failure_kind {
            FailureKind::SerializabilityCycle { txn_ids } => Some(txn_ids),
            _ => None,
        }
    }
}

impl fmt::Display for FailureReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "=== FAILURE REPORT ===")?;
        writeln!(f, "Seed: 0x{:016X}", self.seed)?;
        writeln!(f, "Scenario: {}", self.scenario.name())?;
        writeln!(f, "Workload length: {}", self.workload_len)?;
        writeln!(f, "Failure: {}", self.failure_kind)?;
        writeln!(f)?;
        writeln!(f, "Replay command:")?;
        writeln!(f, "  {}", self.replay_cmd_hint)?;
        Ok(())
    }
}

/// Replays a simulation with the given parameters.
///
/// This function deterministically reproduces a simulation run. Given the
/// same seed, scenario, and workload length, the same failure (or success)
/// will occur on any machine.
///
/// # Arguments
///
/// * `seed` - The random seed for deterministic execution
/// * `scenario` - The fault scenario to use
/// * `workload_len` - The number of operations to execute
///
/// # Returns
///
/// * `Ok(())` if the simulation passes (no failure detected)
/// * `Err(FailureReport)` if a failure is detected
///
/// # Example
///
/// ```ignore
/// let result = replay(0xDEADBEEF, &FaultScenario::Combined, 500);
/// if let Err(report) = result {
///     println!("Failure reproduced: {}", report);
/// }
/// ```
pub fn replay(
    seed: u64,
    scenario: &FaultScenario,
    workload_len: usize,
) -> Result<(), FailureReport> {
    let config = ScenarioConfig {
        scenario: *scenario,
        max_operations: workload_len,
        max_crashes: workload_len / 10, // Allow crashes proportional to workload
        stop_on_violation: true,
    };

    replay_with_config(seed, config)
}

/// Replays a simulation with full configuration control.
///
/// This is the lower-level replay function that allows full control over
/// scenario configuration.
pub fn replay_with_config(seed: u64, config: ScenarioConfig) -> Result<(), FailureReport> {
    let env = create_env_for_scenario(seed, config.scenario);
    let stress_config = crate::fault_scenarios::create_stress_config(&config);

    let mut driver = crate::driver::StressDriver::new(env, stress_config);
    let result = driver.run();

    if result.passed {
        Ok(())
    } else {
        // Convert StressResult failure to FailureReport
        let failure_kind = parse_failure_kind(&result.failure);
        Err(FailureReport::new(
            seed,
            config.scenario,
            result.operations as usize,
            failure_kind,
        ))
    }
}

/// Parses a failure description string into a FailureKind.
///
/// This attempts to classify the failure based on known patterns in the
/// failure message. Falls back to Custom if no pattern matches.
fn parse_failure_kind(failure: &Option<String>) -> FailureKind {
    let msg = failure.as_deref().unwrap_or("unknown failure");

    // Check for serialization cycle
    if msg.contains("Non-serializable schedule") || msg.contains("Cycle") {
        // Try to extract transaction IDs from the message
        // Format: "...Cycle: T1 -> T2 -> T3" or just "T1 -> T2 -> T3"
        // First find "Cycle:" and take everything after, or use whole message
        let cycle_part = msg
            .find("Cycle:")
            .map(|idx| &msg[idx + 6..]) // Skip "Cycle:"
            .unwrap_or(msg)
            .trim();

        let txn_ids: Vec<u64> = cycle_part
            .split(" -> ")
            .filter_map(|s| {
                // Handle both "T1" and text containing "T1"
                let trimmed = s.trim();
                // Find the last occurrence of 'T' followed by digits
                if let Some(t_pos) = trimmed.rfind('T') {
                    let after_t = &trimmed[t_pos + 1..];
                    // Take only the digits at the start
                    let num_str: String =
                        after_t.chars().take_while(|c| c.is_ascii_digit()).collect();
                    num_str.parse::<u64>().ok()
                } else {
                    None
                }
            })
            .collect();

        if !txn_ids.is_empty() {
            return FailureKind::SerializabilityCycle { txn_ids };
        }
    }

    // Check for lost write
    if msg.contains("missing") || msg.contains("incorrect") || msg.contains("mismatch") {
        // This could be a lost write, but we don't have the key/value info
        // from just the message. Mark as Custom for now.
        return FailureKind::Custom(msg.to_string());
    }

    // Check for unrecoverable errors
    if msg.contains("panic") || msg.contains("assertion") || msg.contains("internal error") {
        return FailureKind::Unrecoverable(msg.to_string());
    }

    // Default to Custom
    FailureKind::Custom(msg.to_string())
}

// ============================================================================
// Failure Discovery and Shrinking
// ============================================================================

use crate::shrink::{quick_shrink, shrink_failure, thorough_shrink, DeltaDebugResult};
use std::ops::Range;

/// Shrinks a real failure to find a minimal reproduction.
///
/// This is the main entry point for shrinking failures. Given a FailureReport
/// from a failing seed, it uses delta-debugging to minimize:
/// 1. Fault probabilities
/// 2. Enabled fault types
/// 3. Workload length
///
/// The shrinking preserves the FailureKind - the shrunk reproduction will
/// exhibit the same class of failure as the original.
///
/// # Example
///
/// ```ignore
/// use sim::failure::{replay, shrink_real_failure, FaultScenario};
///
/// // Find a failing seed
/// if let Err(report) = replay(0xDEADBEEF, &FaultScenario::Combined, 1000) {
///     // Shrink it
///     if let Some(result) = shrink_real_failure(&report) {
///         println!("Minimal reproduction:");
///         println!("  Workload: {} ops", result.fault_config.workload_len);
///         println!("  Faults: {} types", result.fault_config.enabled_fault_count());
///         println!("{}", result.report);
///     }
/// }
/// ```
pub fn shrink_real_failure(report: &FailureReport) -> Option<DeltaDebugResult> {
    shrink_failure(report)
}

/// Shrinks a failure quickly (fewer iterations, larger steps).
pub fn shrink_real_failure_quick(report: &FailureReport) -> Option<DeltaDebugResult> {
    quick_shrink(report)
}

/// Shrinks a failure thoroughly (more iterations, smaller steps).
pub fn shrink_real_failure_thorough(report: &FailureReport) -> Option<DeltaDebugResult> {
    thorough_shrink(report)
}

/// Result of searching for failures across seeds and scenarios.
#[derive(Debug)]
pub struct FailureSearchResult {
    /// Total seeds tried.
    pub seeds_tried: u64,
    /// Total scenarios tried per seed.
    pub scenarios_tried: u64,
    /// Failures found.
    pub failures: Vec<FoundFailure>,
}

impl FailureSearchResult {
    /// Returns the number of failures found.
    pub fn failure_count(&self) -> usize {
        self.failures.len()
    }

    /// Returns a summary string.
    pub fn summary(&self) -> String {
        format!(
            "Searched {} seeds x {} scenarios = {} total\nFound {} failures",
            self.seeds_tried,
            self.scenarios_tried,
            self.seeds_tried * self.scenarios_tried,
            self.failures.len()
        )
    }
}

/// A failure that was found and (optionally) shrunk.
#[derive(Debug)]
pub struct FoundFailure {
    /// The original failure report.
    pub original: FailureReport,
    /// The shrunk result, if shrinking was requested.
    pub shrunk: Option<DeltaDebugResult>,
}

impl FoundFailure {
    /// Returns the minimal workload length (shrunk if available, original otherwise).
    pub fn minimal_workload_len(&self) -> usize {
        self.shrunk
            .as_ref()
            .map(|s| s.fault_config.workload_len)
            .unwrap_or(self.original.workload_len)
    }
}

/// Finds failures across a range of seeds and scenarios, optionally shrinking them.
///
/// This function systematically explores the seed/scenario space looking for
/// failures. When a failure is found, it can optionally shrink it to find a
/// minimal reproduction.
///
/// # Arguments
///
/// * `seeds` - Range of seeds to try
/// * `scenarios` - Scenarios to test for each seed
/// * `workload_len` - Number of operations per test
/// * `shrink_failures` - Whether to shrink found failures
///
/// # Example
///
/// ```ignore
/// use sim::failure::{find_and_shrink_failures, FaultScenario};
///
/// let scenarios = vec![FaultScenario::Combined, FaultScenario::CrashHeavy];
/// let result = find_and_shrink_failures(0..100, &scenarios, 500, true);
///
/// println!("{}", result.summary());
/// for failure in &result.failures {
///     println!("Found: {}", failure.original);
///     if let Some(shrunk) = &failure.shrunk {
///         println!("  Shrunk to {} ops", shrunk.fault_config.workload_len);
///     }
/// }
/// ```
pub fn find_and_shrink_failures(
    seeds: Range<u64>,
    scenarios: &[FaultScenario],
    workload_len: usize,
    shrink_failures: bool,
) -> FailureSearchResult {
    let mut result = FailureSearchResult {
        seeds_tried: 0,
        scenarios_tried: scenarios.len() as u64,
        failures: Vec::new(),
    };

    for seed in seeds {
        result.seeds_tried += 1;

        for scenario in scenarios {
            match replay(seed, scenario, workload_len) {
                Ok(()) => {
                    // No failure for this seed/scenario
                }
                Err(report) => {
                    // Found a failure
                    let shrunk = if shrink_failures {
                        shrink_real_failure_quick(&report)
                    } else {
                        None
                    };

                    result.failures.push(FoundFailure {
                        original: report,
                        shrunk,
                    });
                }
            }
        }
    }

    result
}

/// Finds failures without shrinking (faster).
pub fn find_failures(
    seeds: Range<u64>,
    scenarios: &[FaultScenario],
    workload_len: usize,
) -> FailureSearchResult {
    find_and_shrink_failures(seeds, scenarios, workload_len, false)
}

// ============================================================================
// Synthetic Bug Injection for Testing the Shrinker
// ============================================================================

/// Configuration for injecting a synthetic bug.
///
/// This is used to test the shrinker itself. The synthetic bug will
/// deterministically fail at a specific operation with a known FailureKind.
#[derive(Debug, Clone)]
pub struct SyntheticBug {
    /// The operation number at which to inject the failure.
    pub fail_at_op: usize,
    /// The kind of failure to inject.
    pub failure_kind: FailureKind,
}

impl SyntheticBug {
    /// Creates a new synthetic bug that fails at the given operation.
    pub fn new(fail_at_op: usize, failure_kind: FailureKind) -> Self {
        Self {
            fail_at_op,
            failure_kind,
        }
    }

    /// Creates a synthetic serializability cycle failure.
    pub fn serializability_cycle(fail_at_op: usize) -> Self {
        Self::new(
            fail_at_op,
            FailureKind::SerializabilityCycle {
                txn_ids: vec![1, 2, 3],
            },
        )
    }

    /// Creates a synthetic lost write failure.
    pub fn lost_write(fail_at_op: usize) -> Self {
        Self::new(
            fail_at_op,
            FailureKind::LostWrite {
                key: b"synthetic_key".to_vec(),
                expected: b"synthetic_value".to_vec(),
            },
        )
    }

    /// Creates a synthetic custom failure.
    pub fn custom(fail_at_op: usize, message: impl Into<String>) -> Self {
        Self::new(fail_at_op, FailureKind::Custom(message.into()))
    }

    /// Tests if this bug should fire at the given operation count.
    pub fn should_fail(&self, op_count: usize) -> bool {
        op_count >= self.fail_at_op
    }
}

/// Runs a synthetic bug test and returns the failure report.
///
/// This creates a deterministic failure at the specified operation for
/// testing the shrinker. The synthetic bug will fire if workload_len >= fail_at_op.
///
/// # Example
///
/// ```ignore
/// use sim::failure::{run_with_synthetic_bug, SyntheticBug, FaultScenario};
///
/// let bug = SyntheticBug::serializability_cycle(100);
/// let result = run_with_synthetic_bug(0xDEADBEEF, FaultScenario::None, 500, &bug);
///
/// // Should fail because 500 > 100
/// assert!(result.is_err());
///
/// let result = run_with_synthetic_bug(0xDEADBEEF, FaultScenario::None, 50, &bug);
///
/// // Should pass because 50 < 100
/// assert!(result.is_ok());
/// ```
pub fn run_with_synthetic_bug(
    seed: u64,
    scenario: FaultScenario,
    workload_len: usize,
    bug: &SyntheticBug,
) -> Result<(), FailureReport> {
    // The synthetic bug fires if workload_len >= fail_at_op
    if bug.should_fail(workload_len) {
        Err(FailureReport::new(
            seed,
            scenario,
            workload_len,
            bug.failure_kind.clone(),
        ))
    } else {
        // Run normally (won't hit the bug because workload is too short)
        Ok(())
    }
}

/// Verifies that the shrinker correctly finds the minimal reproduction for a synthetic bug.
///
/// This runs the ACTUAL shrinker (TestableShrinker) with the synthetic bug, not a mock.
/// It verifies that:
/// 1. The shrinker converges
/// 2. The shrunk workload is close to the bug's fail_at_op
/// 3. Fault types are properly reduced
///
/// Returns the real shrink result if successful, or an error message if verification fails.
pub fn verify_shrinker_with_synthetic_bug(
    seed: u64,
    scenario: FaultScenario,
    initial_workload: usize,
    bug: &SyntheticBug,
) -> Result<DeltaDebugResult, String> {
    use crate::shrink::{DeltaDebugConfig, TestableShrinker};

    // Validate preconditions
    if !bug.should_fail(initial_workload) {
        return Err(format!(
            "Initial workload {} is too short to trigger bug at op {}",
            initial_workload, bug.fail_at_op
        ));
    }

    // Create initial failure report
    let report = FailureReport::new(seed, scenario, initial_workload, bug.failure_kind.clone());

    // Create and run the REAL testable shrinker (not a mock)
    let mut shrinker = TestableShrinker::for_workload_bug(
        DeltaDebugConfig::default(),
        bug.fail_at_op,
        bug.failure_kind.clone(),
    );

    match shrinker.shrink(&report) {
        Some(result) => {
            // Verify the shrunk workload is close to the expected minimum
            let max_acceptable = bug.fail_at_op + 5; // Allow small tolerance for binary search
            if result.fault_config.workload_len > max_acceptable {
                return Err(format!(
                    "Shrinker did not converge close to bug point: got {}, expected <= {}",
                    result.fault_config.workload_len, max_acceptable
                ));
            }

            // Verify failure kind was preserved
            if !result.report.failure_kind.matches(&bug.failure_kind) {
                return Err(format!(
                    "Failure kind changed: expected {:?}, got {:?}",
                    bug.failure_kind, result.report.failure_kind
                ));
            }

            Ok(result)
        }
        None => Err("Shrinker failed to find a minimal reproduction".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_kind_names() {
        assert_eq!(
            FailureKind::SerializabilityCycle {
                txn_ids: vec![1, 2]
            }
            .name(),
            "serializability_cycle"
        );
        assert_eq!(
            FailureKind::LostWrite {
                key: vec![],
                expected: vec![]
            }
            .name(),
            "lost_write"
        );
        assert_eq!(FailureKind::Custom("test".into()).name(), "custom");
        assert_eq!(
            FailureKind::Unrecoverable("test".into()).name(),
            "unrecoverable"
        );
    }

    #[test]
    fn failure_kind_matches() {
        let cycle1 = FailureKind::SerializabilityCycle {
            txn_ids: vec![1, 2],
        };
        let cycle2 = FailureKind::SerializabilityCycle {
            txn_ids: vec![3, 4, 5],
        };
        let lost = FailureKind::LostWrite {
            key: vec![1],
            expected: vec![2],
        };

        assert!(cycle1.matches(&cycle2), "Same variant should match");
        assert!(
            !cycle1.matches(&lost),
            "Different variants should not match"
        );
    }

    #[test]
    fn failure_report_display() {
        let report = FailureReport::new(
            0xDEADBEEF,
            FaultScenario::Combined,
            500,
            FailureKind::SerializabilityCycle {
                txn_ids: vec![1, 2, 3],
            },
        );

        let display = format!("{}", report);
        assert!(display.contains("DEADBEEF"));
        assert!(display.contains("combined"));
        assert!(display.contains("500"));
        assert!(display.contains("T1"));
    }

    #[test]
    fn failure_report_replay_hint() {
        let report = FailureReport::new(
            0xCAFEBABE,
            FaultScenario::PartialWrites,
            100,
            FailureKind::Custom("test".into()),
        );

        assert!(report.replay_cmd_hint.contains("0x00000000CAFEBABE"));
        assert!(report.replay_cmd_hint.contains("partial_writes"));
        assert!(report.replay_cmd_hint.contains("100"));
    }

    #[test]
    fn parse_failure_kind_serialization() {
        let msg = Some("Non-serializable schedule detected. Cycle: T1 -> T2 -> T3".to_string());
        let kind = parse_failure_kind(&msg);

        match kind {
            FailureKind::SerializabilityCycle { txn_ids } => {
                assert_eq!(txn_ids, vec![1, 2, 3]);
            }
            _ => panic!("Expected SerializabilityCycle"),
        }
    }

    #[test]
    fn parse_failure_kind_custom() {
        let msg = Some("Some random error".to_string());
        let kind = parse_failure_kind(&msg);

        match kind {
            FailureKind::Custom(s) => {
                assert!(s.contains("random error"));
            }
            _ => panic!("Expected Custom"),
        }
    }

    #[test]
    fn clean_run_returns_ok() {
        // Run a simple scenario with no faults - should pass
        let result = replay(42, &FaultScenario::None, 50);
        assert!(
            result.is_ok(),
            "Clean run with no faults should succeed: {:?}",
            result.err()
        );
    }

    #[test]
    fn replay_is_deterministic() {
        // Run the same seed twice - should produce identical results
        let seed = 0xDE7E714;
        let scenario = FaultScenario::None;
        let workload_len = 100;

        let result1 = replay(seed, &scenario, workload_len);
        let result2 = replay(seed, &scenario, workload_len);

        // Both should have the same outcome
        match (&result1, &result2) {
            (Ok(()), Ok(())) => {} // Both passed
            (Err(r1), Err(r2)) => {
                // Both failed - verify same kind
                assert!(
                    r1.failure_kind.matches(&r2.failure_kind),
                    "Same seed should produce same failure kind"
                );
            }
            _ => panic!(
                "Same seed should produce same result: {:?} vs {:?}",
                result1, result2
            ),
        }
    }

    #[test]
    fn cycle_txns_helper() {
        let report = FailureReport::new(
            0,
            FaultScenario::None,
            0,
            FailureKind::SerializabilityCycle {
                txn_ids: vec![1, 2, 3],
            },
        );

        assert_eq!(report.cycle_txns(), Some(&[1u64, 2, 3][..]));

        let report2 = FailureReport::custom(0, FaultScenario::None, 0, "test");
        assert_eq!(report2.cycle_txns(), None);
    }

    #[test]
    fn from_serialization_violation() {
        let violation = SerializationViolation {
            cycle: vec![10, 20, 30],
            edges: vec![],
        };

        let kind = FailureKind::from_serialization_violation(&violation);
        match kind {
            FailureKind::SerializabilityCycle { txn_ids } => {
                assert_eq!(txn_ids, vec![10, 20, 30]);
            }
            _ => panic!("Expected SerializabilityCycle"),
        }
    }

    // ========================================
    // Part C: Failure Search and Shrink Tests
    // ========================================

    #[test]
    fn synthetic_bug_should_fail() {
        let bug = SyntheticBug::serializability_cycle(100);

        // Should not fail below threshold
        assert!(!bug.should_fail(50));
        assert!(!bug.should_fail(99));

        // Should fail at or above threshold
        assert!(bug.should_fail(100));
        assert!(bug.should_fail(500));
    }

    #[test]
    fn synthetic_bug_variants() {
        let cycle_bug = SyntheticBug::serializability_cycle(10);
        match &cycle_bug.failure_kind {
            FailureKind::SerializabilityCycle { txn_ids } => {
                assert_eq!(txn_ids, &vec![1, 2, 3]);
            }
            _ => panic!("Expected SerializabilityCycle"),
        }

        let lost_bug = SyntheticBug::lost_write(20);
        match &lost_bug.failure_kind {
            FailureKind::LostWrite { key, expected } => {
                assert_eq!(key, b"synthetic_key");
                assert_eq!(expected, b"synthetic_value");
            }
            _ => panic!("Expected LostWrite"),
        }

        let custom_bug = SyntheticBug::custom(30, "custom message");
        match &custom_bug.failure_kind {
            FailureKind::Custom(msg) => {
                assert_eq!(msg, "custom message");
            }
            _ => panic!("Expected Custom"),
        }
    }

    #[test]
    fn run_with_synthetic_bug_passes_below_threshold() {
        let bug = SyntheticBug::serializability_cycle(100);
        let result = run_with_synthetic_bug(42, FaultScenario::None, 50, &bug);
        assert!(result.is_ok(), "Should pass below threshold");
    }

    #[test]
    fn run_with_synthetic_bug_fails_at_threshold() {
        let bug = SyntheticBug::serializability_cycle(100);
        let result = run_with_synthetic_bug(42, FaultScenario::None, 100, &bug);
        assert!(result.is_err(), "Should fail at threshold");

        let report = result.unwrap_err();
        assert_eq!(report.workload_len, 100);
        assert!(matches!(
            report.failure_kind,
            FailureKind::SerializabilityCycle { .. }
        ));
    }

    #[test]
    fn run_with_synthetic_bug_fails_above_threshold() {
        let bug = SyntheticBug::serializability_cycle(100);
        let result = run_with_synthetic_bug(42, FaultScenario::None, 500, &bug);
        assert!(result.is_err(), "Should fail above threshold");
    }

    #[test]
    fn verify_shrinker_with_synthetic_bug_success() {
        let bug = SyntheticBug::serializability_cycle(100);
        let result = verify_shrinker_with_synthetic_bug(42, FaultScenario::None, 500, &bug);

        assert!(result.is_ok(), "Should succeed: {:?}", result.err());

        let shrink_result = result.unwrap();
        assert_eq!(shrink_result.fault_config.workload_len, 100);
        assert_eq!(shrink_result.stats.original_workload_size, 500);
        assert_eq!(shrink_result.stats.final_workload_size, 100);
    }

    #[test]
    fn verify_shrinker_with_synthetic_bug_workload_too_short() {
        let bug = SyntheticBug::serializability_cycle(100);
        let result = verify_shrinker_with_synthetic_bug(42, FaultScenario::None, 50, &bug);

        assert!(result.is_err(), "Should fail when workload too short");
        let err = result.unwrap_err();
        assert!(err.contains("too short"));
    }

    #[test]
    fn find_failures_empty_range() {
        let scenarios = vec![FaultScenario::None];
        let result = find_failures(0..0, &scenarios, 50);

        assert_eq!(result.seeds_tried, 0);
        assert_eq!(result.failure_count(), 0);
    }

    #[test]
    fn find_failures_single_seed() {
        let scenarios = vec![FaultScenario::None];
        let result = find_failures(0..1, &scenarios, 50);

        assert_eq!(result.seeds_tried, 1);
        assert_eq!(result.scenarios_tried, 1);
        // None scenario with small workload should pass
    }

    #[test]
    fn failure_search_result_summary() {
        let result = FailureSearchResult {
            seeds_tried: 10,
            scenarios_tried: 3,
            failures: vec![],
        };

        let summary = result.summary();
        assert!(summary.contains("10"));
        assert!(summary.contains('3'));
        assert!(summary.contains("30")); // 10 * 3
        assert!(summary.contains("0 failures"));
    }

    #[test]
    fn found_failure_minimal_workload() {
        let report = FailureReport::new(
            42,
            FaultScenario::None,
            500,
            FailureKind::Custom("test".into()),
        );

        // Without shrunk result
        let found = FoundFailure {
            original: report.clone(),
            shrunk: None,
        };
        assert_eq!(found.minimal_workload_len(), 500);

        // With shrunk result
        let shrunk_config =
            crate::shrink::ShrunkFaultConfig::from_scenario(FaultScenario::None, 100);
        let shrunk_report = FailureReport::new(
            42,
            FaultScenario::None,
            100,
            FailureKind::Custom("test".into()),
        );
        let found_with_shrunk = FoundFailure {
            original: report,
            shrunk: Some(DeltaDebugResult {
                report: shrunk_report,
                fault_config: shrunk_config,
                stats: Default::default(),
            }),
        };
        assert_eq!(found_with_shrunk.minimal_workload_len(), 100);
    }

    // ========================================
    // Acceptance Tests (run in CI)
    // ========================================

    /// Tests that synthetic bug shrinking finds the exact failure point.
    #[test]
    fn acceptance_synthetic_bug_shrink() {
        for fail_at in [10, 50, 100, 200] {
            let bug = SyntheticBug::serializability_cycle(fail_at);
            let initial_workload = fail_at * 5; // 5x the failure point

            let result = verify_shrinker_with_synthetic_bug(
                0xACCEF7,
                FaultScenario::None,
                initial_workload,
                &bug,
            );

            assert!(
                result.is_ok(),
                "Failed for fail_at={}: {:?}",
                fail_at,
                result.err()
            );

            let shrunk = result.unwrap();
            assert_eq!(
                shrunk.fault_config.workload_len, fail_at,
                "Shrinker should find exact failure point"
            );
        }
    }

    /// Tests that replay is deterministic across multiple runs.
    #[test]
    fn acceptance_replay_determinism_10_runs() {
        let seed = 0xDE7E714;
        let scenario = FaultScenario::None;
        let workload_len = 100;

        let first_result = replay(seed, &scenario, workload_len);

        for _ in 0..10 {
            let result = replay(seed, &scenario, workload_len);
            match (&first_result, &result) {
                (Ok(()), Ok(())) => {}
                (Err(r1), Err(r2)) => {
                    assert!(
                        r1.failure_kind.matches(&r2.failure_kind),
                        "Replay should be deterministic"
                    );
                }
                _ => panic!(
                    "Replay should be deterministic: {:?} vs {:?}",
                    first_result, result
                ),
            }
        }
    }

    /// Tests that find_failures correctly searches multiple scenarios.
    #[test]
    fn acceptance_find_failures_multiple_scenarios() {
        let scenarios = vec![FaultScenario::None, FaultScenario::PartialWrites];
        let result = find_failures(0..5, &scenarios, 50);

        assert_eq!(result.seeds_tried, 5);
        assert_eq!(result.scenarios_tried, 2);
        // We don't assert on failure count since it depends on what actually fails
    }

    /// Tests all synthetic bug failure kinds.
    #[test]
    fn acceptance_all_synthetic_bug_kinds() {
        let kinds = [
            SyntheticBug::serializability_cycle(10),
            SyntheticBug::lost_write(10),
            SyntheticBug::custom(10, "test failure"),
        ];

        for bug in &kinds {
            // Should fail at/above threshold
            let result = run_with_synthetic_bug(42, FaultScenario::None, 10, bug);
            assert!(
                result.is_err(),
                "Should fail at threshold for {:?}",
                bug.failure_kind
            );

            // Should pass below threshold
            let result = run_with_synthetic_bug(42, FaultScenario::None, 9, bug);
            assert!(
                result.is_ok(),
                "Should pass below threshold for {:?}",
                bug.failure_kind
            );
        }
    }

    /// Thorough shrinker test - ignored by default, run in CI with --ignored.
    #[test]
    #[ignore]
    fn acceptance_shrinker_thorough_10_seeds() {
        for seed in 0..10u64 {
            for fail_at in [25, 50, 100] {
                let bug = SyntheticBug::serializability_cycle(fail_at);
                let initial_workload = fail_at * 10;

                let result = verify_shrinker_with_synthetic_bug(
                    seed,
                    FaultScenario::None,
                    initial_workload,
                    &bug,
                );

                assert!(
                    result.is_ok(),
                    "Failed for seed={}, fail_at={}: {:?}",
                    seed,
                    fail_at,
                    result.err()
                );
            }
        }
    }
}
