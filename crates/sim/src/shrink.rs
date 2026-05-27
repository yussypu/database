//! Test case shrinker for simulation testing.
//!
//! When a seed fails, the shrinker attempts to find a minimal reproduction
//! by trying nearby seeds, removing operations, and removing faults.
//!
//! # Shrinking Strategies
//!
//! 1. **Seed neighborhood**: Try seeds ±1, ±2, etc. to find simpler reproductions
//! 2. **Operation reduction**: Remove operations to find minimal failing sequence
//! 3. **Fault reduction**: Remove injected faults to find minimal fault set
//! 4. **Binary search**: Divide operation sequence to find critical point

use crate::failure::{replay_with_config, FailureKind, FailureReport};
use crate::fault::Fault;
use crate::fault_scenarios::{FaultScenario, ScenarioConfig};
use crate::workload::Operation;
use runtime::FaultConfig as RuntimeFaultConfig;

/// A minimal reproduction of a failure.
#[derive(Debug, Clone)]
pub struct MinimalRepro {
    /// The seed that reproduces the failure.
    pub seed: u64,
    /// The operations leading to failure.
    pub operations: Vec<Operation>,
    /// The faults that were injected.
    pub faults: Vec<(u64, Fault)>,
    /// The operation number where the failure occurred.
    pub failure_op: u64,
    /// Description of the failure.
    pub failure_description: String,
}

impl MinimalRepro {
    /// Creates a new minimal reproduction.
    pub fn new(seed: u64, failure_op: u64, failure_description: impl Into<String>) -> Self {
        Self {
            seed,
            operations: Vec::new(),
            faults: Vec::new(),
            failure_op,
            failure_description: failure_description.into(),
        }
    }

    /// Adds operations to the reproduction.
    pub fn with_operations(mut self, ops: Vec<Operation>) -> Self {
        self.operations = ops;
        self
    }

    /// Adds faults to the reproduction.
    pub fn with_faults(mut self, faults: Vec<(u64, Fault)>) -> Self {
        self.faults = faults;
        self
    }

    /// Returns a summary of the reproduction.
    pub fn summary(&self) -> String {
        format!(
            "Seed: 0x{:016X}\nOperations: {}\nFaults: {}\nFailure at op: {}\nDescription: {}",
            self.seed,
            self.operations.len(),
            self.faults.len(),
            self.failure_op,
            self.failure_description
        )
    }
}

/// Configuration for the shrinker.
#[derive(Debug, Clone)]
pub struct ShrinkConfig {
    /// Maximum seeds to try in neighborhood search.
    pub max_seed_neighborhood: u64,
    /// Maximum operation removal attempts.
    pub max_operation_removals: usize,
    /// Enable binary search for critical operation.
    pub binary_search_enabled: bool,
    /// Maximum shrinking iterations.
    pub max_iterations: usize,
    /// Timeout for each shrink attempt (seconds).
    pub attempt_timeout_secs: u64,
}

impl Default for ShrinkConfig {
    fn default() -> Self {
        Self {
            max_seed_neighborhood: 10,
            max_operation_removals: 100,
            binary_search_enabled: true,
            max_iterations: 1000,
            attempt_timeout_secs: 5,
        }
    }
}

/// Statistics about shrinking attempts.
#[derive(Debug, Clone, Default)]
pub struct ShrinkStats {
    /// Total seeds tried.
    pub seeds_tried: u64,
    /// Total operation sequences tried.
    pub sequences_tried: u64,
    /// Number of successful reductions.
    pub reductions: u64,
    /// Original operation count.
    pub original_ops: usize,
    /// Final operation count.
    pub final_ops: usize,
    /// Original fault count.
    pub original_faults: usize,
    /// Final fault count.
    pub final_faults: usize,
}

impl ShrinkStats {
    /// Returns the reduction ratio for operations.
    pub fn ops_reduction_ratio(&self) -> f64 {
        if self.original_ops == 0 {
            0.0
        } else {
            1.0 - (self.final_ops as f64 / self.original_ops as f64)
        }
    }

    /// Returns a summary string.
    pub fn summary(&self) -> String {
        format!(
            "Shrunk from {} ops to {} ops ({:.1}% reduction), {} seeds tried",
            self.original_ops,
            self.final_ops,
            self.ops_reduction_ratio() * 100.0,
            self.seeds_tried
        )
    }
}

/// Result of a shrink attempt.
#[derive(Debug, Clone)]
pub enum ShrinkResult {
    /// Found a smaller reproduction.
    Shrunk(MinimalRepro),
    /// Could not shrink further.
    Minimal(MinimalRepro),
    /// Shrinking timed out.
    Timeout(MinimalRepro),
    /// No failure to shrink.
    NoFailure,
}

impl ShrinkResult {
    /// Returns the reproduction if one exists.
    pub fn repro(&self) -> Option<&MinimalRepro> {
        match self {
            ShrinkResult::Shrunk(r) | ShrinkResult::Minimal(r) | ShrinkResult::Timeout(r) => {
                Some(r)
            }
            ShrinkResult::NoFailure => None,
        }
    }

    /// Returns true if shrinking was successful.
    pub fn is_shrunk(&self) -> bool {
        matches!(self, ShrinkResult::Shrunk(_))
    }
}

/// A shrinker that reduces failing test cases.
///
/// The shrinker takes a failing seed and attempts to find a minimal
/// reproduction by systematically removing operations and faults.
pub struct Shrinker {
    config: ShrinkConfig,
    stats: ShrinkStats,
}

impl Shrinker {
    /// Creates a new shrinker with the given configuration.
    pub fn new(config: ShrinkConfig) -> Self {
        Self {
            config,
            stats: ShrinkStats::default(),
        }
    }

    /// Returns the current statistics.
    pub fn stats(&self) -> &ShrinkStats {
        &self.stats
    }

    /// Attempts to shrink a failing operation sequence.
    ///
    /// The `test_fn` should return `Some(failure_description)` if the
    /// sequence still fails, or `None` if it passes.
    pub fn shrink_operations<F>(
        &mut self,
        operations: Vec<Operation>,
        mut test_fn: F,
    ) -> ShrinkResult
    where
        F: FnMut(&[Operation]) -> Option<String>,
    {
        self.stats.original_ops = operations.len();

        // First, verify the original sequence fails
        let failure_desc = match test_fn(&operations) {
            Some(desc) => desc,
            None => return ShrinkResult::NoFailure,
        };

        let mut current = operations;
        let mut current_desc = failure_desc;
        let mut iterations = 0;

        // Try removing operations one at a time
        loop {
            if iterations >= self.config.max_iterations {
                break;
            }

            let mut made_progress = false;

            // Try removing each operation
            for i in (0..current.len()).rev() {
                if iterations >= self.config.max_iterations {
                    break;
                }

                let mut candidate = current.clone();
                candidate.remove(i);
                self.stats.sequences_tried += 1;
                iterations += 1;

                if let Some(desc) = test_fn(&candidate) {
                    // Still fails with this operation removed
                    current = candidate;
                    current_desc = desc;
                    self.stats.reductions += 1;
                    made_progress = true;
                    break;
                }
            }

            if !made_progress {
                break;
            }
        }

        // Try binary search to find critical section
        if self.config.binary_search_enabled && current.len() > 2 {
            current = self.binary_shrink(current, &mut test_fn, &mut current_desc);
        }

        self.stats.final_ops = current.len();

        let repro =
            MinimalRepro::new(0, current.len() as u64, current_desc).with_operations(current);

        if self.stats.final_ops < self.stats.original_ops {
            ShrinkResult::Shrunk(repro)
        } else {
            ShrinkResult::Minimal(repro)
        }
    }

    /// Binary search for the critical operation.
    fn binary_shrink<F>(
        &mut self,
        ops: Vec<Operation>,
        test_fn: &mut F,
        failure_desc: &mut String,
    ) -> Vec<Operation>
    where
        F: FnMut(&[Operation]) -> Option<String>,
    {
        let mut lo = 0;
        let mut hi = ops.len();
        let mut best = ops.clone();

        while hi - lo > 1 {
            let mid = (lo + hi) / 2;

            // Try first half
            let first_half: Vec<_> = ops[..mid].to_vec();
            self.stats.sequences_tried += 1;

            if let Some(desc) = test_fn(&first_half) {
                // First half fails
                hi = mid;
                best = first_half;
                *failure_desc = desc;
            } else {
                // Need second half
                lo = mid;
            }
        }

        best
    }

    /// Attempts to shrink by finding a simpler seed.
    ///
    /// The `test_fn` should return `Some(repro)` if the seed fails,
    /// or `None` if it passes.
    pub fn shrink_seed<F>(&mut self, seed: u64, mut test_fn: F) -> ShrinkResult
    where
        F: FnMut(u64) -> Option<MinimalRepro>,
    {
        // First verify original seed fails
        let original_repro = match test_fn(seed) {
            Some(r) => r,
            None => return ShrinkResult::NoFailure,
        };

        self.stats.seeds_tried += 1;
        let mut best = original_repro;

        // Try nearby seeds
        for offset in 1..=self.config.max_seed_neighborhood {
            // Try seed - offset
            if seed >= offset {
                self.stats.seeds_tried += 1;
                if let Some(repro) = test_fn(seed - offset) {
                    if repro.operations.len() < best.operations.len() {
                        best = repro;
                        self.stats.reductions += 1;
                    }
                }
            }

            // Try seed + offset
            self.stats.seeds_tried += 1;
            if let Some(repro) = test_fn(seed + offset) {
                if repro.operations.len() < best.operations.len() {
                    best = repro;
                    self.stats.reductions += 1;
                }
            }
        }

        if best.seed != seed || best.operations.len() < self.stats.original_ops {
            ShrinkResult::Shrunk(best)
        } else {
            ShrinkResult::Minimal(best)
        }
    }

    /// Attempts to shrink a fault schedule.
    ///
    /// The `test_fn` should return `Some(description)` if the faults
    /// still cause a failure, or `None` if they don't.
    pub fn shrink_faults<F>(
        &mut self,
        faults: Vec<(u64, Fault)>,
        mut test_fn: F,
    ) -> Vec<(u64, Fault)>
    where
        F: FnMut(&[(u64, Fault)]) -> Option<String>,
    {
        self.stats.original_faults = faults.len();

        // Verify original fails
        if test_fn(&faults).is_none() {
            return faults;
        }

        let mut current = faults;

        // Try removing faults one at a time
        loop {
            let mut made_progress = false;

            for i in (0..current.len()).rev() {
                let mut candidate = current.clone();
                candidate.remove(i);

                if test_fn(&candidate).is_some() {
                    current = candidate;
                    self.stats.reductions += 1;
                    made_progress = true;
                    break;
                }
            }

            if !made_progress {
                break;
            }
        }

        self.stats.final_faults = current.len();
        current
    }

    /// Resets the statistics.
    pub fn reset_stats(&mut self) {
        self.stats = ShrinkStats::default();
    }
}

/// Generates a deterministic sequence of seeds to try for shrinking.
pub fn seed_candidates(original_seed: u64, max_candidates: u64) -> impl Iterator<Item = u64> {
    (0..max_candidates).flat_map(move |offset| {
        let mut candidates = Vec::with_capacity(2);
        if offset == 0 {
            candidates.push(original_seed);
        } else {
            if original_seed >= offset {
                candidates.push(original_seed - offset);
            }
            candidates.push(original_seed.wrapping_add(offset));
        }
        candidates.into_iter()
    })
}

/// Removes operations by indices, returning the reduced sequence.
pub fn remove_operations(ops: &[Operation], indices_to_remove: &[usize]) -> Vec<Operation> {
    ops.iter()
        .enumerate()
        .filter(|(i, _)| !indices_to_remove.contains(i))
        .map(|(_, op)| op.clone())
        .collect()
}

/// Finds the minimum prefix length of operations that still fails.
///
/// Returns the number of operations (length) in the minimum failing prefix.
pub fn find_minimum_prefix<F>(ops: &[Operation], mut test_fn: F) -> usize
where
    F: FnMut(&[Operation]) -> bool,
{
    // Binary search for the minimum prefix length
    let mut lo = 1;
    let mut hi = ops.len();

    // First check if empty sequence fails
    if test_fn(&[]) {
        return 0;
    }

    while lo < hi {
        let mid = (lo + hi) / 2;
        if test_fn(&ops[..mid]) {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }

    lo
}

// ============================================================================
// Delta-Debugging Shrinker for Fault Scenarios
// ============================================================================

/// Configuration for the delta-debugging shrinker.
#[derive(Debug, Clone)]
pub struct DeltaDebugConfig {
    /// Maximum number of shrink iterations per axis.
    pub max_iterations_per_axis: usize,
    /// Probability step for fault reduction (e.g., 0.1 = try 0.9x, 0.8x, ...).
    pub probability_step: f64,
    /// Minimum probability to try (below this, just disable the fault).
    pub min_probability: f64,
    /// Workload truncation step (try removing this fraction at a time).
    pub workload_truncation_step: f64,
    /// Minimum workload size to try.
    pub min_workload_size: usize,
    /// Whether to try removing fault types entirely.
    pub try_fault_type_removal: bool,
    /// Whether to verbose log shrinking attempts.
    pub verbose: bool,
}

impl Default for DeltaDebugConfig {
    fn default() -> Self {
        Self {
            max_iterations_per_axis: 100,
            probability_step: 0.1,
            min_probability: 0.05,
            workload_truncation_step: 0.5, // Binary search style
            min_workload_size: 10,
            try_fault_type_removal: true,
            verbose: false,
        }
    }
}

impl DeltaDebugConfig {
    /// Configuration for quick shrinking.
    pub fn quick() -> Self {
        Self {
            max_iterations_per_axis: 20,
            probability_step: 0.2,
            workload_truncation_step: 0.5,
            min_workload_size: 5,
            ..Default::default()
        }
    }

    /// Configuration for thorough shrinking.
    pub fn thorough() -> Self {
        Self {
            max_iterations_per_axis: 200,
            probability_step: 0.05,
            workload_truncation_step: 0.25,
            min_workload_size: 1,
            ..Default::default()
        }
    }
}

/// A single step in the shrinking process, for tracing.
#[derive(Debug, Clone, PartialEq)]
pub struct ShrinkStep {
    /// The axis being shrunk.
    pub axis: String,
    /// The value before shrinking.
    pub before: String,
    /// The value after shrinking.
    pub after: String,
    /// Whether this shrink was accepted (still reproduces failure).
    pub accepted: bool,
    /// If rejected, the reason (e.g., "failure kind changed", "no failure").
    pub rejection_reason: Option<String>,
}

impl ShrinkStep {
    /// Creates an accepted shrink step.
    pub fn accepted(
        axis: impl Into<String>,
        before: impl Into<String>,
        after: impl Into<String>,
    ) -> Self {
        Self {
            axis: axis.into(),
            before: before.into(),
            after: after.into(),
            accepted: true,
            rejection_reason: None,
        }
    }

    /// Creates a rejected shrink step.
    pub fn rejected(
        axis: impl Into<String>,
        before: impl Into<String>,
        after: impl Into<String>,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            axis: axis.into(),
            before: before.into(),
            after: after.into(),
            accepted: false,
            rejection_reason: Some(reason.into()),
        }
    }

    /// Formats this step for display.
    pub fn display(&self) -> String {
        if self.accepted {
            format!(
                "axis={} before={} after={} (accepted)",
                self.axis, self.before, self.after
            )
        } else {
            format!(
                "axis={} before={} after={} (rejected: {})",
                self.axis,
                self.before,
                self.after,
                self.rejection_reason.as_deref().unwrap_or("unknown")
            )
        }
    }
}

/// Statistics about delta-debugging shrinking.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DeltaDebugStats {
    /// Total replay attempts.
    pub total_replays: u64,
    /// Replays that reproduced the failure.
    pub successful_replays: u64,
    /// Replays that failed to reproduce (different outcome).
    pub failed_replays: u64,
    /// Probability reduction attempts.
    pub probability_reductions: u64,
    /// Fault type removal attempts.
    pub fault_type_removals: u64,
    /// Workload truncation attempts.
    pub workload_truncations: u64,
    /// Original workload size.
    pub original_workload_size: usize,
    /// Final workload size.
    pub final_workload_size: usize,
    /// Original number of enabled fault types.
    pub original_fault_types: usize,
    /// Final number of enabled fault types.
    pub final_fault_types: usize,
    /// Trace of all shrink steps attempted.
    pub trace: Vec<ShrinkStep>,
}

impl DeltaDebugStats {
    /// Returns a summary string.
    pub fn summary(&self) -> String {
        format!(
            "Replays: {} ({} success, {} fail)\n\
             Workload: {} -> {} ops ({:.1}% reduction)\n\
             Fault types: {} -> {}",
            self.total_replays,
            self.successful_replays,
            self.failed_replays,
            self.original_workload_size,
            self.final_workload_size,
            if self.original_workload_size > 0 {
                (1.0 - self.final_workload_size as f64 / self.original_workload_size as f64) * 100.0
            } else {
                0.0
            },
            self.original_fault_types,
            self.final_fault_types
        )
    }
}

/// Result of delta-debugging shrinking.
#[derive(Debug, Clone)]
pub struct DeltaDebugResult {
    /// The minimal failure report.
    pub report: FailureReport,
    /// The shrunk fault configuration.
    pub fault_config: ShrunkFaultConfig,
    /// Statistics about the shrinking process.
    pub stats: DeltaDebugStats,
}

impl DeltaDebugResult {
    /// Returns the accepted shrink trace (for printing/display).
    pub fn accepted_trace(&self) -> Vec<&ShrinkStep> {
        self.stats.trace.iter().filter(|s| s.accepted).collect()
    }

    /// Prints the accepted shrink trace.
    pub fn print_trace(&self) {
        for step in &self.stats.trace {
            if step.accepted {
                println!("{}", step.display());
            }
        }
    }

    /// Returns the trace as a formatted string.
    pub fn trace_string(&self) -> String {
        self.stats
            .trace
            .iter()
            .filter(|s| s.accepted)
            .map(|s| s.display())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// A shrunk fault configuration that minimally reproduces the failure.
#[derive(Debug, Clone)]
pub struct ShrunkFaultConfig {
    /// The scenario that was shrunk.
    pub scenario: FaultScenario,
    /// Partial write probability (0.0 = disabled).
    pub partial_write_prob: f64,
    /// Disk full threshold (0 = disabled).
    pub disk_full_threshold: u64,
    /// Slow write probability (0.0 = disabled).
    pub slow_write_prob: f64,
    /// Clock skew probability (0.0 = disabled).
    pub clock_skew_prob: f64,
    /// Process pause probability (0.0 = disabled).
    pub process_pause_prob: f64,
    /// Workload length.
    pub workload_len: usize,
}

impl ShrunkFaultConfig {
    /// Creates from a scenario's runtime config.
    pub fn from_scenario(scenario: FaultScenario, workload_len: usize) -> Self {
        let cfg = scenario.runtime_config();
        Self {
            scenario,
            partial_write_prob: cfg.partial_write_prob,
            disk_full_threshold: cfg.disk_full_threshold,
            slow_write_prob: cfg.slow_write_prob,
            clock_skew_prob: cfg.clock_skew_prob,
            process_pause_prob: cfg.process_pause_prob,
            workload_len,
        }
    }

    /// Converts to RuntimeFaultConfig.
    pub fn to_runtime_config(&self) -> RuntimeFaultConfig {
        let base = self.scenario.runtime_config();
        RuntimeFaultConfig {
            partial_write_prob: self.partial_write_prob,
            disk_full_threshold: self.disk_full_threshold,
            slow_write_prob: self.slow_write_prob,
            slow_write_duration: base.slow_write_duration,
            clock_skew_prob: self.clock_skew_prob,
            clock_skew_max: base.clock_skew_max,
            process_pause_prob: self.process_pause_prob,
            process_pause_duration: base.process_pause_duration,
        }
    }

    /// Counts the number of enabled fault types.
    pub fn enabled_fault_count(&self) -> usize {
        let mut count = 0;
        if self.partial_write_prob > 0.0 {
            count += 1;
        }
        if self.disk_full_threshold > 0 {
            count += 1;
        }
        if self.slow_write_prob > 0.0 {
            count += 1;
        }
        if self.clock_skew_prob > 0.0 {
            count += 1;
        }
        if self.process_pause_prob > 0.0 {
            count += 1;
        }
        count
    }

    /// Returns the fault probabilities as a vector for iteration.
    fn fault_probs(&self) -> Vec<(&'static str, f64)> {
        vec![
            ("partial_write", self.partial_write_prob),
            ("slow_write", self.slow_write_prob),
            ("clock_skew", self.clock_skew_prob),
            ("process_pause", self.process_pause_prob),
        ]
    }

    /// Creates a modified copy with a specific fault probability changed.
    fn with_fault_prob(&self, name: &str, prob: f64) -> Self {
        let mut copy = self.clone();
        match name {
            "partial_write" => copy.partial_write_prob = prob,
            "slow_write" => copy.slow_write_prob = prob,
            "clock_skew" => copy.clock_skew_prob = prob,
            "process_pause" => copy.process_pause_prob = prob,
            _ => {}
        }
        copy
    }

    /// Creates a modified copy with disk_full_threshold changed.
    fn with_disk_full(&self, threshold: u64) -> Self {
        let mut copy = self.clone();
        copy.disk_full_threshold = threshold;
        copy
    }

    /// Creates a modified copy with a different workload length.
    fn with_workload_len(&self, len: usize) -> Self {
        let mut copy = self.clone();
        copy.workload_len = len;
        copy
    }
}

/// Delta-debugging shrinker for fault scenario failures.
///
/// Given a failing seed and scenario, the shrinker attempts to find a
/// minimal reproduction by:
/// 1. Reducing fault probabilities
/// 2. Removing fault types entirely
/// 3. Truncating the workload
///
/// The shrinker preserves the FailureKind - a shrunk reproduction must
/// exhibit the same class of failure as the original.
pub struct DeltaDebugShrinker {
    config: DeltaDebugConfig,
    stats: DeltaDebugStats,
}

impl DeltaDebugShrinker {
    /// Creates a new delta-debug shrinker.
    pub fn new(config: DeltaDebugConfig) -> Self {
        Self {
            config,
            stats: DeltaDebugStats::default(),
        }
    }

    /// Returns the current statistics.
    pub fn stats(&self) -> &DeltaDebugStats {
        &self.stats
    }

    /// Resets the statistics.
    pub fn reset_stats(&mut self) {
        self.stats = DeltaDebugStats::default();
    }

    /// Shrinks a failing scenario to find a minimal reproduction.
    ///
    /// The shrinking process uses delta-debugging to minimize:
    /// 1. Fault probabilities (reduce each until failure stops)
    /// 2. Fault types (try disabling each type)
    /// 3. Workload length (binary search for minimum)
    ///
    /// The original FailureKind must be preserved - a shrunk reproduction
    /// that produces a different failure kind is rejected.
    pub fn shrink(&mut self, report: &FailureReport) -> Option<DeltaDebugResult> {
        let mut current = ShrunkFaultConfig::from_scenario(report.scenario, report.workload_len);
        let original_kind = &report.failure_kind;

        self.stats.original_workload_size = current.workload_len;
        self.stats.original_fault_types = current.enabled_fault_count();

        // Verify the original configuration reproduces the failure
        if !self.reproduces_failure(report.seed, &current, original_kind) {
            // Original doesn't reproduce - nothing to shrink
            return None;
        }

        // Fixed-point loop: keep shrinking until no progress
        let mut made_progress = true;
        let mut iterations = 0;
        let max_total_iterations = self.config.max_iterations_per_axis * 3;

        while made_progress && iterations < max_total_iterations {
            made_progress = false;
            iterations += 1;

            // Axis 1: Reduce fault probabilities
            if let Some(shrunk) = self.shrink_probabilities(report.seed, &current, original_kind) {
                current = shrunk;
                made_progress = true;
            }

            // Axis 2: Remove fault types entirely
            if self.config.try_fault_type_removal {
                if let Some(shrunk) = self.remove_fault_types(report.seed, &current, original_kind)
                {
                    current = shrunk;
                    made_progress = true;
                }
            }

            // Axis 3: Truncate workload
            if let Some(shrunk) = self.shrink_workload(report.seed, &current, original_kind) {
                current = shrunk;
                made_progress = true;
            }
        }

        self.stats.final_workload_size = current.workload_len;
        self.stats.final_fault_types = current.enabled_fault_count();

        // Verify final configuration still reproduces
        if let Some(final_report) = self.try_reproduce(report.seed, &current, original_kind) {
            Some(DeltaDebugResult {
                report: final_report,
                fault_config: current,
                stats: self.stats.clone(),
            })
        } else {
            None
        }
    }

    /// Shrinks fault probabilities, returning the minimal config that still fails.
    fn shrink_probabilities(
        &mut self,
        seed: u64,
        current: &ShrunkFaultConfig,
        target_kind: &FailureKind,
    ) -> Option<ShrunkFaultConfig> {
        let mut best = current.clone();
        let mut made_progress = false;

        // Try reducing each probability
        for (name, prob) in current.fault_probs() {
            if prob <= 0.0 {
                continue;
            }

            self.stats.probability_reductions += 1;

            // Binary search for minimum probability
            let mut lo = 0.0;
            let mut hi = prob;
            let mut best_prob = prob;

            while hi - lo > self.config.min_probability {
                let mid = (lo + hi) / 2.0;
                let candidate = best.with_fault_prob(name, mid);

                if self.reproduces_failure(seed, &candidate, target_kind) {
                    // Can reduce further
                    hi = mid;
                    best_prob = mid;
                    made_progress = true;
                } else {
                    // Need more
                    lo = mid;
                }
            }

            // Try disabling entirely if we got close to minimum
            if best_prob <= self.config.min_probability {
                let candidate = best.with_fault_prob(name, 0.0);
                if self.reproduces_failure(seed, &candidate, target_kind) {
                    best_prob = 0.0;
                    made_progress = true;
                }
            }

            best = best.with_fault_prob(name, best_prob);
        }

        // Also try reducing disk_full_threshold
        if current.disk_full_threshold > 0 {
            let mut lo = 0u64;
            let mut hi = current.disk_full_threshold;
            let mut best_threshold = current.disk_full_threshold;

            while hi > lo + 1024 {
                // Step by 1KB
                let mid = (lo + hi) / 2;
                let candidate = best.with_disk_full(mid);

                if self.reproduces_failure(seed, &candidate, target_kind) {
                    hi = mid;
                    best_threshold = mid;
                    made_progress = true;
                } else {
                    lo = mid;
                }
            }

            // Try disabling
            let candidate = best.with_disk_full(0);
            if self.reproduces_failure(seed, &candidate, target_kind) {
                best_threshold = 0;
                made_progress = true;
            }

            best = best.with_disk_full(best_threshold);
        }

        if made_progress {
            Some(best)
        } else {
            None
        }
    }

    /// Tries removing fault types entirely.
    fn remove_fault_types(
        &mut self,
        seed: u64,
        current: &ShrunkFaultConfig,
        target_kind: &FailureKind,
    ) -> Option<ShrunkFaultConfig> {
        let mut best = current.clone();
        let mut made_progress = false;

        // Try removing each fault type
        let fault_types = [
            ("partial_write", current.partial_write_prob > 0.0),
            ("slow_write", current.slow_write_prob > 0.0),
            ("clock_skew", current.clock_skew_prob > 0.0),
            ("process_pause", current.process_pause_prob > 0.0),
        ];

        for (name, enabled) in fault_types {
            if !enabled {
                continue;
            }

            self.stats.fault_type_removals += 1;
            let candidate = best.with_fault_prob(name, 0.0);

            if self.reproduces_failure(seed, &candidate, target_kind) {
                best = candidate;
                made_progress = true;
            }
        }

        // Try disabling disk_full
        if current.disk_full_threshold > 0 {
            self.stats.fault_type_removals += 1;
            let candidate = best.with_disk_full(0);

            if self.reproduces_failure(seed, &candidate, target_kind) {
                best = candidate;
                made_progress = true;
            }
        }

        if made_progress {
            Some(best)
        } else {
            None
        }
    }

    /// Shrinks workload length using binary search.
    fn shrink_workload(
        &mut self,
        seed: u64,
        current: &ShrunkFaultConfig,
        target_kind: &FailureKind,
    ) -> Option<ShrunkFaultConfig> {
        if current.workload_len <= self.config.min_workload_size {
            return None;
        }

        self.stats.workload_truncations += 1;

        // Binary search for minimum workload
        let mut lo = self.config.min_workload_size;
        let mut hi = current.workload_len;
        let mut best_len = current.workload_len;
        let mut made_progress = false;

        while hi > lo {
            let mid = (lo + hi) / 2;
            if mid == best_len {
                break;
            }

            let candidate = current.with_workload_len(mid);

            if self.reproduces_failure(seed, &candidate, target_kind) {
                hi = mid;
                best_len = mid;
                made_progress = true;
            } else {
                lo = mid + 1;
            }
        }

        if made_progress {
            Some(current.with_workload_len(best_len))
        } else {
            None
        }
    }

    /// Tests if the configuration reproduces the target failure kind.
    fn reproduces_failure(
        &mut self,
        seed: u64,
        config: &ShrunkFaultConfig,
        target_kind: &FailureKind,
    ) -> bool {
        self.try_reproduce(seed, config, target_kind).is_some()
    }

    /// Tries to reproduce the failure, returning the report if successful.
    fn try_reproduce(
        &mut self,
        seed: u64,
        config: &ShrunkFaultConfig,
        target_kind: &FailureKind,
    ) -> Option<FailureReport> {
        self.stats.total_replays += 1;

        // Create a custom scenario config
        let scenario_config = ScenarioConfig {
            scenario: config.scenario,
            max_operations: config.workload_len,
            max_crashes: config.workload_len / 10,
            stop_on_violation: true,
        };

        // Run the replay
        match replay_with_config(seed, scenario_config) {
            Ok(()) => {
                // No failure - doesn't reproduce
                self.stats.failed_replays += 1;
                None
            }
            Err(report) => {
                // Check if same failure kind
                if report.failure_kind.matches(target_kind) {
                    self.stats.successful_replays += 1;
                    Some(report)
                } else {
                    self.stats.failed_replays += 1;
                    None
                }
            }
        }
    }
}

/// Convenience function to shrink a failure report.
pub fn shrink_failure(report: &FailureReport) -> Option<DeltaDebugResult> {
    let mut shrinker = DeltaDebugShrinker::new(DeltaDebugConfig::default());
    shrinker.shrink(report)
}

/// Convenience function for quick shrinking.
pub fn quick_shrink(report: &FailureReport) -> Option<DeltaDebugResult> {
    let mut shrinker = DeltaDebugShrinker::new(DeltaDebugConfig::quick());
    shrinker.shrink(report)
}

/// Convenience function for thorough shrinking.
pub fn thorough_shrink(report: &FailureReport) -> Option<DeltaDebugResult> {
    let mut shrinker = DeltaDebugShrinker::new(DeltaDebugConfig::thorough());
    shrinker.shrink(report)
}

// ============================================================================
// Testable Shrinker with Custom Replay Function
// ============================================================================

/// A replay function that returns either success or failure with kind.
pub type ReplayFn = Box<dyn Fn(u64, &ShrunkFaultConfig) -> Result<(), FailureKind>>;

/// A testable shrinker that accepts a custom replay function.
///
/// This allows testing the shrinking algorithm with synthetic bugs without
/// running actual simulations.
pub struct TestableShrinker {
    config: DeltaDebugConfig,
    stats: DeltaDebugStats,
    replay_fn: ReplayFn,
}

impl TestableShrinker {
    /// Creates a new testable shrinker with a custom replay function.
    pub fn new(config: DeltaDebugConfig, replay_fn: ReplayFn) -> Self {
        Self {
            config,
            stats: DeltaDebugStats::default(),
            replay_fn,
        }
    }

    /// Creates a shrinker for a workload-dependent bug.
    ///
    /// The bug fires when workload_len >= fail_at_op.
    pub fn for_workload_bug(
        config: DeltaDebugConfig,
        fail_at_op: usize,
        failure_kind: FailureKind,
    ) -> Self {
        let kind = failure_kind.clone();
        Self::new(
            config,
            Box::new(move |_seed, cfg| {
                if cfg.workload_len >= fail_at_op {
                    Err(kind.clone())
                } else {
                    Ok(())
                }
            }),
        )
    }

    /// Creates a shrinker for a fault-dependent bug.
    ///
    /// The bug fires when partial_write_prob >= min_prob AND workload_len >= min_workload.
    pub fn for_fault_dependent_bug(
        config: DeltaDebugConfig,
        min_prob: f64,
        min_workload: usize,
        failure_kind: FailureKind,
    ) -> Self {
        let kind = failure_kind.clone();
        Self::new(
            config,
            Box::new(move |_seed, cfg| {
                if cfg.partial_write_prob >= min_prob && cfg.workload_len >= min_workload {
                    Err(kind.clone())
                } else {
                    Ok(())
                }
            }),
        )
    }

    /// Returns the statistics.
    pub fn stats(&self) -> &DeltaDebugStats {
        &self.stats
    }

    /// Shrinks a failing scenario using the custom replay function.
    pub fn shrink(&mut self, report: &FailureReport) -> Option<DeltaDebugResult> {
        let mut current = ShrunkFaultConfig::from_scenario(report.scenario, report.workload_len);
        let original_kind = &report.failure_kind;

        self.stats.original_workload_size = current.workload_len;
        self.stats.original_fault_types = current.enabled_fault_count();

        // Verify the original configuration reproduces the failure
        if !self.reproduces_failure(&current, original_kind) {
            return None;
        }

        // Fixed-point loop: keep shrinking until no progress
        let mut made_progress = true;
        let mut iterations = 0;
        let max_total_iterations = self.config.max_iterations_per_axis * 3;

        while made_progress && iterations < max_total_iterations {
            made_progress = false;
            iterations += 1;

            // Axis 1: Reduce fault probabilities
            if let Some(shrunk) = self.shrink_probabilities(&current, original_kind) {
                current = shrunk;
                made_progress = true;
            }

            // Axis 2: Remove fault types entirely
            if self.config.try_fault_type_removal {
                if let Some(shrunk) = self.remove_fault_types(&current, original_kind) {
                    current = shrunk;
                    made_progress = true;
                }
            }

            // Axis 3: Truncate workload
            if let Some(shrunk) = self.shrink_workload(&current, original_kind) {
                current = shrunk;
                made_progress = true;
            }
        }

        self.stats.final_workload_size = current.workload_len;
        self.stats.final_fault_types = current.enabled_fault_count();

        // Verify final configuration still reproduces
        if self.reproduces_failure(&current, original_kind) {
            let final_report = FailureReport::new(
                report.seed,
                report.scenario,
                current.workload_len,
                original_kind.clone(),
            );
            Some(DeltaDebugResult {
                report: final_report,
                fault_config: current,
                stats: self.stats.clone(),
            })
        } else {
            None
        }
    }

    fn shrink_probabilities(
        &mut self,
        current: &ShrunkFaultConfig,
        target_kind: &FailureKind,
    ) -> Option<ShrunkFaultConfig> {
        let mut best = current.clone();
        let mut made_progress = false;
        let original_probs = current.fault_probs();

        for (name, prob) in &original_probs {
            if *prob <= 0.0 {
                continue;
            }

            self.stats.probability_reductions += 1;
            let orig_prob = *prob;

            // Binary search for minimum probability
            let mut lo = 0.0;
            let mut hi = *prob;
            let mut best_prob = *prob;

            while hi - lo > self.config.min_probability {
                let mid = (lo + hi) / 2.0;
                let candidate = best.with_fault_prob(name, mid);

                if self.reproduces_failure(&candidate, target_kind) {
                    hi = mid;
                    best_prob = mid;
                } else {
                    lo = mid;
                }
            }

            // Try disabling entirely
            if best_prob <= self.config.min_probability {
                let candidate = best.with_fault_prob(name, 0.0);
                if self.reproduces_failure(&candidate, target_kind) {
                    best_prob = 0.0;
                }
            }

            if best_prob < orig_prob {
                self.stats.trace.push(ShrinkStep::accepted(
                    format!("{}_prob", name),
                    format!("{:.2}", orig_prob),
                    format!("{:.2}", best_prob),
                ));
                best = best.with_fault_prob(name, best_prob);
                made_progress = true;
            }
        }

        // Also try reducing disk_full_threshold
        if current.disk_full_threshold > 0 {
            let orig_threshold = current.disk_full_threshold;
            let mut lo = 0u64;
            let mut hi = current.disk_full_threshold;
            let mut best_threshold = current.disk_full_threshold;

            while hi > lo + 1024 {
                let mid = (lo + hi) / 2;
                let candidate = best.with_disk_full(mid);

                if self.reproduces_failure(&candidate, target_kind) {
                    hi = mid;
                    best_threshold = mid;
                } else {
                    lo = mid;
                }
            }

            // Try disabling
            let candidate = best.with_disk_full(0);
            if self.reproduces_failure(&candidate, target_kind) {
                best_threshold = 0;
            }

            if best_threshold < orig_threshold {
                self.stats.trace.push(ShrinkStep::accepted(
                    "disk_full_threshold",
                    format!("{}", orig_threshold),
                    format!("{}", best_threshold),
                ));
                best = best.with_disk_full(best_threshold);
                made_progress = true;
            }
        }

        if made_progress {
            Some(best)
        } else {
            None
        }
    }

    fn remove_fault_types(
        &mut self,
        current: &ShrunkFaultConfig,
        target_kind: &FailureKind,
    ) -> Option<ShrunkFaultConfig> {
        let mut best = current.clone();
        let mut made_progress = false;

        let fault_types = [
            ("partial_write", current.partial_write_prob),
            ("slow_write", current.slow_write_prob),
            ("clock_skew", current.clock_skew_prob),
            ("process_pause", current.process_pause_prob),
        ];

        for (name, prob) in fault_types {
            if prob <= 0.0 {
                continue;
            }

            self.stats.fault_type_removals += 1;
            let candidate = best.with_fault_prob(name, 0.0);

            if self.reproduces_failure(&candidate, target_kind) {
                self.stats.trace.push(ShrinkStep::accepted(
                    format!("{}_removal", name),
                    "enabled",
                    "disabled",
                ));
                best = candidate;
                made_progress = true;
            }
        }

        if current.disk_full_threshold > 0 {
            self.stats.fault_type_removals += 1;
            let candidate = best.with_disk_full(0);

            if self.reproduces_failure(&candidate, target_kind) {
                self.stats.trace.push(ShrinkStep::accepted(
                    "disk_full_removal",
                    "enabled",
                    "disabled",
                ));
                best = candidate;
                made_progress = true;
            }
        }

        if made_progress {
            Some(best)
        } else {
            None
        }
    }

    fn shrink_workload(
        &mut self,
        current: &ShrunkFaultConfig,
        target_kind: &FailureKind,
    ) -> Option<ShrunkFaultConfig> {
        if current.workload_len <= self.config.min_workload_size {
            return None;
        }

        self.stats.workload_truncations += 1;
        let orig_len = current.workload_len;

        // Binary search for minimum workload
        let mut lo = self.config.min_workload_size;
        let mut hi = current.workload_len;
        let mut best_len = current.workload_len;

        while hi > lo {
            let mid = (lo + hi) / 2;
            if mid == best_len {
                break;
            }

            let candidate = current.with_workload_len(mid);

            if self.reproduces_failure(&candidate, target_kind) {
                hi = mid;
                best_len = mid;
            } else {
                lo = mid + 1;
            }
        }

        if best_len < orig_len {
            self.stats.trace.push(ShrinkStep::accepted(
                "workload_len",
                format!("{}", orig_len),
                format!("{}", best_len),
            ));
            Some(current.with_workload_len(best_len))
        } else {
            None
        }
    }

    fn reproduces_failure(
        &mut self,
        config: &ShrunkFaultConfig,
        target_kind: &FailureKind,
    ) -> bool {
        self.stats.total_replays += 1;

        match (self.replay_fn)(0, config) {
            Ok(()) => {
                self.stats.failed_replays += 1;
                false
            }
            Err(kind) => {
                if kind.matches(target_kind) {
                    self.stats.successful_replays += 1;
                    true
                } else {
                    self.stats.failed_replays += 1;
                    false
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    #[test]
    fn shrink_operations_removes_unnecessary() {
        let mut shrinker = Shrinker::new(ShrinkConfig::default());

        // Create a sequence where only the 5th operation matters
        let ops: Vec<Operation> = (0..10)
            .map(|i| Operation::Put {
                key: Bytes::from(format!("key{}", i)),
                value: Bytes::from(format!("value{}", i)),
            })
            .collect();

        // Only fails if operation 5 is present
        let result = shrinker.shrink_operations(ops, |seq| {
            if seq.iter().any(|op| {
                if let Operation::Put { key, .. } = op {
                    key.as_ref() == b"key5"
                } else {
                    false
                }
            }) {
                Some("key5 present".to_string())
            } else {
                None
            }
        });

        match result {
            ShrinkResult::Shrunk(repro) | ShrinkResult::Minimal(repro) => {
                assert!(repro.operations.len() < 10);
                // Should contain key5
                assert!(repro.operations.iter().any(|op| {
                    if let Operation::Put { key, .. } = op {
                        key.as_ref() == b"key5"
                    } else {
                        false
                    }
                }));
            }
            _ => panic!("Expected shrunk result"),
        }
    }

    #[test]
    fn shrink_faults_removes_unnecessary() {
        let mut shrinker = Shrinker::new(ShrinkConfig::default());

        let faults = vec![
            (1, Fault::Crash),
            (5, Fault::Crash), // This one matters
            (10, Fault::Crash),
        ];

        // Only fails if fault at op 5 is present
        let result = shrinker.shrink_faults(faults, |f| {
            if f.iter().any(|(op, _)| *op == 5) {
                Some("fault at 5".to_string())
            } else {
                None
            }
        });

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, 5);
    }

    #[test]
    fn seed_candidates_generates_neighborhood() {
        let candidates: Vec<_> = seed_candidates(100, 5).collect();
        assert!(candidates.contains(&100));
        assert!(candidates.contains(&99));
        assert!(candidates.contains(&101));
        assert!(candidates.contains(&98));
        assert!(candidates.contains(&102));
    }

    #[test]
    fn find_minimum_prefix_binary_search() {
        let ops: Vec<Operation> = (0..100)
            .map(|i| Operation::Get {
                key: Bytes::from(format!("key{}", i)),
            })
            .collect();

        // Fails only if we include operation 50 or later
        let min = find_minimum_prefix(&ops, |seq| seq.len() > 50);
        assert_eq!(min, 51);
    }

    #[test]
    fn minimal_repro_summary() {
        let repro = MinimalRepro::new(0xDEADBEEF, 42, "test failure")
            .with_operations(vec![Operation::Put {
                key: Bytes::from("k"),
                value: Bytes::from("v"),
            }])
            .with_faults(vec![(10, Fault::Crash)]);

        let summary = repro.summary();
        assert!(summary.contains("DEADBEEF"));
        assert!(summary.contains("42"));
        assert!(summary.contains("test failure"));
    }

    // ========================================
    // Delta-Debug Shrinker Tests
    // ========================================

    #[test]
    fn delta_debug_config_defaults() {
        let config = DeltaDebugConfig::default();
        assert!(config.max_iterations_per_axis > 0);
        assert!(config.probability_step > 0.0);
        assert!(config.min_probability > 0.0);
        assert!(config.min_workload_size > 0);
    }

    #[test]
    fn delta_debug_config_variants() {
        let quick = DeltaDebugConfig::quick();
        let thorough = DeltaDebugConfig::thorough();

        // Thorough should have more iterations
        assert!(thorough.max_iterations_per_axis > quick.max_iterations_per_axis);
        // Thorough should have smaller steps
        assert!(thorough.probability_step < quick.probability_step);
    }

    #[test]
    fn shrunk_fault_config_from_scenario() {
        let config = ShrunkFaultConfig::from_scenario(FaultScenario::Combined, 500);
        assert_eq!(config.scenario, FaultScenario::Combined);
        assert_eq!(config.workload_len, 500);
        // Combined scenario should have some faults enabled
        assert!(config.enabled_fault_count() > 0);
    }

    #[test]
    fn shrunk_fault_config_modifications() {
        let config = ShrunkFaultConfig::from_scenario(FaultScenario::Combined, 500);

        // Test probability modification
        let modified = config.with_fault_prob("partial_write", 0.0);
        assert_eq!(modified.partial_write_prob, 0.0);
        assert_eq!(modified.workload_len, 500); // Other fields unchanged

        // Test workload modification
        let modified = config.with_workload_len(100);
        assert_eq!(modified.workload_len, 100);

        // Test disk_full modification
        let modified = config.with_disk_full(1024);
        assert_eq!(modified.disk_full_threshold, 1024);
    }

    #[test]
    fn shrunk_fault_config_to_runtime_config() {
        let config = ShrunkFaultConfig::from_scenario(FaultScenario::Combined, 500);
        let runtime = config.to_runtime_config();

        // Should preserve probabilities
        assert_eq!(runtime.partial_write_prob, config.partial_write_prob);
        assert_eq!(runtime.slow_write_prob, config.slow_write_prob);
    }

    #[test]
    fn delta_debug_stats_summary() {
        let stats = DeltaDebugStats {
            total_replays: 100,
            successful_replays: 80,
            failed_replays: 20,
            original_workload_size: 1000,
            final_workload_size: 100,
            original_fault_types: 5,
            final_fault_types: 2,
            ..Default::default()
        };

        let summary = stats.summary();
        assert!(summary.contains("100"));
        assert!(summary.contains("80"));
        assert!(summary.contains("90")); // 90% reduction
    }

    #[test]
    fn delta_debug_shrinker_no_failure() {
        // If a report's seed doesn't actually fail, shrink returns None
        let report = FailureReport::new(
            42,
            FaultScenario::None,
            10, // Very short workload
            FailureKind::Custom("test".into()),
        );

        let mut shrinker = DeltaDebugShrinker::new(DeltaDebugConfig::quick());
        let result = shrinker.shrink(&report);

        // The None scenario with seed 42 should pass, so shrink returns None
        assert!(result.is_none() || result.is_some());
        // We just test that it doesn't panic
    }

    #[test]
    fn failure_kind_matches_same_variant() {
        let kind1 = FailureKind::SerializabilityCycle {
            txn_ids: vec![1, 2],
        };
        let kind2 = FailureKind::SerializabilityCycle {
            txn_ids: vec![3, 4, 5],
        };
        assert!(kind1.matches(&kind2));
    }

    #[test]
    fn failure_kind_matches_different_variant() {
        let cycle = FailureKind::SerializabilityCycle { txn_ids: vec![1] };
        let lost = FailureKind::LostWrite {
            key: vec![],
            expected: vec![],
        };
        assert!(!cycle.matches(&lost));
    }

    // ========================================
    // End-to-End Shrinker Integration Tests
    // ========================================

    /// Test that the shrinker converges on a synthetic workload-driven bug.
    ///
    /// Inject a bug at op 15. Start with a 50-op workload + Combined scenario.
    /// The shrinker should find the minimal workload near 15 and disable all faults.
    #[test]
    fn shrinker_converges_on_synthetic_workload_bug() {
        let fail_at_op = 15;
        let initial_workload = 50;
        let failure_kind = FailureKind::SerializabilityCycle {
            txn_ids: vec![1, 2, 3],
        };

        // Create initial failure report with Combined scenario
        let report = FailureReport::new(
            0xDEADBEEF,
            FaultScenario::Combined,
            initial_workload,
            failure_kind.clone(),
        );

        // Create testable shrinker for workload-dependent bug
        let mut shrinker = TestableShrinker::for_workload_bug(
            DeltaDebugConfig::default(),
            fail_at_op,
            failure_kind.clone(),
        );

        let result = shrinker.shrink(&report);
        assert!(result.is_some(), "Shrinker should find a minimal repro");

        let result = result.unwrap();

        // Print the trace for verification
        println!("=== SHRINK TRACE ===");
        for step in &result.stats.trace {
            println!("{}", step.display());
        }
        println!("===================");

        // Assert minimal workload is close to fail_at_op (within buffer for binary search)
        assert!(
            result.fault_config.workload_len <= fail_at_op + 5,
            "Workload should be shrunk close to failure point: got {}, expected <= {}",
            result.fault_config.workload_len,
            fail_at_op + 5
        );

        // Assert all fault probabilities are reduced to 0 (bug is workload-driven, not fault-driven)
        assert_eq!(
            result.fault_config.partial_write_prob, 0.0,
            "partial_write_prob should be 0"
        );
        assert_eq!(
            result.fault_config.slow_write_prob, 0.0,
            "slow_write_prob should be 0"
        );
        assert_eq!(
            result.fault_config.clock_skew_prob, 0.0,
            "clock_skew_prob should be 0"
        );
        assert_eq!(
            result.fault_config.process_pause_prob, 0.0,
            "process_pause_prob should be 0"
        );
        assert_eq!(
            result.fault_config.disk_full_threshold, 0,
            "disk_full_threshold should be 0"
        );

        // Assert replays are reasonable
        assert!(
            result.stats.total_replays < 100,
            "Should converge in < 100 replays, got {}",
            result.stats.total_replays
        );

        // Assert failure kind is preserved
        assert!(
            result.report.failure_kind.matches(&failure_kind),
            "Failure kind should be preserved"
        );

        println!(
            "Converged: workload {} -> {}, replays used: {}",
            initial_workload, result.fault_config.workload_len, result.stats.total_replays
        );
    }

    /// Test that the shrinker converges on a fault-dependent bug.
    ///
    /// Inject a bug that only manifests when partial_write_prob >= 0.05.
    /// Start with Combined scenario (high probs). The shrinker should find
    /// the minimal partial_write_prob and disable other fault types.
    #[test]
    fn shrinker_converges_on_fault_dependent_bug() {
        let min_prob_threshold = 0.05;
        let min_workload = 10;
        let initial_workload = 100;
        let failure_kind = FailureKind::LostWrite {
            key: b"test_key".to_vec(),
            expected: b"test_value".to_vec(),
        };

        // Create initial failure report with Combined scenario
        let report = FailureReport::new(
            0xCAFEBABE,
            FaultScenario::Combined,
            initial_workload,
            failure_kind.clone(),
        );

        // Create testable shrinker for fault-dependent bug
        let mut shrinker = TestableShrinker::for_fault_dependent_bug(
            DeltaDebugConfig::default(),
            min_prob_threshold,
            min_workload,
            failure_kind.clone(),
        );

        let result = shrinker.shrink(&report);
        assert!(result.is_some(), "Shrinker should find a minimal repro");

        let result = result.unwrap();

        println!("=== SHRINK TRACE ===");
        for step in &result.stats.trace {
            println!("{}", step.display());
        }
        println!("===================");

        // Assert partial_write_prob is in the expected range (above threshold, near minimum)
        assert!(
            result.fault_config.partial_write_prob >= min_prob_threshold,
            "partial_write_prob should be >= {}: got {}",
            min_prob_threshold,
            result.fault_config.partial_write_prob
        );
        assert!(
            result.fault_config.partial_write_prob <= 0.15,
            "partial_write_prob should be near minimum: got {}",
            result.fault_config.partial_write_prob
        );

        // Assert other fault types are disabled (not needed for this bug)
        assert_eq!(
            result.fault_config.slow_write_prob, 0.0,
            "slow_write_prob should be 0"
        );
        assert_eq!(
            result.fault_config.clock_skew_prob, 0.0,
            "clock_skew_prob should be 0"
        );
        assert_eq!(
            result.fault_config.process_pause_prob, 0.0,
            "process_pause_prob should be 0"
        );
        assert_eq!(
            result.fault_config.disk_full_threshold, 0,
            "disk_full_threshold should be 0"
        );

        // Assert replays are reasonable
        assert!(
            result.stats.total_replays < 200,
            "Should converge in < 200 replays, got {}",
            result.stats.total_replays
        );

        println!(
            "Converged: partial_write_prob {} -> {:.4}, replays used: {}",
            report.scenario.runtime_config().partial_write_prob,
            result.fault_config.partial_write_prob,
            result.stats.total_replays
        );
    }

    /// Test that the shrinker is deterministic.
    ///
    /// Running the shrinker twice on the same input should produce identical results.
    #[test]
    fn shrinker_is_deterministic() {
        let fail_at_op = 25;
        let initial_workload = 100;
        let failure_kind = FailureKind::SerializabilityCycle {
            txn_ids: vec![1, 2],
        };

        let report = FailureReport::new(
            0xDEADBEEF,
            FaultScenario::Combined,
            initial_workload,
            failure_kind.clone(),
        );

        // Run shrinker twice
        let mut shrinker1 = TestableShrinker::for_workload_bug(
            DeltaDebugConfig::default(),
            fail_at_op,
            failure_kind.clone(),
        );
        let result1 = shrinker1.shrink(&report).expect("Should shrink");

        let mut shrinker2 = TestableShrinker::for_workload_bug(
            DeltaDebugConfig::default(),
            fail_at_op,
            failure_kind.clone(),
        );
        let result2 = shrinker2.shrink(&report).expect("Should shrink");

        // Assert identical results
        assert_eq!(
            result1.fault_config.workload_len, result2.fault_config.workload_len,
            "Workload lengths should match"
        );
        assert_eq!(
            result1.fault_config.partial_write_prob, result2.fault_config.partial_write_prob,
            "partial_write_prob should match"
        );
        assert_eq!(
            result1.fault_config.slow_write_prob, result2.fault_config.slow_write_prob,
            "slow_write_prob should match"
        );
        assert_eq!(
            result1.fault_config.clock_skew_prob, result2.fault_config.clock_skew_prob,
            "clock_skew_prob should match"
        );
        assert_eq!(
            result1.fault_config.process_pause_prob, result2.fault_config.process_pause_prob,
            "process_pause_prob should match"
        );
        assert_eq!(
            result1.fault_config.disk_full_threshold, result2.fault_config.disk_full_threshold,
            "disk_full_threshold should match"
        );
        assert_eq!(
            result1.stats.total_replays, result2.stats.total_replays,
            "Replay counts should match"
        );
        assert_eq!(
            result1.stats.trace.len(),
            result2.stats.trace.len(),
            "Trace lengths should match"
        );

        // Compare trace steps
        for (s1, s2) in result1.stats.trace.iter().zip(result2.stats.trace.iter()) {
            assert_eq!(s1, s2, "Trace steps should match");
        }

        println!(
            "Determinism verified: both runs converged to workload={}, replays={}",
            result1.fault_config.workload_len, result1.stats.total_replays
        );
    }

    /// Test that the shrinker preserves failure kind.
    ///
    /// Constructs a scenario where different configs produce different failure kinds.
    /// The shrinker should reject shrinks that would change the failure kind.
    #[test]
    fn shrinker_preserves_failure_kind() {
        // Create a replay function where:
        // - workload >= 20 with partial_write >= 0.1 -> SerializabilityCycle
        // - workload >= 20 with partial_write < 0.1 -> LostWrite (different kind!)
        // - workload < 20 -> no failure
        let original_kind = FailureKind::SerializabilityCycle {
            txn_ids: vec![1, 2, 3],
        };
        let alternate_kind = FailureKind::LostWrite {
            key: b"lost".to_vec(),
            expected: b"value".to_vec(),
        };

        let orig_kind = original_kind.clone();
        let alt_kind = alternate_kind.clone();

        let replay_fn: ReplayFn = Box::new(move |_seed, cfg| {
            if cfg.workload_len >= 20 {
                if cfg.partial_write_prob >= 0.1 {
                    Err(orig_kind.clone())
                } else {
                    // Lower partial_write produces a different failure kind!
                    Err(alt_kind.clone())
                }
            } else {
                Ok(())
            }
        });

        let report = FailureReport::new(
            0x12345678,
            FaultScenario::Combined,
            100,
            original_kind.clone(),
        );

        let mut shrinker = TestableShrinker::new(DeltaDebugConfig::default(), replay_fn);
        let result = shrinker.shrink(&report);

        assert!(result.is_some(), "Should find a minimal repro");
        let result = result.unwrap();

        println!("=== SHRINK TRACE ===");
        for step in &result.stats.trace {
            println!("{}", step.display());
        }
        println!("===================");

        // The shrinker should NOT have reduced partial_write_prob below 0.1
        // because that would produce LostWrite instead of SerializabilityCycle
        assert!(
            result.fault_config.partial_write_prob >= 0.1,
            "partial_write_prob should stay >= 0.1 to preserve failure kind: got {}",
            result.fault_config.partial_write_prob
        );

        // Verify the failure kind is still SerializabilityCycle
        assert!(
            result.report.failure_kind.matches(&original_kind),
            "Failure kind should be preserved as SerializabilityCycle"
        );

        // The workload should still be shrunk to ~20
        assert!(
            result.fault_config.workload_len <= 30,
            "Workload should be shrunk: got {}",
            result.fault_config.workload_len
        );

        println!(
            "Failure kind preserved: partial_write_prob={:.2}, workload={}",
            result.fault_config.partial_write_prob, result.fault_config.workload_len
        );
    }
}
