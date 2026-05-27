//! Metrics collection for benchmarks.
//!
//! Collects throughput, latency percentiles, write amplification, space
//! amplification, and recovery time.

use hdrhistogram::Histogram;
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::Path;
use std::time::Duration;

/// Benchmark metrics collected during a run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchMetrics {
    /// Name of the backend tested.
    pub backend: String,
    /// Name of the workload.
    pub workload: String,
    /// Total operations performed.
    pub ops_count: u64,
    /// Total duration of the run.
    pub duration_secs: f64,
    /// Operations per second.
    pub throughput_ops_sec: f64,
    /// Latency percentiles in microseconds.
    pub latency_us: LatencyMetrics,
    /// Write amplification (bytes written to disk / bytes written by user).
    pub write_amp: Option<f64>,
    /// Space amplification (disk size / logical data size).
    pub space_amp: Option<f64>,
    /// Recovery time in milliseconds (time to reopen after crash).
    pub recovery_ms: Option<f64>,
    /// Number of successful commits.
    pub commits_success: u64,
    /// Number of aborted transactions (due to conflicts).
    pub commits_aborted: u64,
}

/// Latency percentiles in microseconds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LatencyMetrics {
    pub p50: u64,
    pub p90: u64,
    pub p99: u64,
    pub p999: u64,
    pub max: u64,
    pub mean: f64,
}

/// Result of a single benchmark run with concurrent workers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunResult {
    /// Random seed used for this run.
    pub seed: u64,
    /// Run number (0, 1, 2 for 3-run mode).
    pub run_id: u32,
    /// Name of the backend tested.
    pub backend: String,
    /// Name of the workload.
    pub workload: String,
    /// Number of concurrent workers.
    pub workers: u32,
    /// Number of records loaded.
    pub record_count: u64,
    /// Number of operations executed (across all workers).
    pub operation_count: u64,
    /// Warmup duration in seconds.
    pub warmup_secs: f64,
    /// Measurement duration in seconds.
    pub measurement_secs: f64,
    /// Aggregate throughput (ops/sec) during measurement phase.
    pub throughput_ops_sec: f64,
    /// Aggregate latency percentiles across all workers.
    pub latency_us: LatencyMetrics,
    /// Per-worker latency percentiles.
    pub per_worker_latency_us: Vec<LatencyMetrics>,
    /// Total successful commits across all workers.
    pub commits_success: u64,
    /// Total aborted commits across all workers.
    pub commits_aborted: u64,
    /// Per-worker abort counts.
    pub per_worker_aborts: Vec<u64>,
    /// Write amplification (if measurable).
    pub write_amp: Option<f64>,
    /// Space amplification (disk size / logical data size).
    pub space_amp: f64,
    /// Cold-start: time to open database (microseconds).
    pub cold_open_us: Option<u64>,
    /// Cold-start: time for first read after open (microseconds).
    pub first_read_us: Option<u64>,
}

/// Summary of multiple runs (for 3-run repeatability).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MultiRunSummary {
    /// Backend name.
    pub backend: String,
    /// Workload name.
    pub workload: String,
    /// Number of workers used.
    pub workers: u32,
    /// Number of runs.
    pub runs: u32,
    /// Median throughput across runs.
    pub median_throughput: f64,
    /// Median p50 latency across runs.
    pub median_p50: u64,
    /// Median p99 latency across runs.
    pub median_p99: u64,
    /// Total aborts across all runs.
    pub total_aborts: u64,
    /// Median cold-start open time (microseconds).
    pub median_cold_open_us: Option<u64>,
    /// Median first-read time (microseconds).
    pub median_first_read_us: Option<u64>,
}

impl MultiRunSummary {
    /// Creates a summary from multiple run results.
    pub fn from_runs(results: &[RunResult]) -> Option<Self> {
        if results.is_empty() {
            return None;
        }

        let first = &results[0];
        let runs = results.len() as u32;

        // Extract values for median calculation
        let mut throughputs: Vec<f64> = results.iter().map(|r| r.throughput_ops_sec).collect();
        let mut p50s: Vec<u64> = results.iter().map(|r| r.latency_us.p50).collect();
        let mut p99s: Vec<u64> = results.iter().map(|r| r.latency_us.p99).collect();

        throughputs.sort_by(|a, b| a.partial_cmp(b).unwrap());
        p50s.sort();
        p99s.sort();

        let median_idx = results.len() / 2;

        let total_aborts: u64 = results.iter().map(|r| r.commits_aborted).sum();

        // Cold-start medians
        let median_cold_open_us = {
            let mut vals: Vec<u64> = results.iter().filter_map(|r| r.cold_open_us).collect();
            if vals.is_empty() {
                None
            } else {
                vals.sort();
                Some(vals[vals.len() / 2])
            }
        };

        let median_first_read_us = {
            let mut vals: Vec<u64> = results.iter().filter_map(|r| r.first_read_us).collect();
            if vals.is_empty() {
                None
            } else {
                vals.sort();
                Some(vals[vals.len() / 2])
            }
        };

        Some(Self {
            backend: first.backend.clone(),
            workload: first.workload.clone(),
            workers: first.workers,
            runs,
            median_throughput: throughputs[median_idx],
            median_p50: p50s[median_idx],
            median_p99: p99s[median_idx],
            total_aborts,
            median_cold_open_us,
            median_first_read_us,
        })
    }
}

/// Collects latency samples during benchmark runs.
pub struct LatencyCollector {
    histogram: Histogram<u64>,
}

impl LatencyCollector {
    /// Creates a new latency collector.
    ///
    /// Records latencies from 1 microsecond to 60 seconds with 3 significant
    /// figures of precision.
    pub fn new() -> Self {
        // 1 microsecond to 60 seconds, 3 significant figures
        let histogram =
            Histogram::new_with_bounds(1, 60_000_000, 3).expect("invalid histogram parameters");
        Self { histogram }
    }

    /// Records a latency sample.
    pub fn record(&mut self, duration: Duration) {
        let micros = duration.as_micros() as u64;
        // Clamp to max value if exceeds bounds
        let micros = micros.min(60_000_000).max(1);
        let _ = self.histogram.record(micros);
    }

    /// Returns the number of samples recorded.
    pub fn count(&self) -> u64 {
        self.histogram.len()
    }

    /// Computes latency metrics from collected samples.
    pub fn metrics(&self) -> LatencyMetrics {
        LatencyMetrics {
            p50: self.histogram.value_at_percentile(50.0),
            p90: self.histogram.value_at_percentile(90.0),
            p99: self.histogram.value_at_percentile(99.0),
            p999: self.histogram.value_at_percentile(99.9),
            max: self.histogram.max(),
            mean: self.histogram.mean(),
        }
    }
}

impl Default for LatencyCollector {
    fn default() -> Self {
        Self::new()
    }
}

/// Tracks bytes written for write amplification calculation.
#[derive(Debug, Default)]
pub struct WriteTracker {
    /// Logical bytes written by user operations.
    pub logical_bytes: u64,
    /// Physical bytes written to disk (if available).
    pub physical_bytes: Option<u64>,
}

impl WriteTracker {
    /// Creates a new write tracker.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records logical bytes written.
    pub fn record_logical(&mut self, bytes: u64) {
        self.logical_bytes += bytes;
    }

    /// Sets physical bytes written (from disk stats).
    pub fn set_physical(&mut self, bytes: u64) {
        self.physical_bytes = Some(bytes);
    }

    /// Computes write amplification if physical bytes are available.
    pub fn write_amp(&self) -> Option<f64> {
        self.physical_bytes.map(|phys| {
            if self.logical_bytes > 0 {
                phys as f64 / self.logical_bytes as f64
            } else {
                1.0
            }
        })
    }
}

/// JSONL output writer for benchmark results.
///
/// Appends one line of JSON per RunResult to a file.
pub struct JsonlWriter {
    writer: BufWriter<File>,
}

impl JsonlWriter {
    /// Opens a JSONL file for appending results.
    pub fn open(path: &Path) -> std::io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            writer: BufWriter::new(file),
        })
    }

    /// Writes a single RunResult as a JSON line.
    pub fn write_result(&mut self, result: &RunResult) -> std::io::Result<()> {
        let json = serde_json::to_string(result)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        writeln!(self.writer, "{}", json)?;
        self.writer.flush()
    }

    /// Writes a MultiRunSummary as a JSON line.
    pub fn write_summary(&mut self, summary: &MultiRunSummary) -> std::io::Result<()> {
        let json = serde_json::to_string(summary)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        writeln!(self.writer, "{}", json)?;
        self.writer.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latency_collector_produces_sane_values() {
        let mut collector = LatencyCollector::new();

        // Record 1000 samples with increasing latencies
        for i in 1..=1000 {
            collector.record(Duration::from_micros(i * 10));
        }

        let metrics = collector.metrics();

        // Check that percentiles are in order
        assert!(metrics.p50 <= metrics.p90);
        assert!(metrics.p90 <= metrics.p99);
        assert!(metrics.p99 <= metrics.p999);
        assert!(metrics.p999 <= metrics.max);

        // Check reasonable values for uniform distribution 10-10000 us
        assert!(
            metrics.p50 > 4000 && metrics.p50 < 6000,
            "p50: {}",
            metrics.p50
        );
        assert!(
            metrics.mean > 4000.0 && metrics.mean < 6000.0,
            "mean: {}",
            metrics.mean
        );
        assert_eq!(collector.count(), 1000);
    }

    #[test]
    fn write_tracker_computes_amplification() {
        let mut tracker = WriteTracker::new();

        tracker.record_logical(1000);
        tracker.record_logical(500);
        assert_eq!(tracker.logical_bytes, 1500);
        assert!(tracker.write_amp().is_none());

        tracker.set_physical(3000);
        let amp = tracker.write_amp().unwrap();
        assert!((amp - 2.0).abs() < 0.001, "write amp: {}", amp);
    }

    #[test]
    fn multi_run_summary_computes_medians() {
        let make_result = |throughput: f64, p50: u64, p99: u64, aborts: u64| RunResult {
            seed: 42,
            run_id: 0,
            backend: "test".to_string(),
            workload: "ycsb_a".to_string(),
            workers: 4,
            record_count: 1000,
            operation_count: 1000,
            warmup_secs: 10.0,
            measurement_secs: 60.0,
            throughput_ops_sec: throughput,
            latency_us: LatencyMetrics {
                p50,
                p90: p50 + 10,
                p99,
                p999: p99 + 10,
                max: p99 + 100,
                mean: p50 as f64,
            },
            per_worker_latency_us: vec![],
            commits_success: 1000,
            commits_aborted: aborts,
            per_worker_aborts: vec![],
            write_amp: None,
            space_amp: 1.0,
            cold_open_us: Some(1000),
            first_read_us: Some(100),
        };

        let results = vec![
            make_result(1000.0, 100, 500, 10),
            make_result(1200.0, 90, 450, 5), // median
            make_result(1100.0, 110, 550, 15),
        ];

        let summary = MultiRunSummary::from_runs(&results).unwrap();

        assert_eq!(summary.backend, "test");
        assert_eq!(summary.workload, "ycsb_a");
        assert_eq!(summary.workers, 4);
        assert_eq!(summary.runs, 3);
        // Median of [1000, 1100, 1200] is 1100
        assert!((summary.median_throughput - 1100.0).abs() < 0.01);
        // Median of [90, 100, 110] is 100
        assert_eq!(summary.median_p50, 100);
        // Median of [450, 500, 550] is 500
        assert_eq!(summary.median_p99, 500);
        // Total aborts: 10 + 5 + 15 = 30
        assert_eq!(summary.total_aborts, 30);
        assert_eq!(summary.median_cold_open_us, Some(1000));
        assert_eq!(summary.median_first_read_us, Some(100));
    }

    #[test]
    fn jsonl_writer_appends_results() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("results.jsonl");

        let result = RunResult {
            seed: 12345,
            run_id: 0,
            backend: "crackeddb".to_string(),
            workload: "ycsb_a".to_string(),
            workers: 16,
            record_count: 10000,
            operation_count: 10000,
            warmup_secs: 60.0,
            measurement_secs: 120.0,
            throughput_ops_sec: 5000.0,
            latency_us: LatencyMetrics {
                p50: 100,
                p90: 200,
                p99: 500,
                p999: 1000,
                max: 2000,
                mean: 150.0,
            },
            per_worker_latency_us: vec![],
            commits_success: 9500,
            commits_aborted: 500,
            per_worker_aborts: vec![],
            write_amp: Some(2.5),
            space_amp: 1.2,
            cold_open_us: Some(5000),
            first_read_us: Some(50),
        };

        {
            let mut writer = JsonlWriter::open(&path).unwrap();
            writer.write_result(&result).unwrap();
            writer.write_result(&result).unwrap();
        }

        // Verify file has two lines
        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2);

        // Verify each line is valid JSON
        for line in lines {
            let parsed: RunResult = serde_json::from_str(line).unwrap();
            assert_eq!(parsed.backend, "crackeddb");
            assert_eq!(parsed.seed, 12345);
        }
    }
}
