//! crackeddb benchmark harness.
//!
//! Runs YCSB workloads A-F and TPC-C-shape OLTP against crackeddb, rocksdb,
//! lmdb (lmdb-rkv), and sled. Outputs structured metrics as JSON or JSONL.
//!
//! # Concurrent Execution
//!
//! By default, benchmarks run with 16 workers, 60s warmup, and 3 runs.
//! Use `--workers`, `--warmup-secs`, and `--runs` to customize.
//!
//! # Output Formats
//!
//! - Default: Pretty JSON to stdout (summary only)
//! - `--output <path>`: JSONL file with all run results and final summary

use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

pub mod backends;
pub mod metrics;
pub mod runner;
pub mod workloads;

use backends::{CrackeddbBackend, LmdbBackend, RocksdbBackend, SledBackend};
use metrics::{JsonlWriter, MultiRunSummary};
use runner::{multi_run_tpcc_with_cold_start, multi_run_ycsb_with_cold_start, ConcurrentConfig};
use workloads::tpcc::TpccConfig;
use workloads::ycsb::{YcsbConfig, YcsbWorkload};

/// crackeddb benchmark harness
#[derive(Parser)]
#[command(name = "bench")]
#[command(about = "Benchmark harness for crackeddb and competitors")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run a YCSB workload (concurrent, multi-run)
    Ycsb {
        /// Backend to test
        #[arg(short, long)]
        backend: BackendChoice,

        /// YCSB workload (a, b, c, d, e, f)
        #[arg(short, long)]
        workload: String,

        /// Number of records to load
        #[arg(long, default_value = "10000")]
        record_count: u64,

        /// Key size in bytes
        #[arg(long, default_value = "16")]
        key_size: usize,

        /// Value size in bytes
        #[arg(long, default_value = "100")]
        value_size: usize,

        /// Random seed
        #[arg(long, default_value = "3735928559")]
        seed: u64,

        /// Database path
        #[arg(short, long)]
        path: PathBuf,

        /// Number of concurrent workers
        #[arg(long, default_value = "16")]
        workers: u32,

        /// Number of benchmark runs
        #[arg(long, default_value = "3")]
        runs: u32,

        /// Warmup duration in seconds
        #[arg(long, default_value = "60")]
        warmup_secs: f64,

        /// Measurement duration in seconds per run
        #[arg(long, default_value = "120")]
        measurement_secs: f64,

        /// Output file for JSONL results (append mode)
        #[arg(short, long)]
        output: Option<PathBuf>,

        /// Measure cold-start time (database open + first read)
        #[arg(long)]
        measure_cold_start: bool,
    },

    /// Run TPC-C new order workload (concurrent, multi-run)
    Tpcc {
        /// Backend to test
        #[arg(short, long)]
        backend: BackendChoice,

        /// Number of warehouses
        #[arg(long, default_value = "1")]
        warehouses: u32,

        /// Number of items
        #[arg(long, default_value = "100")]
        items: u32,

        /// Random seed
        #[arg(long, default_value = "3735928559")]
        seed: u64,

        /// Database path
        #[arg(short, long)]
        path: PathBuf,

        /// Number of concurrent workers
        #[arg(long, default_value = "16")]
        workers: u32,

        /// Number of benchmark runs
        #[arg(long, default_value = "3")]
        runs: u32,

        /// Warmup duration in seconds
        #[arg(long, default_value = "60")]
        warmup_secs: f64,

        /// Measurement duration in seconds per run
        #[arg(long, default_value = "120")]
        measurement_secs: f64,

        /// Output file for JSONL results (append mode)
        #[arg(short, long)]
        output: Option<PathBuf>,

        /// Measure cold-start time (database open + first read)
        #[arg(long)]
        measure_cold_start: bool,
    },

    /// Run all workloads as smoke test (quick, single-run)
    Smoke {
        /// Output directory for databases
        #[arg(short, long, default_value = "/tmp/bench_smoke")]
        out_dir: PathBuf,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum BackendChoice {
    Crackeddb,
    Rocksdb,
    Lmdb,
    Sled,
}

impl std::fmt::Display for BackendChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BackendChoice::Crackeddb => write!(f, "crackeddb"),
            BackendChoice::Rocksdb => write!(f, "rocksdb"),
            BackendChoice::Lmdb => write!(f, "lmdb"),
            BackendChoice::Sled => write!(f, "sled"),
        }
    }
}

/// Result type for CLI operations.
enum CliResult {
    Summary(MultiRunSummary),
    LegacyMetrics(metrics::BenchMetrics),
}

fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        Commands::Ycsb {
            backend,
            workload,
            record_count,
            key_size,
            value_size,
            seed,
            path,
            workers,
            runs,
            warmup_secs,
            measurement_secs,
            output,
            measure_cold_start,
        } => {
            let workload: YcsbWorkload = workload
                .parse()
                .expect("invalid workload: use a, b, c, d, e, or f");

            let ycsb_config = YcsbConfig {
                workload,
                record_count,
                operation_count: 0, // Not used in concurrent mode
                key_size,
                value_size,
                seed,
                scan_length: 100,
            };

            let concurrent_config = ConcurrentConfig {
                workers,
                warmup_secs,
                measurement_secs,
                seed,
                run_id: 0,
            };

            run_concurrent_ycsb(
                backend,
                &path,
                ycsb_config,
                concurrent_config,
                runs,
                output,
                measure_cold_start,
            )
        }
        Commands::Tpcc {
            backend,
            warehouses,
            items,
            seed,
            path,
            workers,
            runs,
            warmup_secs,
            measurement_secs,
            output,
            measure_cold_start,
        } => {
            let tpcc_config = TpccConfig {
                warehouses,
                items,
                orders_per_run: 0, // Not used in concurrent mode
                seed,
            };

            let concurrent_config = ConcurrentConfig {
                workers,
                warmup_secs,
                measurement_secs,
                seed,
                run_id: 0,
            };

            run_concurrent_tpcc(
                backend,
                &path,
                tpcc_config,
                concurrent_config,
                runs,
                output,
                measure_cold_start,
            )
        }
        Commands::Smoke { out_dir } => run_smoke_tests(&out_dir).map(CliResult::LegacyMetrics),
    };

    match result {
        Ok(CliResult::Summary(summary)) => {
            println!("{}", serde_json::to_string_pretty(&summary).unwrap());
        }
        Ok(CliResult::LegacyMetrics(metrics)) => {
            println!("{}", serde_json::to_string_pretty(&metrics).unwrap());
        }
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    }
}

fn run_concurrent_ycsb(
    backend: BackendChoice,
    path: &PathBuf,
    ycsb_config: YcsbConfig,
    concurrent_config: ConcurrentConfig,
    num_runs: u32,
    output_path: Option<PathBuf>,
    measure_cold_start: bool,
) -> Result<CliResult, backends::Error> {
    // Open JSONL output if specified
    let mut writer = output_path
        .as_ref()
        .map(|p| JsonlWriter::open(p))
        .transpose()
        .map_err(|e| backends::Error::Other(e.to_string()))?;

    // Print run configuration
    eprintln!(
        "Running YCSB {} on {} with {} workers, {} runs",
        ycsb_config.workload, backend, concurrent_config.workers, num_runs
    );
    eprintln!(
        "  warmup: {}s, measurement: {}s per run",
        concurrent_config.warmup_secs, concurrent_config.measurement_secs
    );
    if measure_cold_start {
        eprintln!("  cold-start measurement: enabled");
    }
    eprintln!();

    let (_results, summary) = match backend {
        BackendChoice::Crackeddb => multi_run_ycsb_with_cold_start::<CrackeddbBackend>(
            &backend.to_string(),
            path,
            ycsb_config,
            concurrent_config,
            num_runs,
            writer.as_mut(),
            measure_cold_start,
        )?,
        BackendChoice::Rocksdb => multi_run_ycsb_with_cold_start::<RocksdbBackend>(
            &backend.to_string(),
            path,
            ycsb_config,
            concurrent_config,
            num_runs,
            writer.as_mut(),
            measure_cold_start,
        )?,
        BackendChoice::Lmdb => multi_run_ycsb_with_cold_start::<LmdbBackend>(
            &backend.to_string(),
            path,
            ycsb_config,
            concurrent_config,
            num_runs,
            writer.as_mut(),
            measure_cold_start,
        )?,
        BackendChoice::Sled => multi_run_ycsb_with_cold_start::<SledBackend>(
            &backend.to_string(),
            path,
            ycsb_config,
            concurrent_config,
            num_runs,
            writer.as_mut(),
            measure_cold_start,
        )?,
    };

    if let Some(p) = output_path {
        eprintln!("Results written to: {}", p.display());
    }

    Ok(CliResult::Summary(summary))
}

fn run_concurrent_tpcc(
    backend: BackendChoice,
    path: &PathBuf,
    tpcc_config: TpccConfig,
    concurrent_config: ConcurrentConfig,
    num_runs: u32,
    output_path: Option<PathBuf>,
    measure_cold_start: bool,
) -> Result<CliResult, backends::Error> {
    // Open JSONL output if specified
    let mut writer = output_path
        .as_ref()
        .map(|p| JsonlWriter::open(p))
        .transpose()
        .map_err(|e| backends::Error::Other(e.to_string()))?;

    // Print run configuration
    eprintln!(
        "Running TPC-C on {} with {} workers, {} runs",
        backend, concurrent_config.workers, num_runs
    );
    eprintln!(
        "  warmup: {}s, measurement: {}s per run",
        concurrent_config.warmup_secs, concurrent_config.measurement_secs
    );
    if measure_cold_start {
        eprintln!("  cold-start measurement: enabled");
    }
    eprintln!();

    let (_results, summary) = match backend {
        BackendChoice::Crackeddb => multi_run_tpcc_with_cold_start::<CrackeddbBackend>(
            &backend.to_string(),
            path,
            tpcc_config,
            concurrent_config,
            num_runs,
            writer.as_mut(),
            measure_cold_start,
        )?,
        BackendChoice::Rocksdb => multi_run_tpcc_with_cold_start::<RocksdbBackend>(
            &backend.to_string(),
            path,
            tpcc_config,
            concurrent_config,
            num_runs,
            writer.as_mut(),
            measure_cold_start,
        )?,
        BackendChoice::Lmdb => multi_run_tpcc_with_cold_start::<LmdbBackend>(
            &backend.to_string(),
            path,
            tpcc_config,
            concurrent_config,
            num_runs,
            writer.as_mut(),
            measure_cold_start,
        )?,
        BackendChoice::Sled => multi_run_tpcc_with_cold_start::<SledBackend>(
            &backend.to_string(),
            path,
            tpcc_config,
            concurrent_config,
            num_runs,
            writer.as_mut(),
            measure_cold_start,
        )?,
    };

    if let Some(p) = output_path {
        eprintln!("Results written to: {}", p.display());
    }

    Ok(CliResult::Summary(summary))
}

fn run_smoke_tests(out_dir: &PathBuf) -> Result<metrics::BenchMetrics, backends::Error> {
    use runner::{run_tpcc, run_ycsb};

    let backends = [
        BackendChoice::Crackeddb,
        BackendChoice::Rocksdb,
        BackendChoice::Lmdb,
        BackendChoice::Sled,
    ];

    let ycsb_workloads = ["a", "b", "c", "d", "e", "f"];

    let mut all_results = Vec::new();
    let mut failed = 0;
    let mut passed = 0;

    // Clean up output directory
    let _ = std::fs::remove_dir_all(out_dir);
    std::fs::create_dir_all(out_dir)?;

    // YCSB workloads (single-threaded, quick)
    for backend in &backends {
        for workload in &ycsb_workloads {
            let name = format!("{}_{}", backend, workload);
            let path = out_dir.join(&name);

            // Clean up any existing database
            let _ = std::fs::remove_dir_all(&path);

            let config = YcsbConfig {
                workload: workload.parse().unwrap(),
                record_count: 100,
                operation_count: 100,
                key_size: 16,
                value_size: 100,
                seed: 42,
                scan_length: 10,
            };

            eprint!("Running {} ... ", name);

            let result = match backend {
                BackendChoice::Crackeddb => {
                    run_ycsb::<CrackeddbBackend>(&backend.to_string(), &path, config)
                }
                BackendChoice::Rocksdb => {
                    run_ycsb::<RocksdbBackend>(&backend.to_string(), &path, config)
                }
                BackendChoice::Lmdb => run_ycsb::<LmdbBackend>(&backend.to_string(), &path, config),
                BackendChoice::Sled => run_ycsb::<SledBackend>(&backend.to_string(), &path, config),
            };

            match result {
                Ok(metrics) => {
                    eprintln!("OK ({:.1} ops/sec)", metrics.throughput_ops_sec);
                    all_results.push(metrics);
                    passed += 1;
                }
                Err(e) => {
                    eprintln!("FAILED: {}", e);
                    failed += 1;
                }
            }
        }
    }

    // TPC-C workload (single-threaded, quick)
    for backend in &backends {
        let name = format!("{}_tpcc", backend);
        let path = out_dir.join(&name);

        // Clean up any existing database
        let _ = std::fs::remove_dir_all(&path);

        let config = TpccConfig {
            warehouses: 1,
            items: 10,
            orders_per_run: 10,
            seed: 42,
        };

        eprint!("Running {} ... ", name);

        let result = match backend {
            BackendChoice::Crackeddb => {
                run_tpcc::<CrackeddbBackend>(&backend.to_string(), &path, config)
            }
            BackendChoice::Rocksdb => {
                run_tpcc::<RocksdbBackend>(&backend.to_string(), &path, config)
            }
            BackendChoice::Lmdb => run_tpcc::<LmdbBackend>(&backend.to_string(), &path, config),
            BackendChoice::Sled => run_tpcc::<SledBackend>(&backend.to_string(), &path, config),
        };

        match result {
            Ok(metrics) => {
                eprintln!("OK ({:.1} ops/sec)", metrics.throughput_ops_sec);
                all_results.push(metrics);
                passed += 1;
            }
            Err(e) => {
                eprintln!("FAILED: {}", e);
                failed += 1;
            }
        }
    }

    eprintln!();
    eprintln!("Smoke test summary: {} passed, {} failed", passed, failed);

    if failed > 0 {
        return Err(backends::Error::Other(format!(
            "{} smoke tests failed",
            failed
        )));
    }

    // Return a summary
    Ok(metrics::BenchMetrics {
        backend: "smoke".to_string(),
        workload: "all".to_string(),
        ops_count: all_results.iter().map(|m| m.ops_count).sum(),
        duration_secs: all_results.iter().map(|m| m.duration_secs).sum(),
        throughput_ops_sec: all_results
            .iter()
            .map(|m| m.throughput_ops_sec)
            .sum::<f64>()
            / all_results.len() as f64,
        latency_us: metrics::LatencyMetrics {
            p50: 0,
            p90: 0,
            p99: 0,
            p999: 0,
            max: 0,
            mean: 0.0,
        },
        write_amp: None,
        space_amp: None,
        recovery_ms: None,
        commits_success: all_results.iter().map(|m| m.commits_success).sum(),
        commits_aborted: all_results.iter().map(|m| m.commits_aborted).sum(),
    })
}
