//! Deterministic simulation harness for cracked-db.
//!
//! This crate is the soul of the project. It provides:
//!
//! - **Model**: Reference database for oracle-based testing
//! - **Workload**: Deterministic workload generation from seeds
//! - **Driver**: Main stress test execution engine
//! - **Fault Injector**: Crash, partial write, disk-full, slow disk, etc.
//! - **Invariant Checker**: Verifies correctness properties
//! - **Shrinker**: Minimizes failing test cases
//!
//! # The Killer Demo
//!
//! ```bash
//! cracked-db sim replay --seed=0xDEADBEEF
//! ```
//!
//! This command deterministically reproduces any bug the simulator found,
//! in under a second, on any machine.
//!
//! # Example Usage
//!
//! ```ignore
//! use sim::{run_stress_test, StressConfig};
//!
//! // Run a stress test with a specific seed
//! let result = run_stress_test(0xDEADBEEF, StressConfig::default());
//!
//! if !result.passed {
//!     println!("FAILURE: {}", result.summary());
//!     println!("Replay with: cracked-db sim replay --seed=0x{:016X}", result.seed);
//! }
//! ```
//!
//! # Reference
//!
//! Will Wilson, "Testing Distributed Systems w/ Deterministic Simulation"
//! (Strange Loop 2014)

pub mod driver;
pub mod failure;
pub mod fault;
pub mod fault_scenarios;
pub mod invariant;
pub mod model;
pub mod shrink;
pub mod txn_stress;
pub mod workload;

// Re-export main types for convenience
pub use driver::{
    replay_seed, run_stress_test, run_stress_tests, seed_range, StressConfig, StressDriver,
    StressResult,
};
pub use failure::{
    find_and_shrink_failures, find_failures, replay, replay_with_config, run_with_synthetic_bug,
    shrink_real_failure, shrink_real_failure_quick, shrink_real_failure_thorough,
    verify_shrinker_with_synthetic_bug, FailureKind, FailureReport, FailureSearchResult,
    FoundFailure, SyntheticBug,
};
pub use fault::{
    CombinedFaultInjector, Fault, FaultConfig, FaultInjector, FaultSchedule, FaultStats,
};
pub use fault_scenarios::{
    create_env_for_scenario, run_all_scenarios, run_quick_validation, run_scenario,
    run_scenario_with_config, run_thorough_validation, FaultScenario,
    FaultStats as ScenarioFaultStats, ScenarioConfig, ScenarioReport, ScenarioResult,
};
pub use invariant::{
    check_durability, check_read_your_writes, check_scan_correctness, check_scan_order,
    full_verification, spot_check, verify_recovery, InvariantChecker, InvariantConfig,
    InvariantReport, InvariantResult, InvariantViolation,
};
pub use model::{compare_model, ComparisonResult, Mismatch, Model, ModelOp, ModelSnapshot, OpKind};
pub use shrink::{
    find_minimum_prefix, quick_shrink, remove_operations, seed_candidates, shrink_failure,
    thorough_shrink, DeltaDebugConfig, DeltaDebugResult, DeltaDebugShrinker, DeltaDebugStats,
    MinimalRepro, ShrinkConfig, ShrinkResult, ShrinkStats, ShrinkStep, Shrinker, ShrunkFaultConfig,
    TestableShrinker,
};
pub use txn_stress::{
    run_txn_stress_test, run_txn_stress_tests, run_txn_stress_via_kv,
    run_txn_stress_with_crashes_via_kv, run_write_skew_tests, test_explicit_write_skew_via_kv,
    TxnStressConfig, TxnStressResult, TxnStressWithCrashesResult,
};
pub use workload::{
    Operation, OperationBatch, OperationLog, RecordedOp, WorkloadConfig, WorkloadGenerator,
};

/// Re-export runtime for convenience.
pub use runtime;
