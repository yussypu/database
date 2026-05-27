//! Quick stress test demonstration.

use sim::{run_stress_test, seed_range, FaultConfig, StressConfig};

fn main() {
    println!("=== CrackedDB Stress Test Demo ===\n");

    let config = StressConfig {
        max_operations: 200,
        max_crashes: 3,
        faults: FaultConfig {
            crash_probability: 0.08,
            min_ops_between_crashes: 30,
            ..Default::default()
        },
        ..Default::default()
    };

    // Run several seeds
    for seed in seed_range(0xDEADBEEF, 5) {
        let result = run_stress_test(seed, config.clone());
        println!("{}", result.summary());
    }

    println!("\n=== Determinism Verification ===");
    let r1 = run_stress_test(0xCAFE, config.clone());
    let r2 = run_stress_test(0xCAFE, config);

    assert_eq!(r1.operations, r2.operations, "Operations must match");
    assert_eq!(r1.crashes, r2.crashes, "Crashes must match");
    assert_eq!(r1.passed, r2.passed, "Pass/fail must match");

    println!("✓ Seed 0xCAFE produces identical results across runs!");
    println!(
        "  ops={}, crashes={}, passed={}",
        r1.operations, r1.crashes, r1.passed
    );
}
