//! Predefined fault injection scenarios for simulation testing.
//!
//! This module provides high-level fault scenarios that combine the driver-level
//! fault injection (crashes, bit flips) with the runtime-level fault injection
//! (partial writes, disk full, slow writes, clock skew, process pauses).
//!
//! # Scenario Types
//!
//! - **PartialWrites**: Tests torn write recovery
//! - **DiskFull**: Tests ENOSPC handling and recovery
//! - **SlowDisk**: Tests timing behavior under slow IO
//! - **ClockSkew**: Tests clock drift handling
//! - **ProcessPause**: Tests GC pause-like scenarios
//! - **Combined**: Multiple faults at once for stress testing
//!
//! # Example
//!
//! ```ignore
//! use sim::fault_scenarios::{FaultScenario, run_scenario};
//!
//! // Run a partial writes scenario
//! let result = run_scenario(0xDEADBEEF, FaultScenario::PartialWrites);
//! assert!(result.passed);
//!
//! // Run combined faults for maximum stress
//! let result = run_scenario(0xBEEF, FaultScenario::Combined);
//! ```

use crate::driver::{StressConfig, StressResult};
use crate::fault::FaultConfig as DriverFaultConfig;
use runtime::{Duration, FaultConfig as RuntimeFaultConfig, SimEnv, SimEnvConfig};

/// A predefined fault injection scenario.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultScenario {
    /// No faults - baseline testing.
    None,
    /// Partial write faults only - tests torn write recovery.
    PartialWrites,
    /// Disk full faults only - tests ENOSPC handling.
    DiskFull,
    /// Slow write faults only - tests timing behavior.
    SlowDisk,
    /// Clock skew faults only - tests clock drift handling.
    ClockSkew,
    /// Process pause faults only - tests GC pause-like scenarios.
    ProcessPause,
    /// Crash faults only (driver-level).
    CrashHeavy,
    /// Combined partial writes + crashes.
    PartialWritesWithCrashes,
    /// Combined disk scenarios (slow + full).
    DiskStress,
    /// Combined timing faults (clock skew + process pause).
    TimingChaos,
    /// All faults enabled at moderate rates.
    Combined,
    /// Aggressive fault injection for maximum stress.
    Chaos,
}

impl FaultScenario {
    /// Returns a human-readable name for the scenario.
    pub fn name(&self) -> &'static str {
        match self {
            FaultScenario::None => "none",
            FaultScenario::PartialWrites => "partial_writes",
            FaultScenario::DiskFull => "disk_full",
            FaultScenario::SlowDisk => "slow_disk",
            FaultScenario::ClockSkew => "clock_skew",
            FaultScenario::ProcessPause => "process_pause",
            FaultScenario::CrashHeavy => "crash_heavy",
            FaultScenario::PartialWritesWithCrashes => "partial_writes_with_crashes",
            FaultScenario::DiskStress => "disk_stress",
            FaultScenario::TimingChaos => "timing_chaos",
            FaultScenario::Combined => "combined",
            FaultScenario::Chaos => "chaos",
        }
    }

    /// Returns a description of the scenario.
    pub fn description(&self) -> &'static str {
        match self {
            FaultScenario::None => "Baseline testing with no faults",
            FaultScenario::PartialWrites => "Tests torn write recovery",
            FaultScenario::DiskFull => "Tests ENOSPC handling and recovery",
            FaultScenario::SlowDisk => "Tests behavior under slow disk IO",
            FaultScenario::ClockSkew => "Tests clock drift handling",
            FaultScenario::ProcessPause => "Tests GC pause-like scenarios",
            FaultScenario::CrashHeavy => "Aggressive crash testing",
            FaultScenario::PartialWritesWithCrashes => "Torn writes combined with crashes",
            FaultScenario::DiskStress => "Slow disk + disk full scenarios",
            FaultScenario::TimingChaos => "Clock skew + process pause chaos",
            FaultScenario::Combined => "All faults at moderate rates",
            FaultScenario::Chaos => "Maximum stress with all faults aggressive",
        }
    }

    /// Returns the runtime fault configuration for this scenario.
    pub fn runtime_config(&self) -> RuntimeFaultConfig {
        match self {
            FaultScenario::None => RuntimeFaultConfig::default(),

            FaultScenario::PartialWrites => RuntimeFaultConfig::with_partial_writes(0.3),

            FaultScenario::DiskFull => RuntimeFaultConfig::with_disk_full(1024 * 1024), // 1MB

            FaultScenario::SlowDisk => {
                RuntimeFaultConfig::with_slow_writes(0.2, Duration::from_millis(100))
            }

            FaultScenario::ClockSkew => {
                RuntimeFaultConfig::with_clock_skew(0.3, Duration::from_secs(1))
            }

            FaultScenario::ProcessPause => {
                RuntimeFaultConfig::with_process_pause(0.1, Duration::from_millis(500))
            }

            FaultScenario::CrashHeavy => {
                // CrashHeavy uses driver-level faults, runtime is clean
                RuntimeFaultConfig::default()
            }

            FaultScenario::PartialWritesWithCrashes => RuntimeFaultConfig::with_partial_writes(0.2),

            FaultScenario::DiskStress => RuntimeFaultConfig {
                partial_write_prob: 0.0,
                disk_full_threshold: 2 * 1024 * 1024, // 2MB
                slow_write_prob: 0.3,
                slow_write_duration: Duration::from_millis(50),
                clock_skew_prob: 0.0,
                clock_skew_max: Duration::ZERO,
                process_pause_prob: 0.0,
                process_pause_duration: Duration::ZERO,
            },

            FaultScenario::TimingChaos => RuntimeFaultConfig {
                partial_write_prob: 0.0,
                disk_full_threshold: 0,
                slow_write_prob: 0.0,
                slow_write_duration: Duration::ZERO,
                clock_skew_prob: 0.5,
                clock_skew_max: Duration::from_secs(2),
                process_pause_prob: 0.3,
                process_pause_duration: Duration::from_millis(200),
            },

            FaultScenario::Combined => RuntimeFaultConfig {
                partial_write_prob: 0.1,
                disk_full_threshold: 0, // disabled for stability
                slow_write_prob: 0.1,
                slow_write_duration: Duration::from_millis(50),
                clock_skew_prob: 0.2,
                clock_skew_max: Duration::from_millis(500),
                process_pause_prob: 0.1,
                process_pause_duration: Duration::from_millis(100),
            },

            FaultScenario::Chaos => RuntimeFaultConfig {
                partial_write_prob: 0.3,
                disk_full_threshold: 5 * 1024 * 1024, // 5MB before ENOSPC
                slow_write_prob: 0.2,
                slow_write_duration: Duration::from_millis(100),
                clock_skew_prob: 0.4,
                clock_skew_max: Duration::from_secs(1),
                process_pause_prob: 0.2,
                process_pause_duration: Duration::from_millis(300),
            },
        }
    }

    /// Returns the driver-level fault configuration for this scenario.
    pub fn driver_config(&self) -> DriverFaultConfig {
        match self {
            FaultScenario::None => DriverFaultConfig::none(),

            FaultScenario::PartialWrites
            | FaultScenario::DiskFull
            | FaultScenario::SlowDisk
            | FaultScenario::ClockSkew
            | FaultScenario::ProcessPause
            | FaultScenario::DiskStress
            | FaultScenario::TimingChaos => {
                // These scenarios focus on runtime faults, minimal driver faults
                DriverFaultConfig::none()
            }

            FaultScenario::CrashHeavy => DriverFaultConfig::crash_heavy(),

            FaultScenario::PartialWritesWithCrashes => DriverFaultConfig {
                crash_probability: 0.1,
                min_ops_between_crashes: 10,
                ..DriverFaultConfig::default()
            },

            FaultScenario::Combined => DriverFaultConfig {
                crash_probability: 0.05,
                min_ops_between_crashes: 15,
                ..DriverFaultConfig::default()
            },

            FaultScenario::Chaos => DriverFaultConfig {
                crash_probability: 0.1,
                partial_write_probability: 0.05, // Additional driver-level partial writes
                min_ops_between_crashes: 5,
                max_crash_depth: 3,
                ..DriverFaultConfig::default()
            },
        }
    }

    /// Returns all available scenarios.
    pub fn all() -> &'static [FaultScenario] {
        &[
            FaultScenario::None,
            FaultScenario::PartialWrites,
            FaultScenario::DiskFull,
            FaultScenario::SlowDisk,
            FaultScenario::ClockSkew,
            FaultScenario::ProcessPause,
            FaultScenario::CrashHeavy,
            FaultScenario::PartialWritesWithCrashes,
            FaultScenario::DiskStress,
            FaultScenario::TimingChaos,
            FaultScenario::Combined,
            FaultScenario::Chaos,
        ]
    }

    /// Returns scenarios suitable for quick testing.
    pub fn quick_scenarios() -> &'static [FaultScenario] {
        &[
            FaultScenario::None,
            FaultScenario::PartialWrites,
            FaultScenario::CrashHeavy,
        ]
    }

    /// Returns scenarios suitable for thorough testing.
    pub fn thorough_scenarios() -> &'static [FaultScenario] {
        &[
            FaultScenario::None,
            FaultScenario::PartialWrites,
            FaultScenario::DiskFull,
            FaultScenario::SlowDisk,
            FaultScenario::CrashHeavy,
            FaultScenario::PartialWritesWithCrashes,
            FaultScenario::Combined,
        ]
    }
}

/// Configuration for running a fault scenario.
#[derive(Debug, Clone)]
pub struct ScenarioConfig {
    /// The fault scenario to run.
    pub scenario: FaultScenario,
    /// Maximum number of operations.
    pub max_operations: usize,
    /// Maximum number of crashes.
    pub max_crashes: usize,
    /// Whether to stop on first violation.
    pub stop_on_violation: bool,
}

impl Default for ScenarioConfig {
    fn default() -> Self {
        Self {
            scenario: FaultScenario::None,
            max_operations: 500,
            max_crashes: 10,
            stop_on_violation: true,
        }
    }
}

impl ScenarioConfig {
    /// Creates a quick scenario configuration.
    pub fn quick(scenario: FaultScenario) -> Self {
        Self {
            scenario,
            max_operations: 100,
            max_crashes: 3,
            stop_on_violation: true,
        }
    }

    /// Creates a thorough scenario configuration.
    pub fn thorough(scenario: FaultScenario) -> Self {
        Self {
            scenario,
            max_operations: 1000,
            max_crashes: 20,
            stop_on_violation: true,
        }
    }

    /// Creates a stress scenario configuration.
    pub fn stress(scenario: FaultScenario) -> Self {
        Self {
            scenario,
            max_operations: 5000,
            max_crashes: 50,
            stop_on_violation: false, // Run to completion for statistics
        }
    }
}

/// Result of a scenario run, extending StressResult with scenario info.
#[derive(Debug)]
pub struct ScenarioResult {
    /// The scenario that was run.
    pub scenario: FaultScenario,
    /// The underlying stress result.
    pub result: StressResult,
    /// Runtime fault events from the SimEnv.
    pub runtime_fault_events: Vec<runtime::FaultEvent>,
}

impl ScenarioResult {
    /// Returns whether the scenario passed.
    pub fn passed(&self) -> bool {
        self.result.passed
    }

    /// Returns a summary of the scenario result.
    pub fn summary(&self) -> String {
        format!(
            "[{}] {}\n  Runtime faults: {}",
            self.scenario.name(),
            self.result.summary(),
            self.runtime_fault_events.len()
        )
    }

    /// Returns fault statistics.
    pub fn fault_stats(&self) -> FaultStats {
        let mut stats = FaultStats::default();

        for event in &self.runtime_fault_events {
            match event {
                runtime::FaultEvent::PartialWrite { .. } => stats.partial_writes += 1,
                runtime::FaultEvent::DiskFull { .. } => stats.disk_full += 1,
                runtime::FaultEvent::SlowWrite { .. } => stats.slow_writes += 1,
                runtime::FaultEvent::ClockSkew { .. } => stats.clock_skews += 1,
                runtime::FaultEvent::ProcessPause { .. } => stats.process_pauses += 1,
            }
        }

        stats.driver_crashes = self.result.crashes;
        stats
    }
}

/// Statistics about faults that occurred during a scenario run.
#[derive(Debug, Clone, Default)]
pub struct FaultStats {
    /// Number of partial writes.
    pub partial_writes: u64,
    /// Number of disk full errors.
    pub disk_full: u64,
    /// Number of slow writes.
    pub slow_writes: u64,
    /// Number of clock skews.
    pub clock_skews: u64,
    /// Number of process pauses.
    pub process_pauses: u64,
    /// Number of driver-level crashes.
    pub driver_crashes: u64,
}

impl FaultStats {
    /// Returns the total number of runtime faults.
    pub fn total_runtime_faults(&self) -> u64 {
        self.partial_writes
            + self.disk_full
            + self.slow_writes
            + self.clock_skews
            + self.process_pauses
    }

    /// Returns the total number of all faults.
    pub fn total_faults(&self) -> u64 {
        self.total_runtime_faults() + self.driver_crashes
    }
}

/// Creates a SimEnv configured for the given scenario.
pub fn create_env_for_scenario(seed: u64, scenario: FaultScenario) -> SimEnv {
    let runtime_config = scenario.runtime_config();

    if runtime_config.all_disabled() {
        SimEnv::new(SimEnvConfig::with_seed(seed))
    } else {
        SimEnv::new_with_faults(SimEnvConfig::with_seed(seed), runtime_config)
    }
}

/// Creates a StressConfig for the given scenario.
pub fn create_stress_config(scenario_config: &ScenarioConfig) -> StressConfig {
    StressConfig {
        faults: scenario_config.scenario.driver_config(),
        max_operations: scenario_config.max_operations,
        max_crashes: scenario_config.max_crashes,
        stop_on_violation: scenario_config.stop_on_violation,
        ..Default::default()
    }
}

/// Runs a fault scenario with the given seed.
pub fn run_scenario(seed: u64, scenario: FaultScenario) -> ScenarioResult {
    run_scenario_with_config(
        seed,
        ScenarioConfig {
            scenario,
            ..Default::default()
        },
    )
}

/// Runs a fault scenario with full configuration.
pub fn run_scenario_with_config(seed: u64, config: ScenarioConfig) -> ScenarioResult {
    let env = create_env_for_scenario(seed, config.scenario);
    let stress_config = create_stress_config(&config);

    let mut driver = crate::driver::StressDriver::new(env.clone(), stress_config);
    let result = driver.run();

    let runtime_fault_events = env.fault_events();

    ScenarioResult {
        scenario: config.scenario,
        result,
        runtime_fault_events,
    }
}

/// Runs multiple scenarios with the same seed.
pub fn run_all_scenarios(seed: u64) -> Vec<ScenarioResult> {
    FaultScenario::all()
        .iter()
        .map(|&scenario| run_scenario(seed, scenario))
        .collect()
}

/// Runs quick scenarios for fast validation.
pub fn run_quick_validation(seed: u64) -> Vec<ScenarioResult> {
    FaultScenario::quick_scenarios()
        .iter()
        .map(|&scenario| run_scenario_with_config(seed, ScenarioConfig::quick(scenario)))
        .collect()
}

/// Runs thorough scenarios for comprehensive testing.
pub fn run_thorough_validation(seed: u64) -> Vec<ScenarioResult> {
    FaultScenario::thorough_scenarios()
        .iter()
        .map(|&scenario| run_scenario_with_config(seed, ScenarioConfig::thorough(scenario)))
        .collect()
}

/// Report summarizing multiple scenario runs.
#[derive(Debug)]
pub struct ScenarioReport {
    /// Seed used for all runs.
    pub seed: u64,
    /// Results for each scenario.
    pub results: Vec<ScenarioResult>,
}

impl ScenarioReport {
    /// Creates a new report from scenario results.
    pub fn new(seed: u64, results: Vec<ScenarioResult>) -> Self {
        Self { seed, results }
    }

    /// Returns whether all scenarios passed.
    pub fn all_passed(&self) -> bool {
        self.results.iter().all(|r| r.passed())
    }

    /// Returns the number of passing scenarios.
    pub fn passed_count(&self) -> usize {
        self.results.iter().filter(|r| r.passed()).count()
    }

    /// Returns the number of failing scenarios.
    pub fn failed_count(&self) -> usize {
        self.results.iter().filter(|r| !r.passed()).count()
    }

    /// Returns failing scenarios.
    pub fn failures(&self) -> impl Iterator<Item = &ScenarioResult> {
        self.results.iter().filter(|r| !r.passed())
    }

    /// Returns a summary of the report.
    pub fn summary(&self) -> String {
        let total = self.results.len();
        let passed = self.passed_count();
        let failed = self.failed_count();

        let mut summary = format!(
            "Scenario Report (seed=0x{:016X})\n  Total: {}, Passed: {}, Failed: {}\n",
            self.seed, total, passed, failed
        );

        for result in &self.results {
            let status = if result.passed() { "PASS" } else { "FAIL" };
            let stats = result.fault_stats();
            summary.push_str(&format!(
                "  [{}] {}: ops={}, crashes={}, runtime_faults={}\n",
                status,
                result.scenario.name(),
                result.result.operations,
                result.result.crashes,
                stats.total_runtime_faults()
            ));
        }

        summary
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scenario_names_are_unique() {
        let scenarios = FaultScenario::all();
        let names: std::collections::HashSet<_> = scenarios.iter().map(|s| s.name()).collect();
        assert_eq!(
            names.len(),
            scenarios.len(),
            "All scenario names must be unique"
        );
    }

    #[test]
    fn none_scenario_has_no_faults() {
        let runtime = FaultScenario::None.runtime_config();
        let driver = FaultScenario::None.driver_config();

        assert!(
            runtime.all_disabled(),
            "None scenario should have no runtime faults"
        );
        assert_eq!(
            driver.crash_probability, 0.0,
            "None scenario should have no crashes"
        );
    }

    #[test]
    fn partial_writes_scenario_enables_partial_writes() {
        let config = FaultScenario::PartialWrites.runtime_config();
        assert!(
            config.partial_write_prob > 0.0,
            "PartialWrites should enable partial writes"
        );
    }

    #[test]
    fn disk_full_scenario_enables_disk_full() {
        let config = FaultScenario::DiskFull.runtime_config();
        assert!(
            config.disk_full_threshold > 0,
            "DiskFull should enable disk full"
        );
    }

    #[test]
    fn slow_disk_scenario_enables_slow_writes() {
        let config = FaultScenario::SlowDisk.runtime_config();
        assert!(
            config.slow_write_prob > 0.0,
            "SlowDisk should enable slow writes"
        );
    }

    #[test]
    fn clock_skew_scenario_enables_clock_skew() {
        let config = FaultScenario::ClockSkew.runtime_config();
        assert!(
            config.clock_skew_prob > 0.0,
            "ClockSkew should enable clock skew"
        );
    }

    #[test]
    fn process_pause_scenario_enables_process_pause() {
        let config = FaultScenario::ProcessPause.runtime_config();
        assert!(
            config.process_pause_prob > 0.0,
            "ProcessPause should enable process pause"
        );
    }

    #[test]
    fn crash_heavy_scenario_enables_crashes() {
        let config = FaultScenario::CrashHeavy.driver_config();
        assert!(
            config.crash_probability > 0.0,
            "CrashHeavy should enable crashes"
        );
    }

    #[test]
    fn combined_scenario_enables_multiple_faults() {
        let runtime = FaultScenario::Combined.runtime_config();
        let driver = FaultScenario::Combined.driver_config();

        assert!(
            !runtime.all_disabled(),
            "Combined should have runtime faults"
        );
        assert!(
            driver.crash_probability > 0.0,
            "Combined should have crashes"
        );
    }

    #[test]
    fn chaos_scenario_most_aggressive() {
        let chaos = FaultScenario::Chaos.runtime_config();
        let combined = FaultScenario::Combined.runtime_config();

        // Chaos should generally have higher fault rates
        assert!(
            chaos.partial_write_prob >= combined.partial_write_prob,
            "Chaos should be at least as aggressive as Combined"
        );
    }

    #[test]
    fn create_env_for_scenario_uses_faults() {
        let env = create_env_for_scenario(42, FaultScenario::PartialWrites);
        let config = env.fault_config();
        assert!(config.partial_write_prob > 0.0);
    }

    #[test]
    fn create_env_for_none_has_no_faults() {
        let env = create_env_for_scenario(42, FaultScenario::None);
        let config = env.fault_config();
        assert!(config.all_disabled());
    }

    #[test]
    fn run_quick_scenario() {
        let result = run_scenario_with_config(42, ScenarioConfig::quick(FaultScenario::None));
        assert!(result.passed(), "Quick None scenario should pass");
    }

    #[test]
    fn scenario_result_summary() {
        let result = run_scenario_with_config(42, ScenarioConfig::quick(FaultScenario::None));
        let summary = result.summary();
        assert!(
            summary.contains("none"),
            "Summary should contain scenario name"
        );
    }

    #[test]
    fn fault_stats_counts_correctly() {
        let stats = FaultStats {
            partial_writes: 5,
            slow_writes: 3,
            driver_crashes: 2,
            ..Default::default()
        };

        assert_eq!(stats.total_runtime_faults(), 8);
        assert_eq!(stats.total_faults(), 10);
    }

    #[test]
    fn quick_scenarios_subset() {
        let quick = FaultScenario::quick_scenarios();
        let all = FaultScenario::all();

        for scenario in quick {
            assert!(
                all.contains(scenario),
                "Quick scenarios should be subset of all"
            );
        }
    }

    #[test]
    fn thorough_scenarios_subset() {
        let thorough = FaultScenario::thorough_scenarios();
        let all = FaultScenario::all();

        for scenario in thorough {
            assert!(
                all.contains(scenario),
                "Thorough scenarios should be subset of all"
            );
        }
    }

    // ========================================
    // Acceptance Tests (run in CI)
    // ========================================

    /// Quick validation runs all quick scenarios with a single seed.
    #[test]
    fn acceptance_quick_validation() {
        let results = run_quick_validation(0xACCEF7);
        for result in &results {
            assert!(
                result.passed(),
                "Quick validation failed for {}: {}",
                result.scenario.name(),
                result.result.failure.as_deref().unwrap_or("unknown")
            );
        }
    }

    /// Runs the None scenario with 10 different seeds.
    #[test]
    fn acceptance_none_10_seeds() {
        for seed in 0..10 {
            let result = run_scenario_with_config(seed, ScenarioConfig::quick(FaultScenario::None));
            assert!(
                result.passed(),
                "None scenario failed for seed {}: {}",
                seed,
                result.result.failure.as_deref().unwrap_or("unknown")
            );
        }
    }

    /// Runs the PartialWrites scenario with 5 different seeds.
    #[test]
    fn acceptance_partial_writes_5_seeds() {
        for seed in 100..105 {
            let result =
                run_scenario_with_config(seed, ScenarioConfig::quick(FaultScenario::PartialWrites));
            assert!(
                result.passed(),
                "PartialWrites scenario failed for seed {}: {}",
                seed,
                result.result.failure.as_deref().unwrap_or("unknown")
            );
        }
    }

    /// Runs the CrashHeavy scenario with 5 different seeds.
    #[test]
    fn acceptance_crash_heavy_5_seeds() {
        for seed in 200..205 {
            let result =
                run_scenario_with_config(seed, ScenarioConfig::quick(FaultScenario::CrashHeavy));
            assert!(
                result.passed(),
                "CrashHeavy scenario failed for seed {}: {}",
                seed,
                result.result.failure.as_deref().unwrap_or("unknown")
            );
        }
    }

    /// Runs the Combined scenario with 3 different seeds.
    #[test]
    fn acceptance_combined_3_seeds() {
        for seed in 300..303 {
            let result =
                run_scenario_with_config(seed, ScenarioConfig::quick(FaultScenario::Combined));
            assert!(
                result.passed(),
                "Combined scenario failed for seed {}: {}",
                seed,
                result.result.failure.as_deref().unwrap_or("unknown")
            );
        }
    }

    /// Thorough validation - ignored by default, run in CI with --ignored
    #[test]
    #[ignore]
    fn acceptance_thorough_validation_10_seeds() {
        for seed in 0..10 {
            let results = run_thorough_validation(seed);
            let report = ScenarioReport::new(seed, results);
            assert!(
                report.all_passed(),
                "Thorough validation failed for seed {}:\n{}",
                seed,
                report.summary()
            );
        }
    }

    /// Full scenario sweep - ignored by default, run in CI with --ignored
    #[test]
    #[ignore]
    fn acceptance_all_scenarios_5_seeds() {
        for seed in 0..5 {
            let results = run_all_scenarios(seed);
            let report = ScenarioReport::new(seed, results);
            assert!(
                report.all_passed(),
                "Full scenario sweep failed for seed {}:\n{}",
                seed,
                report.summary()
            );
        }
    }

    /// Determinism test: same seed produces same faults
    #[test]
    fn acceptance_fault_determinism() {
        let seed = 0xDE7E5714;
        let scenario = FaultScenario::Combined;

        let result1 = run_scenario_with_config(seed, ScenarioConfig::quick(scenario));
        let result2 = run_scenario_with_config(seed, ScenarioConfig::quick(scenario));

        assert_eq!(
            result1.result.operations, result2.result.operations,
            "Operations should be identical"
        );
        assert_eq!(
            result1.result.crashes, result2.result.crashes,
            "Crashes should be identical"
        );
        assert_eq!(
            result1.runtime_fault_events.len(),
            result2.runtime_fault_events.len(),
            "Fault event count should be identical"
        );

        // Verify fault events are identical
        for (i, (e1, e2)) in result1
            .runtime_fault_events
            .iter()
            .zip(result2.runtime_fault_events.iter())
            .enumerate()
        {
            assert_eq!(e1, e2, "Fault event {} differs: {:?} vs {:?}", i, e1, e2);
        }
    }
}
