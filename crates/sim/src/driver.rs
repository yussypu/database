//! Main stress test driver for simulation testing.
//!
//! The driver orchestrates the entire simulation:
//! - Generates deterministic workloads from a seed
//! - Applies operations to both the model and the real database
//! - Injects faults at controlled points
//! - Verifies invariants after each operation and crash
//! - Records traces for replay and shrinking
//!
//! # Example
//!
//! ```ignore
//! use sim::{StressConfig, StressDriver};
//! use runtime::{SimEnv, SimEnvConfig};
//!
//! let env = SimEnv::new(SimEnvConfig::with_seed(0xDEADBEEF));
//! let config = StressConfig::default();
//! let mut driver = StressDriver::new(env, config);
//!
//! let result = driver.run();
//! if !result.passed {
//!     println!("Failure: {}", result.summary());
//! }
//! ```

use crate::fault::{Fault, FaultConfig, FaultInjector};
use crate::invariant::{full_verification, InvariantChecker, InvariantConfig, InvariantReport};
use crate::model::Model;
use crate::shrink::MinimalRepro;
use crate::workload::{Operation, OperationLog, WorkloadConfig, WorkloadGenerator};
use runtime::{Path, SimEnv, SimEnvConfig};
use storage::engine::{EngineConfig, LsmEngine};
use tracing::{debug, error, info};

/// Configuration for the stress test driver.
#[derive(Debug, Clone)]
pub struct StressConfig {
    /// Workload generation configuration.
    pub workload: WorkloadConfig,
    /// Fault injection configuration.
    pub faults: FaultConfig,
    /// Invariant checking configuration.
    pub invariants: InvariantConfig,
    /// Engine configuration.
    pub engine: EngineConfig,
    /// Maximum number of operations per run.
    pub max_operations: usize,
    /// Maximum number of crash cycles.
    pub max_crashes: usize,
    /// Database path.
    pub db_path: String,
    /// Stop on first invariant violation.
    pub stop_on_violation: bool,
    /// Verify after every write (slow but thorough).
    pub verify_after_writes: bool,
    /// Perform full verification after each crash.
    pub full_verify_after_crash: bool,
}

impl Default for StressConfig {
    fn default() -> Self {
        Self {
            workload: WorkloadConfig::default(),
            faults: FaultConfig::default(),
            invariants: InvariantConfig::default(),
            engine: EngineConfig::default(),
            max_operations: 1000,
            max_crashes: 10,
            db_path: "/db".to_string(),
            stop_on_violation: true,
            verify_after_writes: false,
            full_verify_after_crash: true,
        }
    }
}

impl StressConfig {
    /// Creates a stress testing configuration.
    pub fn stress() -> Self {
        Self {
            workload: WorkloadConfig::stress(),
            faults: FaultConfig::stress(),
            ..Default::default()
        }
    }

    /// Creates a crash-heavy testing configuration.
    pub fn crash_heavy() -> Self {
        Self {
            workload: WorkloadConfig::default(),
            faults: FaultConfig::crash_heavy(),
            max_crashes: 100,
            ..Default::default()
        }
    }

    /// Creates a quick test configuration.
    pub fn quick() -> Self {
        Self {
            max_operations: 100,
            max_crashes: 3,
            ..Default::default()
        }
    }
}

/// Result of a stress test run.
#[derive(Debug)]
pub struct StressResult {
    /// Whether the test passed.
    pub passed: bool,
    /// The seed used.
    pub seed: u64,
    /// Total operations performed.
    pub operations: u64,
    /// Total crashes simulated.
    pub crashes: u64,
    /// Invariant report.
    pub invariant_report: InvariantReport,
    /// Operation log for replay.
    pub operation_log: OperationLog,
    /// Fault history.
    pub fault_history: Vec<(u64, Fault)>,
    /// Failure description if any.
    pub failure: Option<String>,
}

impl StressResult {
    /// Returns a summary of the test result.
    pub fn summary(&self) -> String {
        if self.passed {
            format!(
                "PASS: {} operations, {} crashes, seed=0x{:016X}",
                self.operations, self.crashes, self.seed
            )
        } else {
            format!(
                "FAIL: {} operations, {} crashes, seed=0x{:016X}\n  {}",
                self.operations,
                self.crashes,
                self.seed,
                self.failure.as_deref().unwrap_or("unknown")
            )
        }
    }

    /// Creates a minimal reproduction from this result.
    pub fn to_repro(&self) -> MinimalRepro {
        let failure_op = self.operations;
        let description = self
            .failure
            .clone()
            .unwrap_or_else(|| "unknown failure".to_string());

        MinimalRepro::new(self.seed, failure_op, description)
            .with_operations(
                self.operation_log
                    .ops()
                    .iter()
                    .map(|r| r.op.clone())
                    .collect(),
            )
            .with_faults(self.fault_history.clone())
    }
}

/// The main stress test driver.
pub struct StressDriver {
    env: SimEnv,
    config: StressConfig,
    model: Model,
    fault_injector: FaultInjector,
    invariant_checker: InvariantChecker,
    operation_log: OperationLog,
    op_count: u64,
    crash_count: u64,
}

impl StressDriver {
    /// Creates a new stress driver.
    pub fn new(env: SimEnv, config: StressConfig) -> Self {
        Self {
            env: env.clone(),
            model: Model::new(),
            fault_injector: FaultInjector::new(config.faults.clone()),
            invariant_checker: InvariantChecker::new(config.invariants.clone()),
            operation_log: OperationLog::new(),
            config,
            op_count: 0,
            crash_count: 0,
        }
    }

    /// Returns the seed for this run.
    pub fn seed(&self) -> u64 {
        self.env.seed()
    }

    /// Runs the stress test.
    pub fn run(&mut self) -> StressResult {
        info!(
            seed = self.seed(),
            max_ops = self.config.max_operations,
            max_crashes = self.config.max_crashes,
            "Starting stress test"
        );

        let db_path_str = self.config.db_path.clone();
        let db_path = Path::new(&db_path_str);

        // Open the database
        let mut engine =
            match LsmEngine::open(self.env.clone(), db_path, self.config.engine.clone()) {
                Ok(e) => e,
                Err(err) => {
                    return StressResult {
                        passed: false,
                        seed: self.seed(),
                        operations: 0,
                        crashes: 0,
                        invariant_report: InvariantReport::new(),
                        operation_log: self.operation_log.clone(),
                        fault_history: Vec::new(),
                        failure: Some(format!("Failed to open database: {}", err)),
                    };
                }
            };

        let generator = WorkloadGenerator::new(self.env.clone(), self.config.workload.clone());

        // Main stress loop
        while self.op_count < self.config.max_operations as u64
            && self.crash_count < self.config.max_crashes as u64
        {
            // Generate next operation
            let op = generator.next_op();

            // Execute operation
            if let Err(err) = self.execute_op(&mut engine, &op) {
                return self.fail(format!("Operation failed: {}", err));
            }

            // Check for fault injection
            if let Some(fault) = self.fault_injector.maybe_crash(&self.env) {
                debug!(op = self.op_count, "Injecting fault: {:?}", fault);

                // Simulate crash
                drop(engine);
                self.env.simulate_crash();
                self.crash_count += 1;
                self.fault_injector.enter_recovery();

                // Verify after crash
                if self.config.full_verify_after_crash {
                    // Reopen for verification
                    engine = match LsmEngine::open(
                        self.env.clone(),
                        db_path,
                        self.config.engine.clone(),
                    ) {
                        Ok(e) => e,
                        Err(err) => {
                            return self.fail(format!("Failed to reopen after crash: {}", err));
                        }
                    };

                    let report =
                        full_verification(&self.model, |key| engine.get(key).ok().flatten());

                    if !report.all_passed() {
                        let failures: Vec<_> = report.failures().collect();
                        return self.fail(format!(
                            "Durability violation after crash {}: {} failures",
                            self.crash_count,
                            failures.len()
                        ));
                    }
                } else {
                    // Just reopen
                    engine = match LsmEngine::open(
                        self.env.clone(),
                        db_path,
                        self.config.engine.clone(),
                    ) {
                        Ok(e) => e,
                        Err(err) => {
                            return self.fail(format!("Failed to reopen after crash: {}", err));
                        }
                    };
                }

                self.fault_injector.exit_recovery();
            }

            // Check invariants if we should stop on violation
            if self.config.stop_on_violation && self.invariant_checker.has_violations() {
                let violations: Vec<_> = self.invariant_checker.violations().to_vec();
                return self.fail(format!("Invariant violation: {:?}", violations.first()));
            }
        }

        // Final verification
        let final_report = full_verification(&self.model, |key| engine.get(key).ok().flatten());

        if !final_report.all_passed() {
            let failures: Vec<_> = final_report.failures().collect();
            return self.fail(format!(
                "Final verification failed: {} mismatches",
                failures.len()
            ));
        }

        info!(
            ops = self.op_count,
            crashes = self.crash_count,
            "Stress test completed successfully"
        );

        StressResult {
            passed: true,
            seed: self.seed(),
            operations: self.op_count,
            crashes: self.crash_count,
            invariant_report: final_report,
            operation_log: self.operation_log.clone(),
            fault_history: self.fault_injector.fault_history().to_vec(),
            failure: None,
        }
    }

    /// Executes a single operation.
    fn execute_op(&mut self, engine: &mut LsmEngine<SimEnv>, op: &Operation) -> Result<(), String> {
        self.op_count += 1;

        match op {
            Operation::Put { key, value } => {
                engine.put(key, value).map_err(|e| e.to_string())?;
                self.model.put(key, value);
                self.operation_log.record_write(op.clone());

                // Verify read-your-writes
                if self.config.verify_after_writes {
                    let actual = engine.get(key).map_err(|e| e.to_string())?;
                    let result = crate::invariant::check_read_your_writes(key, Some(value), actual);
                    self.invariant_checker.record(self.op_count, &result);
                }
            }
            Operation::Get { key } => {
                let actual = engine.get(key).map_err(|e| e.to_string())?;
                let expected = self.model.get_value(key).cloned();
                self.operation_log.record_get(key.clone(), actual.clone());

                // Verify read correctness
                let result =
                    crate::invariant::check_read_your_writes(key, expected.as_ref(), actual);
                self.invariant_checker.record(self.op_count, &result);
            }
            Operation::Delete { key } => {
                engine.delete(key).map_err(|e| e.to_string())?;
                self.model.delete(key);
                self.operation_log.record_write(op.clone());

                // Verify read-your-writes (should return None)
                if self.config.verify_after_writes {
                    let actual = engine.get(key).map_err(|e| e.to_string())?;
                    let result = crate::invariant::check_read_your_writes(key, None, actual);
                    self.invariant_checker.record(self.op_count, &result);
                }
            }
            Operation::Flush => {
                engine.flush().map_err(|e| e.to_string())?;
            }
            Operation::Compact => {
                let _ = engine.maybe_compact();
            }
            Operation::Crash => {
                // Crash is handled at the driver level
            }
        }

        Ok(())
    }

    /// Creates a failure result.
    fn fail(&self, message: String) -> StressResult {
        error!(
            seed = self.seed(),
            op = self.op_count,
            "Stress test failed: {}",
            message
        );

        StressResult {
            passed: false,
            seed: self.seed(),
            operations: self.op_count,
            crashes: self.crash_count,
            invariant_report: InvariantReport::new(),
            operation_log: self.operation_log.clone(),
            fault_history: self.fault_injector.fault_history().to_vec(),
            failure: Some(message),
        }
    }
}

/// Runs a stress test with the given seed.
pub fn run_stress_test(seed: u64, config: StressConfig) -> StressResult {
    let env = SimEnv::new(SimEnvConfig::with_seed(seed));
    let mut driver = StressDriver::new(env, config);
    driver.run()
}

/// Runs multiple stress tests with different seeds.
pub fn run_stress_tests(
    seeds: impl Iterator<Item = u64>,
    config: StressConfig,
) -> Vec<StressResult> {
    seeds
        .map(|seed| run_stress_test(seed, config.clone()))
        .collect()
}

/// Replays a specific seed with the given configuration.
pub fn replay_seed(seed: u64, config: StressConfig) -> StressResult {
    info!(seed = seed, "Replaying seed");
    run_stress_test(seed, config)
}

/// Generates a range of seeds for stress testing.
pub fn seed_range(base: u64, count: u64) -> impl Iterator<Item = u64> {
    (0..count).map(move |i| base.wrapping_add(i))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_stress_test() {
        let config = StressConfig {
            max_operations: 100,
            max_crashes: 2,
            faults: FaultConfig::none(),
            ..Default::default()
        };

        let result = run_stress_test(42, config);
        assert!(
            result.passed,
            "Basic stress test should pass: {}",
            result.summary()
        );
    }

    /// Transaction stress test with crash injection.
    ///
    /// This exercises the stress driver with intentional crashes to verify
    /// crash recovery under transactional workloads.
    #[test]
    fn txn_stress_with_crashes() {
        let config = StressConfig {
            max_operations: 200,
            max_crashes: 5,
            faults: FaultConfig {
                crash_probability: 0.1,
                min_ops_between_crashes: 10,
                ..Default::default()
            },
            ..Default::default()
        };

        let result = run_stress_test(12345, config);
        assert!(
            result.passed,
            "Stress test with crashes should pass: {}",
            result.summary()
        );
        assert!(result.crashes > 0, "Should have had some crashes");
    }

    #[test]
    fn stress_test_deterministic() {
        let config = StressConfig {
            max_operations: 50,
            max_crashes: 2,
            faults: FaultConfig {
                crash_probability: 0.2,
                min_ops_between_crashes: 5,
                ..Default::default()
            },
            ..Default::default()
        };

        let result1 = run_stress_test(999, config.clone());
        let result2 = run_stress_test(999, config);

        assert_eq!(result1.operations, result2.operations);
        assert_eq!(result1.crashes, result2.crashes);
        assert_eq!(result1.passed, result2.passed);
    }

    #[test]
    fn stress_test_write_heavy() {
        let config = StressConfig {
            workload: WorkloadConfig::write_heavy(),
            max_operations: 200,
            max_crashes: 3,
            faults: FaultConfig {
                crash_probability: 0.05,
                min_ops_between_crashes: 20,
                ..Default::default()
            },
            ..Default::default()
        };

        let result = run_stress_test(0xBEEF, config);
        assert!(
            result.passed,
            "Write-heavy test should pass: {}",
            result.summary()
        );
    }

    #[test]
    fn multiple_seeds() {
        let config = StressConfig::quick();
        let results: Vec<_> = seed_range(100, 10)
            .map(|seed| run_stress_test(seed, config.clone()))
            .collect();

        for result in &results {
            assert!(
                result.passed,
                "Seed {} failed: {}",
                result.seed,
                result.summary()
            );
        }
    }

    #[test]
    fn stress_result_to_repro() {
        let config = StressConfig::quick();
        let result = run_stress_test(0xDEAD, config);

        let repro = result.to_repro();
        assert_eq!(repro.seed, result.seed);
    }
}
