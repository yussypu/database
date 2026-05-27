//! CLI binary for crackeddb.
//!
//! Provides simulation testing commands: replay, find-and-shrink, demo.
//! Also provides an interactive shell for database operations.

mod shell;

use clap::{Parser, Subcommand};
use sim::failure::{FailureKind, FailureReport, SyntheticBug};
use sim::fault_scenarios::FaultScenario;
use sim::shrink::{DeltaDebugConfig, DeltaDebugResult, TestableShrinker};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(name = "crackeddb")]
#[command(about = "embedded OLTP database with deterministic simulation testing")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// simulation testing commands
    Sim {
        #[command(subcommand)]
        sim_command: SimCommand,
    },
    /// interactive database shell
    Shell {
        /// path to the database directory
        #[arg(long)]
        path: PathBuf,
    },
}

#[derive(Subcommand)]
enum SimCommand {
    /// replay a specific seed
    Replay {
        /// seed in hex (0xDEADBEEF) or decimal
        #[arg(long, value_parser = parse_seed)]
        seed: u64,
        /// fault scenario name
        #[arg(long, default_value = "Combined")]
        scenario: String,
        /// number of operations in workload
        #[arg(long, default_value_t = 50)]
        workload_len: usize,
    },
    /// search for failures and shrink them
    FindAndShrink {
        /// number of seeds to try
        #[arg(long, default_value_t = 100)]
        seeds: usize,
        /// fault scenario name
        #[arg(long, default_value = "Combined")]
        scenario: String,
        /// number of operations in workload
        #[arg(long, default_value_t = 50)]
        workload_len: usize,
        /// first seed to try
        #[arg(long, default_value_t = 0xDEADBEEF)]
        start_seed: u64,
    },
    /// run demo with synthetic bug injection
    Demo,
}

fn parse_seed(s: &str) -> Result<u64, String> {
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16).map_err(|e| format!("invalid hex seed: {}", e))
    } else {
        s.parse::<u64>().map_err(|e| format!("invalid seed: {}", e))
    }
}

fn parse_scenario(name: &str) -> Result<FaultScenario, String> {
    match name.to_lowercase().as_str() {
        "none" => Ok(FaultScenario::None),
        "partialwrites" | "partial_writes" => Ok(FaultScenario::PartialWrites),
        "diskfull" | "disk_full" => Ok(FaultScenario::DiskFull),
        "slowdisk" | "slow_disk" => Ok(FaultScenario::SlowDisk),
        "clockskew" | "clock_skew" => Ok(FaultScenario::ClockSkew),
        "processpause" | "process_pause" => Ok(FaultScenario::ProcessPause),
        "combined" => Ok(FaultScenario::Combined),
        "chaos" => Ok(FaultScenario::Chaos),
        _ => Err(format!("unknown scenario: {}", name)),
    }
}

fn scenario_name(scenario: FaultScenario) -> &'static str {
    match scenario {
        FaultScenario::None => "None",
        FaultScenario::PartialWrites => "PartialWrites",
        FaultScenario::DiskFull => "DiskFull",
        FaultScenario::SlowDisk => "SlowDisk",
        FaultScenario::ClockSkew => "ClockSkew",
        FaultScenario::ProcessPause => "ProcessPause",
        FaultScenario::CrashHeavy => "CrashHeavy",
        FaultScenario::PartialWritesWithCrashes => "PartialWritesWithCrashes",
        FaultScenario::DiskStress => "DiskStress",
        FaultScenario::TimingChaos => "TimingChaos",
        FaultScenario::Combined => "Combined",
        FaultScenario::Chaos => "Chaos",
    }
}

fn short_kind(kind: &FailureKind) -> String {
    match kind {
        FailureKind::SerializabilityCycle { txn_ids } => {
            let ids: Vec<String> = txn_ids.iter().map(|id| id.to_string()).collect();
            format!("SerializabilityCycle(txns=[{}])", ids.join(","))
        }
        FailureKind::LostWrite { key, expected } => {
            let key_str = String::from_utf8_lossy(key);
            let exp_str = String::from_utf8_lossy(expected);
            format!("LostWrite(key={}, expected={})", key_str, exp_str)
        }
        FailureKind::Custom(msg) => format!("Custom({})", msg),
        FailureKind::Unrecoverable(msg) => format!("Unrecoverable({})", msg),
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    match cli.command {
        Command::Shell { path } => match shell::run_shell_main(&path) {
            Ok(()) => ExitCode::from(0),
            Err(e) => {
                eprintln!("error: {}", e);
                ExitCode::from(1)
            }
        },
        Command::Sim { sim_command } => match sim_command {
            SimCommand::Replay {
                seed,
                scenario,
                workload_len,
            } => match handle_replay(seed, &scenario, workload_len) {
                Ok(passed) => {
                    if passed {
                        ExitCode::from(0)
                    } else {
                        ExitCode::from(1)
                    }
                }
                Err(e) => {
                    eprintln!("error: {}", e);
                    ExitCode::from(2)
                }
            },
            SimCommand::FindAndShrink {
                seeds,
                scenario,
                workload_len,
                start_seed,
            } => match handle_find_and_shrink(seeds, &scenario, workload_len, start_seed) {
                Ok(found) => {
                    if found {
                        ExitCode::from(1)
                    } else {
                        ExitCode::from(0)
                    }
                }
                Err(e) => {
                    eprintln!("error: {}", e);
                    ExitCode::from(2)
                }
            },
            SimCommand::Demo => match handle_demo() {
                Ok(()) => ExitCode::from(0),
                Err(e) => {
                    eprintln!("error: {}", e);
                    ExitCode::from(2)
                }
            },
        },
    }
}

fn handle_replay(seed: u64, scenario_name: &str, workload_len: usize) -> Result<bool, String> {
    let scenario = parse_scenario(scenario_name)?;

    println!(
        "replay seed=0x{:016X} scenario={} workload_len={}",
        seed, scenario_name, workload_len
    );
    println!();

    let start = std::time::Instant::now();
    let result = sim::failure::replay(seed, &scenario, workload_len);
    let elapsed = start.elapsed();

    match result {
        Ok(()) => {
            println!("pass");
            println!("elapsed {:?}", elapsed);
            Ok(true)
        }
        Err(report) => {
            print_failure_report(&report);
            println!("elapsed {:?}", elapsed);
            Ok(false)
        }
    }
}

fn print_failure_report(report: &FailureReport) {
    println!("fail {}", short_kind(&report.failure_kind));
    println!();
    println!("workload_len {}", report.workload_len);
}

fn handle_find_and_shrink(
    max_seeds: usize,
    scenario_str: &str,
    workload_len: usize,
    start_seed: u64,
) -> Result<bool, String> {
    let scenario = parse_scenario(scenario_str)?;

    println!(
        "searching scenario={} workload_len={} max_seeds={}",
        scenario_str, workload_len, max_seeds
    );

    let overall_start = std::time::Instant::now();

    for i in 0..max_seeds {
        let seed = start_seed.wrapping_add(i as u64);
        let result = sim::failure::replay(seed, &scenario, workload_len);

        if let Err(report) = result {
            let elapsed = overall_start.elapsed();
            println!(
                "failure found at seed=0x{:016X} after {} seeds ({:.0?})",
                seed,
                i + 1,
                elapsed
            );
            println!();

            // Print original failure
            println!("original failure");
            println!("  kind {}", short_kind(&report.failure_kind));
            println!("  scenario {}", scenario_str);
            println!("  workload_len {}", report.workload_len);
            println!();

            // Run shrinker
            let shrink_result = shrink_failure(&report);

            if let Some(result) = shrink_result {
                print_shrink_result(&result, seed);
            } else {
                println!("shrinking failed to converge");
            }

            return Ok(true);
        }
    }

    let elapsed = overall_start.elapsed();
    println!("no failure found in {} seeds ({:.0?})", max_seeds, elapsed);
    Ok(false)
}

fn shrink_failure(report: &FailureReport) -> Option<DeltaDebugResult> {
    let mut shrinker = sim::shrink::DeltaDebugShrinker::new(DeltaDebugConfig::default());
    shrinker.shrink(report)
}

fn effective_scenario_name(cfg: &sim::shrink::ShrunkFaultConfig) -> &'static str {
    // If all fault probabilities/thresholds are 0, the effective scenario is None
    if cfg.enabled_fault_count() == 0 {
        "None"
    } else {
        scenario_name(cfg.scenario)
    }
}

fn print_shrink_result(result: &DeltaDebugResult, seed: u64) {
    println!("shrinking");
    for step in &result.stats.trace {
        if step.accepted {
            println!(
                "  axis={} before={} after={} (accepted)",
                step.axis, step.before, step.after
            );
        }
    }
    println!();

    let cfg = &result.fault_config;
    let eff_scenario = effective_scenario_name(cfg);
    println!("minimal reproducer");
    println!("  seed 0x{:016X}", seed);
    println!("  scenario {}", eff_scenario);
    println!("  workload_len {}", cfg.workload_len);
    println!("  replays {}", result.stats.total_replays);
    println!();

    println!("reproduce with");
    println!(
        "  crackeddb sim replay --seed=0x{:X} --scenario={} --workload-len={}",
        seed, eff_scenario, cfg.workload_len
    );
}

fn handle_demo() -> Result<(), String> {
    // Demo uses synthetic bug injection from Stage 2
    let seed = 0xDEADBEEF_u64;
    let fail_at_op = 15;
    let initial_workload = 50;
    let scenario = FaultScenario::Chaos;

    let bug = SyntheticBug::serializability_cycle(fail_at_op);

    println!(
        "searching scenario=Chaos workload_len={} max_seeds=1",
        initial_workload
    );

    let start = std::time::Instant::now();

    // Create the failure report for the synthetic bug
    let report = FailureReport::new(seed, scenario, initial_workload, bug.failure_kind.clone());

    let elapsed_find = start.elapsed();
    println!(
        "failure found at seed=0x{:016X} after 1 seeds ({:.0?})",
        seed, elapsed_find
    );
    println!();

    println!("original failure");
    println!("  kind {}", short_kind(&bug.failure_kind));
    println!("  scenario Chaos");
    println!("  workload_len {}", initial_workload);
    println!();

    // Run shrinker with synthetic bug
    let mut shrinker = TestableShrinker::for_workload_bug(
        DeltaDebugConfig::default(),
        fail_at_op,
        bug.failure_kind,
    );

    let shrink_result = shrinker.shrink(&report);

    if let Some(result) = shrink_result {
        print_shrink_result(&result, seed);
    } else {
        return Err("shrinking failed to converge".to_string());
    }

    println!();
    println!(
        "synthetic bug injected at op {} for this demo. real bugs caught by the",
        fail_at_op
    );
    println!("simulator are documented in DECISIONS.md (ADR-007, ADR-023, ADR-026).");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::time::Instant;

    #[test]
    fn cli_parses_replay_with_hex_seed() {
        let result = parse_seed("0xDEADBEEF");
        assert_eq!(result, Ok(0xDEADBEEF));
    }

    #[test]
    fn cli_parses_replay_with_decimal_seed() {
        let result = parse_seed("12345");
        assert_eq!(result, Ok(12345));
    }

    #[test]
    fn cli_rejects_unknown_scenario() {
        let result = parse_scenario("NotARealScenario");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("unknown scenario"));
    }

    #[test]
    fn cli_parses_known_scenarios() {
        assert!(parse_scenario("None").is_ok());
        assert!(parse_scenario("Combined").is_ok());
        assert!(parse_scenario("Chaos").is_ok());
        assert!(parse_scenario("PartialWrites").is_ok());
        assert!(parse_scenario("partial_writes").is_ok());
    }

    fn get_binary_path() -> std::path::PathBuf {
        // Build first to ensure binary exists
        let status = Command::new("cargo")
            .args(["build", "--package", "cli"])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .status()
            .expect("failed to build");
        assert!(status.success());

        // Get workspace root (two dirs up from cli crate)
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let workspace_root = manifest_dir.parent().unwrap().parent().unwrap();
        workspace_root.join("target/debug/crackeddb")
    }

    fn get_release_binary_path() -> std::path::PathBuf {
        let status = Command::new("cargo")
            .args(["build", "--release", "--package", "cli"])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .status()
            .expect("failed to build");
        assert!(status.success());

        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let workspace_root = manifest_dir.parent().unwrap().parent().unwrap();
        workspace_root.join("target/release/crackeddb")
    }

    #[test]
    fn replay_passing_seed_returns_zero() {
        let bin = get_binary_path();
        let output = Command::new(bin)
            .args([
                "sim",
                "replay",
                "--seed=0x42",
                "--scenario=None",
                "--workload-len=10",
            ])
            .output()
            .expect("failed to run");
        assert!(
            output.status.success(),
            "exit code should be 0 for passing seed"
        );
    }

    #[test]
    fn replay_output_contains_seed() {
        let bin = get_binary_path();
        let output = Command::new(bin)
            .args([
                "sim",
                "replay",
                "--seed=0xDEADBEEF",
                "--scenario=None",
                "--workload-len=10",
            ])
            .output()
            .expect("failed to run");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("0x00000000DEADBEEF"),
            "output should contain seed in 0x format: {}",
            stdout
        );
    }

    #[test]
    fn replay_output_has_no_em_dash() {
        let bin = get_binary_path();
        let output = Command::new(bin)
            .args([
                "sim",
                "replay",
                "--seed=0x42",
                "--scenario=None",
                "--workload-len=10",
            ])
            .output()
            .expect("failed to run");

        // Em dash in UTF-8 is bytes [0xe2, 0x80, 0x94]
        let em_dash: &[u8] = &[0xe2, 0x80, 0x94];
        let has_em_dash = output.stdout.windows(3).any(|window| window == em_dash);
        assert!(!has_em_dash, "output should not contain em dash");
    }

    #[test]
    fn demo_exits_zero() {
        let bin = get_binary_path();
        let output = Command::new(bin)
            .args(["sim", "demo"])
            .output()
            .expect("failed to run");
        assert!(
            output.status.success(),
            "demo should exit 0: {:?}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn demo_output_has_no_em_dash() {
        let bin = get_binary_path();
        let output = Command::new(bin)
            .args(["sim", "demo"])
            .output()
            .expect("failed to run");

        let em_dash: &[u8] = &[0xe2, 0x80, 0x94];
        let has_em_dash = output.stdout.windows(3).any(|window| window == em_dash);
        assert!(
            !has_em_dash,
            "output should not contain em dash: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }

    #[test]
    fn demo_output_contains_reproduce_line() {
        let bin = get_binary_path();
        let output = Command::new(bin)
            .args(["sim", "demo"])
            .output()
            .expect("failed to run");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("reproduce with"),
            "output should contain reproduce with: {}",
            stdout
        );
    }

    #[test]
    fn demo_completes_in_under_5_seconds() {
        let bin = get_release_binary_path();
        let start = Instant::now();
        let output = Command::new(bin)
            .args(["sim", "demo"])
            .output()
            .expect("failed to run");
        let elapsed = start.elapsed();

        assert!(output.status.success(), "demo should exit 0");
        assert!(
            elapsed.as_secs() < 5,
            "demo took {:?}, should be under 5 seconds",
            elapsed
        );
    }

    #[test]
    fn find_and_shrink_with_synthetic_bug() {
        // This test uses the demo path with synthetic bug
        let bin = get_binary_path();
        let output = Command::new(bin)
            .args(["sim", "demo"])
            .output()
            .expect("failed to run");

        let stdout = String::from_utf8_lossy(&output.stdout);

        // Assert seed is present
        assert!(
            stdout.contains("0x00000000DEADBEEF"),
            "output should contain seed: {}",
            stdout
        );

        // Assert workload was shrunk (should be <= 20)
        assert!(
            stdout.contains("workload_len 15")
                || stdout.contains("workload_len 16")
                || stdout.contains("workload_len 17")
                || stdout.contains("workload_len 18")
                || stdout.contains("workload_len 19")
                || stdout.contains("workload_len 20"),
            "workload should be shrunk to <= 20: {}",
            stdout
        );
    }
}
