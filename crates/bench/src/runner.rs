//! Benchmark runner that drives workloads against backends.
//!
//! Provides functions to run YCSB and TPC-C workloads against any backend
//! implementing the Backend trait, collecting metrics throughout.
//!
//! # Concurrent Execution
//!
//! The `concurrent_run_*` functions spawn multiple worker threads that execute
//! operations in parallel. Each worker has its own generator with a unique seed
//! (base_seed + worker_id) for reproducibility.
//!
//! Workers synchronize with a barrier at the start of measurement to ensure
//! all threads begin simultaneously.

use crate::backends::{Backend, BackendTxn, CommitOutcome};
use crate::metrics::{
    BenchMetrics, LatencyCollector, LatencyMetrics, RunResult as ConcurrentRunResult, WriteTracker,
};
use crate::workloads::tpcc::{self, TpccConfig, TpccGenerator};
use crate::workloads::ycsb::{Operation, YcsbConfig, YcsbGenerator};
use std::ops::Bound;
use std::path::Path;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

/// Result of a single-threaded benchmark run (legacy API).
pub type SingleRunResult = Result<BenchMetrics, crate::backends::Error>;

/// Runs a YCSB workload against a backend.
///
/// # Arguments
/// * `backend_name` - Name for metrics reporting
/// * `path` - Path to open the database
/// * `config` - YCSB workload configuration
pub fn run_ycsb<B: Backend>(
    backend_name: &str,
    path: &Path,
    config: YcsbConfig,
) -> SingleRunResult {
    let workload_name = config.workload.to_string();
    let operation_count = config.operation_count;

    // Open database
    let db = B::open(path)?;

    // Load initial data
    let mut gen = YcsbGenerator::new(config);
    {
        let mut txn = db.begin()?;
        for (key, value) in gen.load_keys() {
            txn.put(&key, &value)?;
        }
        txn.commit()?;
    }

    // Get initial disk size for space amp
    let initial_disk_size = db.disk_size_bytes().ok();

    // Run operations and collect metrics
    let mut latency = LatencyCollector::new();
    let mut write_tracker = WriteTracker::new();
    let mut commits_success = 0u64;
    let mut commits_aborted = 0u64;

    let start = Instant::now();

    for _ in 0..operation_count {
        let op = gen.next_op();
        let op_start = Instant::now();

        let outcome = execute_ycsb_op(&db, &op, &mut gen, &mut write_tracker)?;

        latency.record(op_start.elapsed());

        if outcome.success {
            commits_success += 1;
        }
        if outcome.aborted_for_conflict {
            commits_aborted += 1;
        }
    }

    let duration = start.elapsed();

    // Get final disk size for space amp
    let final_disk_size = db.disk_size_bytes().ok();
    let space_amp = match (initial_disk_size, final_disk_size) {
        (Some(_initial), Some(final_size)) if write_tracker.logical_bytes > 0 => {
            Some(final_size as f64 / write_tracker.logical_bytes as f64)
        }
        _ => None,
    };

    db.close()?;

    Ok(BenchMetrics {
        backend: backend_name.to_string(),
        workload: workload_name,
        ops_count: operation_count,
        duration_secs: duration.as_secs_f64(),
        throughput_ops_sec: operation_count as f64 / duration.as_secs_f64(),
        latency_us: latency.metrics(),
        write_amp: write_tracker.write_amp(),
        space_amp,
        recovery_ms: None, // Not measured in basic run
        commits_success,
        commits_aborted,
    })
}

/// Executes a single YCSB operation.
fn execute_ycsb_op<B: Backend>(
    db: &B,
    op: &Operation,
    gen: &mut YcsbGenerator,
    write_tracker: &mut WriteTracker,
) -> Result<CommitOutcome, crate::backends::Error> {
    let mut txn = db.begin()?;

    match op {
        Operation::Read(key) => {
            let _ = txn.get(key)?;
        }
        Operation::Update(key, value) => {
            write_tracker.record_logical((key.len() + value.len()) as u64);
            txn.put(key, value)?;
        }
        Operation::Insert(key, value) => {
            write_tracker.record_logical((key.len() + value.len()) as u64);
            txn.put(key, value)?;
        }
        Operation::Scan(key, length) => {
            // Scan from key, taking `length` records
            let results = txn.scan(Bound::Included(key.as_slice()), Bound::Unbounded)?;
            // Consume exactly `length` records to measure real scan cost
            let _consumed: Vec<_> = results.into_iter().take(*length as usize).collect();
        }
        Operation::ReadModifyWrite(key) => {
            // Read
            let _existing = txn.get(key)?;
            // Modify (generate new value)
            let new_value = gen.generate_value();
            write_tracker.record_logical((key.len() + new_value.len()) as u64);
            txn.put(key, &new_value)?;
        }
    }

    txn.commit()
}

/// Runs a TPC-C workload against a backend.
///
/// # Arguments
/// * `backend_name` - Name for metrics reporting
/// * `path` - Path to open the database
/// * `config` - TPC-C workload configuration
pub fn run_tpcc<B: Backend>(
    backend_name: &str,
    path: &Path,
    config: TpccConfig,
) -> SingleRunResult {
    let orders_per_run = config.orders_per_run;

    // Open database
    let db = B::open(path)?;

    // Load initial data
    let mut gen = TpccGenerator::new(config);
    let mut write_tracker = WriteTracker::new();
    {
        let mut txn = db.begin()?;
        for (key, value) in gen.load_data() {
            write_tracker.record_logical((key.len() + value.len()) as u64);
            txn.put(&key, &value)?;
        }
        txn.commit()?;
    }

    // Run transactions and collect metrics
    let mut latency = LatencyCollector::new();
    let mut commits_success = 0u64;
    let mut commits_aborted = 0u64;

    let start = Instant::now();

    for _ in 0..orders_per_run {
        let txn_data = gen.new_order();
        let op_start = Instant::now();

        // Track logical bytes for the transaction
        for _item in &txn_data.items {
            // Stock update + order line
            write_tracker.record_logical(12 + 8); // stock value + order line value
        }
        write_tracker.record_logical(8 + 12); // warehouse balance update + order record

        let result = tpcc::execute_new_order(&db, &txn_data)?;

        latency.record(op_start.elapsed());

        if result.success {
            commits_success += 1;
        }
        if result.aborted_for_conflict {
            commits_aborted += 1;
        }
    }

    let duration = start.elapsed();

    // Get final disk size for space amp
    let final_disk_size = db.disk_size_bytes().ok();
    let space_amp = match final_disk_size {
        Some(size) if write_tracker.logical_bytes > 0 => {
            Some(size as f64 / write_tracker.logical_bytes as f64)
        }
        _ => None,
    };

    db.close()?;

    Ok(BenchMetrics {
        backend: backend_name.to_string(),
        workload: "tpcc_new_order".to_string(),
        ops_count: orders_per_run,
        duration_secs: duration.as_secs_f64(),
        throughput_ops_sec: orders_per_run as f64 / duration.as_secs_f64(),
        latency_us: latency.metrics(),
        write_amp: write_tracker.write_amp(),
        space_amp,
        recovery_ms: None, // Not measured in basic run
        commits_success,
        commits_aborted,
    })
}

/// Measures recovery time for a backend.
///
/// Opens a database, closes it, and measures how long reopening takes.
pub fn measure_recovery_time<B: Backend>(path: &Path) -> Result<f64, crate::backends::Error> {
    // First open to ensure database exists
    let db = B::open(path)?;
    db.close()?;

    // Measure reopen time
    let start = Instant::now();
    let db = B::open(path)?;
    let recovery_ms = start.elapsed().as_secs_f64() * 1000.0;
    db.close()?;

    Ok(recovery_ms)
}

/// Cold-start measurement result.
#[derive(Debug, Clone)]
pub struct ColdStartMetrics {
    /// Time to open database in microseconds.
    pub open_us: u64,
    /// Time for first read after open in microseconds.
    pub first_read_us: u64,
}

/// Measures cold-start performance for a database.
///
/// Opens the database and performs a single read to measure:
/// - Time to open database (includes WAL recovery, index loading)
/// - Time for first read (measures cache miss / disk seek)
///
/// The database must already exist with data loaded.
pub fn measure_cold_start<B: Backend>(
    path: &Path,
    first_key: &[u8],
) -> Result<ColdStartMetrics, crate::backends::Error> {
    // Measure open time
    let open_start = Instant::now();
    let db = B::open(path)?;
    let open_us = open_start.elapsed().as_micros() as u64;

    // Measure first read time
    let read_start = Instant::now();
    let mut txn = db.begin()?;
    let _value = txn.get(first_key)?;
    txn.rollback()?;
    let first_read_us = read_start.elapsed().as_micros() as u64;

    db.close()?;

    Ok(ColdStartMetrics {
        open_us,
        first_read_us,
    })
}

/// Configuration for concurrent benchmark execution.
#[derive(Debug, Clone)]
pub struct ConcurrentConfig {
    /// Number of worker threads.
    pub workers: u32,
    /// Warmup duration in seconds.
    pub warmup_secs: f64,
    /// Measurement duration in seconds.
    pub measurement_secs: f64,
    /// Base seed for reproducibility.
    pub seed: u64,
    /// Run ID for multi-run tracking.
    pub run_id: u32,
}

impl Default for ConcurrentConfig {
    fn default() -> Self {
        Self {
            workers: 16,
            warmup_secs: 60.0,
            measurement_secs: 120.0,
            seed: 0xDEADBEEF,
            run_id: 0,
        }
    }
}

/// Result from a single worker thread.
struct WorkerResult {
    #[allow(dead_code)]
    worker_id: u32,
    latency: LatencyCollector,
    ops_count: u64,
    commits_success: u64,
    commits_aborted: u64,
    logical_bytes: u64,
}

/// Runs a concurrent YCSB workload against a backend.
///
/// Spawns `config.workers` threads, each executing operations from its own
/// generator (seeded with base_seed + worker_id). Returns detailed metrics
/// including per-worker latency and abort counts.
///
/// # Arguments
/// * `backend_name` - Name for metrics reporting
/// * `path` - Path to open the database
/// * `ycsb_config` - YCSB workload configuration
/// * `concurrent_config` - Concurrency and timing configuration
/// * `measure_cold_start` - If true, measure cold-start time (close and reopen db)
pub fn concurrent_run_ycsb<B: Backend + 'static>(
    backend_name: &str,
    path: &Path,
    ycsb_config: YcsbConfig,
    concurrent_config: ConcurrentConfig,
) -> Result<ConcurrentRunResult, crate::backends::Error> {
    concurrent_run_ycsb_with_cold_start::<B>(
        backend_name,
        path,
        ycsb_config,
        concurrent_config,
        false,
    )
}

/// Runs a concurrent YCSB workload with optional cold-start measurement.
pub fn concurrent_run_ycsb_with_cold_start<B: Backend + 'static>(
    backend_name: &str,
    path: &Path,
    ycsb_config: YcsbConfig,
    concurrent_config: ConcurrentConfig,
    measure_cold: bool,
) -> Result<ConcurrentRunResult, crate::backends::Error> {
    let workload_name = ycsb_config.workload.to_string();
    let num_workers = concurrent_config.workers as usize;

    // Open database
    let db = B::open(path)?;

    // Load initial data (single-threaded)
    let load_gen = YcsbGenerator::new(ycsb_config.clone());
    let mut logical_bytes_load = 0u64;
    let first_key = load_gen.load_keys().next().map(|(k, _)| k);
    {
        let mut txn = db.begin()?;
        for (key, value) in YcsbGenerator::new(ycsb_config.clone()).load_keys() {
            logical_bytes_load += (key.len() + value.len()) as u64;
            txn.put(&key, &value)?;
        }
        txn.commit()?;
    }

    // Measure cold-start if requested (close and reopen to get realistic metrics)
    let (db, cold_metrics) = if measure_cold {
        // Close the database we just loaded
        db.close()?;

        // Measure cold-start
        let first_key = first_key.unwrap_or_else(|| b"user0000000000000".to_vec());
        let metrics = measure_cold_start::<B>(path, &first_key)?;

        // Reopen for the actual benchmark
        let db = Arc::new(B::open(path)?);
        (db, Some(metrics))
    } else {
        (Arc::new(db), None)
    };

    // Get initial disk size for space amp
    let initial_disk_size = db.disk_size_bytes().ok();

    // Barrier for synchronized start (workers + main thread)
    let warmup_barrier = Arc::new(Barrier::new(num_workers + 1));
    let measurement_barrier = Arc::new(Barrier::new(num_workers + 1));
    let stop_barrier = Arc::new(Barrier::new(num_workers + 1));

    // Spawn worker threads
    let mut handles = Vec::with_capacity(num_workers);

    for worker_id in 0..num_workers {
        let db = Arc::clone(&db);
        let warmup_barrier = Arc::clone(&warmup_barrier);
        let measurement_barrier = Arc::clone(&measurement_barrier);
        let stop_barrier = Arc::clone(&stop_barrier);

        // Create worker-specific generator with unique seed
        let mut worker_config = ycsb_config.clone();
        worker_config.seed = concurrent_config.seed.wrapping_add(worker_id as u64);
        let warmup_secs = concurrent_config.warmup_secs;
        let measurement_secs = concurrent_config.measurement_secs;

        let handle = thread::spawn(move || {
            run_ycsb_worker(
                db,
                worker_id as u32,
                worker_config,
                warmup_barrier,
                measurement_barrier,
                stop_barrier,
                warmup_secs,
                measurement_secs,
            )
        });
        handles.push(handle);
    }

    // Signal warmup start
    warmup_barrier.wait();
    let warmup_start = Instant::now();

    // Wait for warmup to complete
    thread::sleep(Duration::from_secs_f64(concurrent_config.warmup_secs));

    // Signal measurement start
    measurement_barrier.wait();
    let measurement_start = Instant::now();

    // Wait for measurement to complete
    thread::sleep(Duration::from_secs_f64(concurrent_config.measurement_secs));

    // Signal stop
    stop_barrier.wait();
    let measurement_duration = measurement_start.elapsed();

    // Collect worker results
    let mut worker_results = Vec::with_capacity(num_workers);
    for handle in handles {
        match handle.join() {
            Ok(result) => worker_results.push(result),
            Err(_) => {
                return Err(crate::backends::Error::Other(
                    "worker thread panicked".into(),
                ))
            }
        }
    }

    // Aggregate results
    let total_ops: u64 = worker_results.iter().map(|r| r.ops_count).sum();
    let total_success: u64 = worker_results.iter().map(|r| r.commits_success).sum();
    let total_aborted: u64 = worker_results.iter().map(|r| r.commits_aborted).sum();
    let total_logical_bytes: u64 =
        logical_bytes_load + worker_results.iter().map(|r| r.logical_bytes).sum::<u64>();

    // Merge latency collectors
    let merged_latency = merge_latency_collectors(&worker_results);
    let per_worker_latency: Vec<LatencyMetrics> =
        worker_results.iter().map(|r| r.latency.metrics()).collect();
    let per_worker_aborts: Vec<u64> = worker_results.iter().map(|r| r.commits_aborted).collect();

    // Calculate write amplification and space amplification
    let final_disk_size = db.disk_size_bytes().ok();
    let space_amp = match (initial_disk_size, final_disk_size) {
        (Some(_), Some(final_size)) if total_logical_bytes > 0 => {
            final_size as f64 / total_logical_bytes as f64
        }
        _ => 1.0,
    };

    // Close database (need to get the Arc's inner value)
    drop(worker_results); // Ensure workers have dropped their Arc refs
    match Arc::try_unwrap(db) {
        Ok(db) => db.close()?,
        Err(_) => {
            return Err(crate::backends::Error::Other(
                "failed to close database".into(),
            ))
        }
    }

    let throughput = total_ops as f64 / measurement_duration.as_secs_f64();

    Ok(ConcurrentRunResult {
        seed: concurrent_config.seed,
        run_id: concurrent_config.run_id,
        backend: backend_name.to_string(),
        workload: workload_name,
        workers: concurrent_config.workers,
        record_count: ycsb_config.record_count,
        operation_count: total_ops,
        warmup_secs: warmup_start.elapsed().as_secs_f64() - measurement_duration.as_secs_f64(),
        measurement_secs: measurement_duration.as_secs_f64(),
        throughput_ops_sec: throughput,
        latency_us: merged_latency,
        per_worker_latency_us: per_worker_latency,
        commits_success: total_success,
        commits_aborted: total_aborted,
        per_worker_aborts,
        write_amp: None, // TODO: measure physical writes
        space_amp,
        cold_open_us: cold_metrics.as_ref().map(|m| m.open_us),
        first_read_us: cold_metrics.as_ref().map(|m| m.first_read_us),
    })
}

/// Worker function for concurrent YCSB execution.
fn run_ycsb_worker<B: Backend>(
    db: Arc<B>,
    worker_id: u32,
    config: YcsbConfig,
    warmup_barrier: Arc<Barrier>,
    measurement_barrier: Arc<Barrier>,
    stop_barrier: Arc<Barrier>,
    warmup_secs: f64,
    measurement_secs: f64,
) -> WorkerResult {
    let mut gen = YcsbGenerator::new(config);
    let mut write_tracker = WriteTracker::new();

    // Wait for warmup start
    warmup_barrier.wait();

    // Warmup phase: run ops but don't record latency
    let warmup_deadline = Instant::now() + Duration::from_secs_f64(warmup_secs);
    while Instant::now() < warmup_deadline {
        let op = gen.next_op();
        let _ = execute_ycsb_op_worker(&db, &op, &mut gen, &mut write_tracker);
    }

    // Wait for measurement start
    measurement_barrier.wait();

    // Reset tracker for measurement phase
    let mut measurement_tracker = WriteTracker::new();
    let mut latency = LatencyCollector::new();
    let mut ops_count = 0u64;
    let mut commits_success = 0u64;
    let mut commits_aborted = 0u64;

    // Measurement phase
    let measurement_deadline = Instant::now() + Duration::from_secs_f64(measurement_secs);
    while Instant::now() < measurement_deadline {
        let op = gen.next_op();
        let op_start = Instant::now();

        match execute_ycsb_op_worker(&db, &op, &mut gen, &mut measurement_tracker) {
            Ok(outcome) => {
                latency.record(op_start.elapsed());
                ops_count += 1;
                if outcome.success {
                    commits_success += 1;
                }
                if outcome.aborted_for_conflict {
                    commits_aborted += 1;
                }
            }
            Err(_) => {
                // Count error as abort
                commits_aborted += 1;
            }
        }
    }

    // Wait for stop signal
    stop_barrier.wait();

    WorkerResult {
        worker_id,
        latency,
        ops_count,
        commits_success,
        commits_aborted,
        logical_bytes: measurement_tracker.logical_bytes,
    }
}

/// Executes a single YCSB operation (worker version, takes Arc<B>).
fn execute_ycsb_op_worker<B: Backend>(
    db: &Arc<B>,
    op: &Operation,
    gen: &mut YcsbGenerator,
    write_tracker: &mut WriteTracker,
) -> Result<CommitOutcome, crate::backends::Error> {
    let mut txn = db.begin()?;

    match op {
        Operation::Read(key) => {
            let _ = txn.get(key)?;
        }
        Operation::Update(key, value) => {
            write_tracker.record_logical((key.len() + value.len()) as u64);
            txn.put(key, value)?;
        }
        Operation::Insert(key, value) => {
            write_tracker.record_logical((key.len() + value.len()) as u64);
            txn.put(key, value)?;
        }
        Operation::Scan(key, length) => {
            let results = txn.scan(Bound::Included(key.as_slice()), Bound::Unbounded)?;
            let _consumed: Vec<_> = results.into_iter().take(*length as usize).collect();
        }
        Operation::ReadModifyWrite(key) => {
            let _existing = txn.get(key)?;
            let new_value = gen.generate_value();
            write_tracker.record_logical((key.len() + new_value.len()) as u64);
            txn.put(key, &new_value)?;
        }
    }

    txn.commit()
}

/// Merges latency collectors from multiple workers.
///
/// Creates a merged histogram by sampling from each worker's distribution.
fn merge_latency_collectors(results: &[WorkerResult]) -> LatencyMetrics {
    // Create a new collector to hold merged samples
    let mut merged = LatencyCollector::new();

    // For each worker, add their recorded latencies to the merged collector
    // We use the histogram's recorded values
    for result in results {
        // Get the metrics from each worker and weight by ops count
        let worker_metrics = result.latency.metrics();
        let count = result.latency.count();

        // Add representative samples from this worker
        // We add p50, p90, p99 weighted by count to approximate the distribution
        if count > 0 {
            // Add samples at key percentiles weighted by count
            let samples_to_add = count.min(10000) as usize; // Cap for performance
            for _ in 0..samples_to_add / 4 {
                merged.record(Duration::from_micros(worker_metrics.p50));
            }
            for _ in 0..samples_to_add / 4 {
                merged.record(Duration::from_micros(
                    (worker_metrics.p50 + worker_metrics.p90) / 2,
                ));
            }
            for _ in 0..samples_to_add / 4 {
                merged.record(Duration::from_micros(worker_metrics.p90));
            }
            for _ in 0..samples_to_add / 4 {
                merged.record(Duration::from_micros(worker_metrics.p99));
            }
        }
    }

    // If no samples, return zeros
    if merged.count() == 0 {
        return LatencyMetrics {
            p50: 0,
            p90: 0,
            p99: 0,
            p999: 0,
            max: 0,
            mean: 0.0,
        };
    }

    merged.metrics()
}

/// Runs a concurrent TPC-C workload against a backend.
///
/// Similar to `concurrent_run_ycsb`, but for TPC-C new order transactions.
pub fn concurrent_run_tpcc<B: Backend + 'static>(
    backend_name: &str,
    path: &Path,
    tpcc_config: TpccConfig,
    concurrent_config: ConcurrentConfig,
) -> Result<ConcurrentRunResult, crate::backends::Error> {
    concurrent_run_tpcc_with_cold_start::<B>(
        backend_name,
        path,
        tpcc_config,
        concurrent_config,
        false,
    )
}

/// Runs a concurrent TPC-C workload with optional cold-start measurement.
pub fn concurrent_run_tpcc_with_cold_start<B: Backend + 'static>(
    backend_name: &str,
    path: &Path,
    tpcc_config: TpccConfig,
    concurrent_config: ConcurrentConfig,
    measure_cold: bool,
) -> Result<ConcurrentRunResult, crate::backends::Error> {
    let num_workers = concurrent_config.workers as usize;

    // Open database
    let db = B::open(path)?;

    // Load initial data (single-threaded)
    let load_gen = TpccGenerator::new(tpcc_config.clone());
    let mut logical_bytes_load = 0u64;
    let first_key = load_gen.load_data().next().map(|(k, _)| k);
    {
        let mut txn = db.begin()?;
        for (key, value) in TpccGenerator::new(tpcc_config.clone()).load_data() {
            logical_bytes_load += (key.len() + value.len()) as u64;
            txn.put(&key, &value)?;
        }
        txn.commit()?;
    }

    // Measure cold-start if requested
    let (db, cold_metrics) = if measure_cold {
        db.close()?;

        let first_key = first_key.unwrap_or_else(|| b"warehouse_1".to_vec());
        let metrics = measure_cold_start::<B>(path, &first_key)?;

        let db = Arc::new(B::open(path)?);
        (db, Some(metrics))
    } else {
        (Arc::new(db), None)
    };

    // Get initial disk size for space amp
    let initial_disk_size = db.disk_size_bytes().ok();

    // Barriers for synchronization
    let warmup_barrier = Arc::new(Barrier::new(num_workers + 1));
    let measurement_barrier = Arc::new(Barrier::new(num_workers + 1));
    let stop_barrier = Arc::new(Barrier::new(num_workers + 1));

    // Spawn worker threads
    let mut handles = Vec::with_capacity(num_workers);

    for worker_id in 0..num_workers {
        let db = Arc::clone(&db);
        let warmup_barrier = Arc::clone(&warmup_barrier);
        let measurement_barrier = Arc::clone(&measurement_barrier);
        let stop_barrier = Arc::clone(&stop_barrier);

        // Create worker-specific generator with unique seed
        let mut worker_config = tpcc_config.clone();
        worker_config.seed = concurrent_config.seed.wrapping_add(worker_id as u64);
        let warmup_secs = concurrent_config.warmup_secs;
        let measurement_secs = concurrent_config.measurement_secs;

        let handle = thread::spawn(move || {
            run_tpcc_worker(
                db,
                worker_id as u32,
                worker_config,
                warmup_barrier,
                measurement_barrier,
                stop_barrier,
                warmup_secs,
                measurement_secs,
            )
        });
        handles.push(handle);
    }

    // Signal warmup start
    warmup_barrier.wait();
    let warmup_start = Instant::now();

    // Wait for warmup
    thread::sleep(Duration::from_secs_f64(concurrent_config.warmup_secs));

    // Signal measurement start
    measurement_barrier.wait();
    let measurement_start = Instant::now();

    // Wait for measurement
    thread::sleep(Duration::from_secs_f64(concurrent_config.measurement_secs));

    // Signal stop
    stop_barrier.wait();
    let measurement_duration = measurement_start.elapsed();

    // Collect worker results
    let mut worker_results = Vec::with_capacity(num_workers);
    for handle in handles {
        match handle.join() {
            Ok(result) => worker_results.push(result),
            Err(_) => {
                return Err(crate::backends::Error::Other(
                    "worker thread panicked".into(),
                ))
            }
        }
    }

    // Aggregate results
    let total_ops: u64 = worker_results.iter().map(|r| r.ops_count).sum();
    let total_success: u64 = worker_results.iter().map(|r| r.commits_success).sum();
    let total_aborted: u64 = worker_results.iter().map(|r| r.commits_aborted).sum();
    let total_logical_bytes: u64 =
        logical_bytes_load + worker_results.iter().map(|r| r.logical_bytes).sum::<u64>();

    // Merge latency
    let merged_latency = merge_latency_collectors(&worker_results);
    let per_worker_latency: Vec<LatencyMetrics> =
        worker_results.iter().map(|r| r.latency.metrics()).collect();
    let per_worker_aborts: Vec<u64> = worker_results.iter().map(|r| r.commits_aborted).collect();

    // Calculate space amplification
    let final_disk_size = db.disk_size_bytes().ok();
    let space_amp = match (initial_disk_size, final_disk_size) {
        (Some(_), Some(final_size)) if total_logical_bytes > 0 => {
            final_size as f64 / total_logical_bytes as f64
        }
        _ => 1.0,
    };

    // Close database
    drop(worker_results);
    match Arc::try_unwrap(db) {
        Ok(db) => db.close()?,
        Err(_) => {
            return Err(crate::backends::Error::Other(
                "failed to close database".into(),
            ))
        }
    }

    let throughput = total_ops as f64 / measurement_duration.as_secs_f64();

    Ok(ConcurrentRunResult {
        seed: concurrent_config.seed,
        run_id: concurrent_config.run_id,
        backend: backend_name.to_string(),
        workload: "tpcc_new_order".to_string(),
        workers: concurrent_config.workers,
        record_count: 0, // N/A for TPC-C
        operation_count: total_ops,
        warmup_secs: warmup_start.elapsed().as_secs_f64() - measurement_duration.as_secs_f64(),
        measurement_secs: measurement_duration.as_secs_f64(),
        throughput_ops_sec: throughput,
        latency_us: merged_latency,
        per_worker_latency_us: per_worker_latency,
        commits_success: total_success,
        commits_aborted: total_aborted,
        per_worker_aborts,
        write_amp: None,
        space_amp,
        cold_open_us: cold_metrics.as_ref().map(|m| m.open_us),
        first_read_us: cold_metrics.as_ref().map(|m| m.first_read_us),
    })
}

/// Runs multiple benchmark iterations and returns median summary.
///
/// Performs `num_runs` benchmark runs, writing each result to JSONL output,
/// and returns a summary with median values across runs.
pub fn multi_run_ycsb<B: Backend + 'static>(
    backend_name: &str,
    path: &Path,
    ycsb_config: YcsbConfig,
    concurrent_config: ConcurrentConfig,
    num_runs: u32,
    output: Option<&mut crate::metrics::JsonlWriter>,
) -> Result<(Vec<ConcurrentRunResult>, crate::metrics::MultiRunSummary), crate::backends::Error> {
    multi_run_ycsb_with_cold_start::<B>(
        backend_name,
        path,
        ycsb_config,
        concurrent_config,
        num_runs,
        output,
        false,
    )
}

/// Runs multiple YCSB benchmark iterations with optional cold-start measurement.
pub fn multi_run_ycsb_with_cold_start<B: Backend + 'static>(
    backend_name: &str,
    path: &Path,
    ycsb_config: YcsbConfig,
    concurrent_config: ConcurrentConfig,
    num_runs: u32,
    mut output: Option<&mut crate::metrics::JsonlWriter>,
    measure_cold_start: bool,
) -> Result<(Vec<ConcurrentRunResult>, crate::metrics::MultiRunSummary), crate::backends::Error> {
    let mut results = Vec::with_capacity(num_runs as usize);

    for run_id in 0..num_runs {
        // Clean up database between runs
        let _ = std::fs::remove_dir_all(path);

        // Create config for this run
        let mut run_config = concurrent_config.clone();
        run_config.run_id = run_id;

        let result = concurrent_run_ycsb_with_cold_start::<B>(
            backend_name,
            path,
            ycsb_config.clone(),
            run_config,
            measure_cold_start,
        )?;

        // Write to JSONL if output provided
        if let Some(writer) = output.as_mut() {
            writer
                .write_result(&result)
                .map_err(|e| crate::backends::Error::Other(e.to_string()))?;
        }

        results.push(result);
    }

    // Compute summary
    let summary = crate::metrics::MultiRunSummary::from_runs(&results)
        .ok_or_else(|| crate::backends::Error::Other("no results to summarize".into()))?;

    // Write summary to JSONL if output provided
    if let Some(writer) = output.as_mut() {
        writer
            .write_summary(&summary)
            .map_err(|e| crate::backends::Error::Other(e.to_string()))?;
    }

    Ok((results, summary))
}

/// Runs multiple TPC-C benchmark iterations and returns median summary.
pub fn multi_run_tpcc<B: Backend + 'static>(
    backend_name: &str,
    path: &Path,
    tpcc_config: TpccConfig,
    concurrent_config: ConcurrentConfig,
    num_runs: u32,
    output: Option<&mut crate::metrics::JsonlWriter>,
) -> Result<(Vec<ConcurrentRunResult>, crate::metrics::MultiRunSummary), crate::backends::Error> {
    multi_run_tpcc_with_cold_start::<B>(
        backend_name,
        path,
        tpcc_config,
        concurrent_config,
        num_runs,
        output,
        false,
    )
}

/// Runs multiple TPC-C benchmark iterations with optional cold-start measurement.
pub fn multi_run_tpcc_with_cold_start<B: Backend + 'static>(
    backend_name: &str,
    path: &Path,
    tpcc_config: TpccConfig,
    concurrent_config: ConcurrentConfig,
    num_runs: u32,
    mut output: Option<&mut crate::metrics::JsonlWriter>,
    measure_cold_start: bool,
) -> Result<(Vec<ConcurrentRunResult>, crate::metrics::MultiRunSummary), crate::backends::Error> {
    let mut results = Vec::with_capacity(num_runs as usize);

    for run_id in 0..num_runs {
        // Clean up database between runs
        let _ = std::fs::remove_dir_all(path);

        // Create config for this run
        let mut run_config = concurrent_config.clone();
        run_config.run_id = run_id;

        let result = concurrent_run_tpcc_with_cold_start::<B>(
            backend_name,
            path,
            tpcc_config.clone(),
            run_config,
            measure_cold_start,
        )?;

        // Write to JSONL if output provided
        if let Some(writer) = output.as_mut() {
            writer
                .write_result(&result)
                .map_err(|e| crate::backends::Error::Other(e.to_string()))?;
        }

        results.push(result);
    }

    // Compute summary
    let summary = crate::metrics::MultiRunSummary::from_runs(&results)
        .ok_or_else(|| crate::backends::Error::Other("no results to summarize".into()))?;

    // Write summary to JSONL if output provided
    if let Some(writer) = output.as_mut() {
        writer
            .write_summary(&summary)
            .map_err(|e| crate::backends::Error::Other(e.to_string()))?;
    }

    Ok((results, summary))
}

/// Worker function for concurrent TPC-C execution.
fn run_tpcc_worker<B: Backend>(
    db: Arc<B>,
    worker_id: u32,
    config: TpccConfig,
    warmup_barrier: Arc<Barrier>,
    measurement_barrier: Arc<Barrier>,
    stop_barrier: Arc<Barrier>,
    warmup_secs: f64,
    measurement_secs: f64,
) -> WorkerResult {
    let mut gen = TpccGenerator::new(config);

    // Wait for warmup start
    warmup_barrier.wait();

    // Warmup phase
    let warmup_deadline = Instant::now() + Duration::from_secs_f64(warmup_secs);
    while Instant::now() < warmup_deadline {
        let txn_data = gen.new_order();
        let _ = tpcc::execute_new_order(&*db, &txn_data);
    }

    // Wait for measurement start
    measurement_barrier.wait();

    // Reset tracker for measurement phase
    let mut measurement_tracker = WriteTracker::new();
    let mut latency = LatencyCollector::new();
    let mut ops_count = 0u64;
    let mut commits_success = 0u64;
    let mut commits_aborted = 0u64;

    // Measurement phase
    let measurement_deadline = Instant::now() + Duration::from_secs_f64(measurement_secs);
    while Instant::now() < measurement_deadline {
        let txn_data = gen.new_order();
        let op_start = Instant::now();

        // Track logical bytes
        for _item in &txn_data.items {
            measurement_tracker.record_logical(12 + 8);
        }
        measurement_tracker.record_logical(8 + 12);

        match tpcc::execute_new_order(&*db, &txn_data) {
            Ok(outcome) => {
                latency.record(op_start.elapsed());
                ops_count += 1;
                if outcome.success {
                    commits_success += 1;
                }
                if outcome.aborted_for_conflict {
                    commits_aborted += 1;
                }
            }
            Err(_) => {
                commits_aborted += 1;
            }
        }
    }

    // Wait for stop signal
    stop_barrier.wait();

    WorkerResult {
        worker_id,
        latency,
        ops_count,
        commits_success,
        commits_aborted,
        logical_bytes: measurement_tracker.logical_bytes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::CrackeddbBackend;
    use crate::workloads::ycsb::YcsbWorkload;
    use tempfile::TempDir;

    #[test]
    fn metrics_collection_produces_sane_values() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("db");

        let config = YcsbConfig {
            workload: YcsbWorkload::A,
            record_count: 100,
            operation_count: 100,
            key_size: 16,
            value_size: 100,
            seed: 42,
            scan_length: 10,
        };

        let metrics = run_ycsb::<CrackeddbBackend>("crackeddb", &path, config).unwrap();

        // Check basic sanity
        assert_eq!(metrics.backend, "crackeddb");
        assert_eq!(metrics.workload, "ycsb_a");
        assert_eq!(metrics.ops_count, 100);
        assert!(metrics.duration_secs > 0.0);
        assert!(metrics.throughput_ops_sec > 0.0);

        // Latency should be positive
        assert!(metrics.latency_us.p50 > 0);
        assert!(metrics.latency_us.p99 >= metrics.latency_us.p50);
        assert!(metrics.latency_us.max >= metrics.latency_us.p99);

        // Should have some successful commits
        assert!(metrics.commits_success > 0);
    }

    #[test]
    fn tpcc_runner_produces_metrics() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("db");

        let config = TpccConfig {
            warehouses: 1,
            items: 10,
            orders_per_run: 10,
            seed: 42,
        };

        let metrics = run_tpcc::<CrackeddbBackend>("crackeddb", &path, config).unwrap();

        assert_eq!(metrics.backend, "crackeddb");
        assert_eq!(metrics.workload, "tpcc_new_order");
        assert_eq!(metrics.ops_count, 10);
        assert!(metrics.throughput_ops_sec > 0.0);
        assert!(metrics.commits_success > 0);
    }

    #[test]
    fn concurrent_ycsb_runs_with_multiple_workers() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("db");

        let ycsb_config = YcsbConfig {
            workload: YcsbWorkload::A,
            record_count: 100,
            operation_count: 1000, // ignored for concurrent (time-based)
            key_size: 16,
            value_size: 100,
            seed: 42,
            scan_length: 10,
        };

        let concurrent_config = ConcurrentConfig {
            workers: 4,
            warmup_secs: 0.5,      // Short warmup for test
            measurement_secs: 1.0, // Short measurement for test
            seed: 42,
            run_id: 0,
        };

        let result = concurrent_run_ycsb::<CrackeddbBackend>(
            "crackeddb",
            &path,
            ycsb_config,
            concurrent_config,
        )
        .unwrap();

        assert_eq!(result.backend, "crackeddb");
        assert_eq!(result.workload, "ycsb_a");
        assert_eq!(result.workers, 4);
        assert!(result.throughput_ops_sec > 0.0);
        assert!(result.operation_count > 0);
        assert_eq!(result.per_worker_latency_us.len(), 4);
        assert_eq!(result.per_worker_aborts.len(), 4);
    }

    #[test]
    fn concurrent_tpcc_runs_with_multiple_workers() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("db");

        let tpcc_config = TpccConfig {
            warehouses: 1,
            items: 10,
            orders_per_run: 100, // ignored for concurrent (time-based)
            seed: 42,
        };

        let concurrent_config = ConcurrentConfig {
            workers: 2,
            warmup_secs: 0.5,
            measurement_secs: 1.0,
            seed: 42,
            run_id: 0,
        };

        let result = concurrent_run_tpcc::<CrackeddbBackend>(
            "crackeddb",
            &path,
            tpcc_config,
            concurrent_config,
        )
        .unwrap();

        assert_eq!(result.backend, "crackeddb");
        assert_eq!(result.workload, "tpcc_new_order");
        assert_eq!(result.workers, 2);
        assert!(result.throughput_ops_sec > 0.0);
        assert_eq!(result.per_worker_latency_us.len(), 2);
    }

    #[test]
    fn multi_run_ycsb_computes_median() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("db");

        let ycsb_config = YcsbConfig {
            workload: YcsbWorkload::C, // Read-only for speed
            record_count: 50,
            operation_count: 100,
            key_size: 16,
            value_size: 100,
            seed: 42,
            scan_length: 10,
        };

        let concurrent_config = ConcurrentConfig {
            workers: 2,
            warmup_secs: 0.1,
            measurement_secs: 0.5,
            seed: 42,
            run_id: 0,
        };

        let (results, summary) = multi_run_ycsb::<CrackeddbBackend>(
            "crackeddb",
            &path,
            ycsb_config,
            concurrent_config,
            3, // 3 runs
            None,
        )
        .unwrap();

        assert_eq!(results.len(), 3);
        assert_eq!(summary.runs, 3);
        assert_eq!(summary.backend, "crackeddb");
        assert_eq!(summary.workload, "ycsb_c");
        assert!(summary.median_throughput > 0.0);
    }

    #[test]
    fn cold_start_measurement_captures_open_and_read() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("db");

        let ycsb_config = YcsbConfig {
            workload: YcsbWorkload::C,
            record_count: 100,
            operation_count: 100,
            key_size: 16,
            value_size: 100,
            seed: 42,
            scan_length: 10,
        };

        let concurrent_config = ConcurrentConfig {
            workers: 2,
            warmup_secs: 0.1,
            measurement_secs: 0.5,
            seed: 42,
            run_id: 0,
        };

        let result = concurrent_run_ycsb_with_cold_start::<CrackeddbBackend>(
            "crackeddb",
            &path,
            ycsb_config,
            concurrent_config,
            true, // measure cold start
        )
        .unwrap();

        // Cold-start metrics should be present when measure_cold is true
        assert!(result.cold_open_us.is_some());
        assert!(result.first_read_us.is_some());

        // Open time should be positive
        assert!(result.cold_open_us.unwrap() > 0);
        // First read time should be positive
        assert!(result.first_read_us.unwrap() > 0);
    }
}
