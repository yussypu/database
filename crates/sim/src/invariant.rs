//! Invariant checking for simulation testing.
//!
//! Runs after each operation to verify correctness properties. These checks
//! are the foundation of correctness verification in the simulation.
//!
//! # Checked Invariants
//!
//! - **Durability**: Synced writes survive crash
//! - **Read correctness**: Reads return expected values
//! - **Scan correctness**: Range scans return correct ordered results
//! - **No phantom data**: No data appears that wasn't written
//! - **Structural integrity**: Internal data structures are consistent

use crate::model::Model;
use bytes::Bytes;

/// Result of an invariant check.
#[derive(Debug, Clone)]
pub struct InvariantResult {
    /// Name of the invariant.
    pub name: String,
    /// Whether the invariant passed.
    pub passed: bool,
    /// Details about the check.
    pub details: String,
    /// Keys involved in a violation (if any).
    pub violated_keys: Vec<Vec<u8>>,
}

impl InvariantResult {
    /// Creates a passing result.
    pub fn pass(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            passed: true,
            details: "OK".to_string(),
            violated_keys: Vec::new(),
        }
    }

    /// Creates a failing result.
    pub fn fail(name: impl Into<String>, details: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            passed: false,
            details: details.into(),
            violated_keys: Vec::new(),
        }
    }

    /// Creates a failing result with violated keys.
    pub fn fail_with_keys(
        name: impl Into<String>,
        details: impl Into<String>,
        keys: Vec<Vec<u8>>,
    ) -> Self {
        Self {
            name: name.into(),
            passed: false,
            details: details.into(),
            violated_keys: keys,
        }
    }
}

/// A collection of invariant check results.
#[derive(Debug, Clone, Default)]
pub struct InvariantReport {
    /// Individual check results.
    pub results: Vec<InvariantResult>,
    /// Total checks performed.
    pub total_checks: usize,
    /// Checks that passed.
    pub passed: usize,
    /// Checks that failed.
    pub failed: usize,
}

impl InvariantReport {
    /// Creates a new empty report.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a result to the report.
    pub fn add(&mut self, result: InvariantResult) {
        self.total_checks += 1;
        if result.passed {
            self.passed += 1;
        } else {
            self.failed += 1;
        }
        self.results.push(result);
    }

    /// Returns true if all checks passed.
    pub fn all_passed(&self) -> bool {
        self.failed == 0
    }

    /// Returns failed results only.
    pub fn failures(&self) -> impl Iterator<Item = &InvariantResult> {
        self.results.iter().filter(|r| !r.passed)
    }

    /// Returns a summary string.
    pub fn summary(&self) -> String {
        if self.all_passed() {
            format!("All {} invariants passed", self.total_checks)
        } else {
            format!("{}/{} invariants failed", self.failed, self.total_checks)
        }
    }

    /// Merges another report into this one.
    pub fn merge(&mut self, other: InvariantReport) {
        self.total_checks += other.total_checks;
        self.passed += other.passed;
        self.failed += other.failed;
        self.results.extend(other.results);
    }
}

/// Checks that all keys in the model have correct values in the database.
///
/// This is the core durability check: every committed write should be readable.
pub fn check_durability<F>(model: &Model, mut get: F) -> InvariantResult
where
    F: FnMut(&[u8]) -> Option<Bytes>,
{
    let mut mismatches = Vec::new();

    for key in model.all_keys() {
        let expected = model.get_value(&key);
        let actual = get(&key);

        match (expected, actual.as_ref()) {
            (Some(exp), Some(act)) if exp == act => {}
            (None, None) => {}
            _ => {
                mismatches.push(key.clone());
            }
        }
    }

    if mismatches.is_empty() {
        InvariantResult::pass("durability")
    } else {
        InvariantResult::fail_with_keys(
            "durability",
            format!(
                "{} keys have incorrect values after crash",
                mismatches.len()
            ),
            mismatches,
        )
    }
}

/// Checks that no unexpected keys exist in the database.
///
/// This catches phantom writes or data corruption that adds spurious data.
pub fn check_no_phantom_data<F>(
    model: &Model,
    keys_to_check: &[Vec<u8>],
    mut get: F,
) -> InvariantResult
where
    F: FnMut(&[u8]) -> Option<Bytes>,
{
    let mut phantoms = Vec::new();

    for key in keys_to_check {
        if model.get(key).is_none() {
            // Key was never written to model
            if get(key).is_some() {
                phantoms.push(key.clone());
            }
        }
    }

    if phantoms.is_empty() {
        InvariantResult::pass("no_phantom_data")
    } else {
        InvariantResult::fail_with_keys(
            "no_phantom_data",
            format!("{} phantom keys found in database", phantoms.len()),
            phantoms,
        )
    }
}

/// Checks read-your-writes consistency.
///
/// After a successful put, an immediate get should return the written value.
pub fn check_read_your_writes(
    key: &[u8],
    expected_value: Option<&Bytes>,
    actual_value: Option<Bytes>,
) -> InvariantResult {
    match (expected_value, actual_value.as_ref()) {
        (Some(exp), Some(act)) if exp == act => InvariantResult::pass("read_your_writes"),
        (None, None) => InvariantResult::pass("read_your_writes"),
        (Some(exp), Some(act)) => InvariantResult::fail_with_keys(
            "read_your_writes",
            format!(
                "Expected {:?}, got {:?}",
                String::from_utf8_lossy(exp),
                String::from_utf8_lossy(act)
            ),
            vec![key.to_vec()],
        ),
        (Some(exp), None) => InvariantResult::fail_with_keys(
            "read_your_writes",
            format!("Expected {:?}, got None", String::from_utf8_lossy(exp)),
            vec![key.to_vec()],
        ),
        (None, Some(act)) => InvariantResult::fail_with_keys(
            "read_your_writes",
            format!("Expected None, got {:?}", String::from_utf8_lossy(act)),
            vec![key.to_vec()],
        ),
    }
}

/// Checks that a scan returns results in sorted order.
pub fn check_scan_order(results: &[(Vec<u8>, Bytes)]) -> InvariantResult {
    for window in results.windows(2) {
        if window[0].0 >= window[1].0 {
            return InvariantResult::fail_with_keys(
                "scan_order",
                format!(
                    "Keys out of order: {:?} >= {:?}",
                    String::from_utf8_lossy(&window[0].0),
                    String::from_utf8_lossy(&window[1].0)
                ),
                vec![window[0].0.clone(), window[1].0.clone()],
            );
        }
    }
    InvariantResult::pass("scan_order")
}

/// Checks that a scan returns the expected results.
pub fn check_scan_correctness(
    model: &Model,
    start: &[u8],
    end: &[u8],
    actual_results: &[(Vec<u8>, Bytes)],
) -> InvariantResult {
    let expected = model.scan(start, end);

    if expected.len() != actual_results.len() {
        return InvariantResult::fail(
            "scan_correctness",
            format!(
                "Expected {} results, got {}",
                expected.len(),
                actual_results.len()
            ),
        );
    }

    for (exp, act) in expected.iter().zip(actual_results.iter()) {
        if exp.0 != act.0 || exp.1 != act.1 {
            return InvariantResult::fail_with_keys(
                "scan_correctness",
                format!(
                    "Mismatch at key {:?}: expected {:?}, got {:?}",
                    String::from_utf8_lossy(&exp.0),
                    String::from_utf8_lossy(&exp.1),
                    String::from_utf8_lossy(&act.1)
                ),
                vec![exp.0.clone()],
            );
        }
    }

    InvariantResult::pass("scan_correctness")
}

/// Configuration for invariant checking.
#[derive(Debug, Clone)]
pub struct InvariantConfig {
    /// Check durability after each crash.
    pub check_durability: bool,
    /// Check read-your-writes after each write.
    pub check_read_your_writes: bool,
    /// Check for phantom data periodically.
    pub check_phantom_data: bool,
    /// Check scan order on scan operations.
    pub check_scan_order: bool,
    /// Number of random keys to check for phantoms.
    pub phantom_check_sample_size: usize,
}

impl Default for InvariantConfig {
    fn default() -> Self {
        Self {
            check_durability: true,
            check_read_your_writes: true,
            check_phantom_data: true,
            check_scan_order: true,
            phantom_check_sample_size: 100,
        }
    }
}

/// An invariant checker that tracks state and runs checks.
pub struct InvariantChecker {
    config: InvariantConfig,
    /// Total checks performed.
    total_checks: u64,
    /// Total failures.
    total_failures: u64,
    /// History of violations for debugging.
    violations: Vec<InvariantViolation>,
}

/// A recorded invariant violation.
#[derive(Debug, Clone)]
pub struct InvariantViolation {
    /// When the violation occurred (operation count).
    pub op_num: u64,
    /// The invariant that was violated.
    pub invariant: String,
    /// Details about the violation.
    pub details: String,
    /// Keys involved.
    pub keys: Vec<Vec<u8>>,
}

impl InvariantChecker {
    /// Creates a new invariant checker.
    pub fn new(config: InvariantConfig) -> Self {
        Self {
            config,
            total_checks: 0,
            total_failures: 0,
            violations: Vec::new(),
        }
    }

    /// Returns the configuration.
    pub fn config(&self) -> &InvariantConfig {
        &self.config
    }

    /// Returns total checks performed.
    pub fn total_checks(&self) -> u64 {
        self.total_checks
    }

    /// Returns total failures.
    pub fn total_failures(&self) -> u64 {
        self.total_failures
    }

    /// Returns all violations.
    pub fn violations(&self) -> &[InvariantViolation] {
        &self.violations
    }

    /// Records a check result.
    pub fn record(&mut self, op_num: u64, result: &InvariantResult) {
        self.total_checks += 1;
        if !result.passed {
            self.total_failures += 1;
            self.violations.push(InvariantViolation {
                op_num,
                invariant: result.name.clone(),
                details: result.details.clone(),
                keys: result.violated_keys.clone(),
            });
        }
    }

    /// Runs durability check if enabled.
    pub fn check_durability_if_enabled<F>(
        &mut self,
        op_num: u64,
        model: &Model,
        get: F,
    ) -> Option<InvariantResult>
    where
        F: FnMut(&[u8]) -> Option<Bytes>,
    {
        if !self.config.check_durability {
            return None;
        }
        let result = check_durability(model, get);
        self.record(op_num, &result);
        Some(result)
    }

    /// Runs read-your-writes check if enabled.
    pub fn check_ryw_if_enabled(
        &mut self,
        op_num: u64,
        key: &[u8],
        expected: Option<&Bytes>,
        actual: Option<Bytes>,
    ) -> Option<InvariantResult> {
        if !self.config.check_read_your_writes {
            return None;
        }
        let result = check_read_your_writes(key, expected, actual);
        self.record(op_num, &result);
        Some(result)
    }

    /// Clears recorded violations.
    pub fn clear_violations(&mut self) {
        self.violations.clear();
    }

    /// Returns true if any violations occurred.
    pub fn has_violations(&self) -> bool {
        !self.violations.is_empty()
    }

    /// Gets a summary of the checker state.
    pub fn summary(&self) -> String {
        if self.violations.is_empty() {
            format!("All {} invariant checks passed", self.total_checks)
        } else {
            format!(
                "{} invariant violations in {} checks",
                self.violations.len(),
                self.total_checks
            )
        }
    }
}

/// Performs a full database verification against the model.
///
/// This is an expensive operation that checks every key in the model.
pub fn full_verification<F>(model: &Model, mut get: F) -> InvariantReport
where
    F: FnMut(&[u8]) -> Option<Bytes>,
{
    let mut report = InvariantReport::new();

    // Check all keys
    let mut mismatches = Vec::new();
    let mut correct = 0;

    for key in model.all_keys() {
        let expected = model.get_value(&key);
        let actual = get(&key);

        match (expected, actual.as_ref()) {
            (Some(exp), Some(act)) if exp == act => correct += 1,
            (None, None) => correct += 1,
            _ => mismatches.push(key.clone()),
        }
    }

    if mismatches.is_empty() {
        report.add(InvariantResult::pass("full_verification"));
    } else {
        report.add(InvariantResult::fail_with_keys(
            "full_verification",
            format!("{} correct, {} mismatches", correct, mismatches.len()),
            mismatches,
        ));
    }

    report
}

/// Spot-checks a random sample of keys for correctness.
///
/// Faster than full verification but may miss some issues.
pub fn spot_check<F, R>(
    model: &Model,
    sample_size: usize,
    mut get: F,
    mut rand: R,
) -> InvariantReport
where
    F: FnMut(&[u8]) -> Option<Bytes>,
    R: FnMut() -> u64,
{
    let mut report = InvariantReport::new();

    let keys = model.all_keys();
    if keys.is_empty() {
        report.add(InvariantResult::pass("spot_check"));
        return report;
    }

    let mut checked = 0;
    let mut mismatches = Vec::new();

    for _ in 0..sample_size.min(keys.len()) {
        let idx = (rand() as usize) % keys.len();
        let key = &keys[idx];

        let expected = model.get_value(key);
        let actual = get(key);

        match (expected, actual.as_ref()) {
            (Some(exp), Some(act)) if exp == act => {}
            (None, None) => {}
            _ => mismatches.push(key.clone()),
        }
        checked += 1;
    }

    if mismatches.is_empty() {
        report.add(InvariantResult::pass("spot_check"));
    } else {
        report.add(InvariantResult::fail_with_keys(
            "spot_check",
            format!(
                "{}/{} checked, {} mismatches",
                checked,
                keys.len(),
                mismatches.len()
            ),
            mismatches,
        ));
    }

    report
}

/// Verifies that the state after recovery matches the model.
pub fn verify_recovery<F>(model: &Model, mut get: F, keys_written: &[Vec<u8>]) -> InvariantReport
where
    F: FnMut(&[u8]) -> Option<Bytes>,
{
    let mut report = InvariantReport::new();

    // Check that all written keys have correct values
    let mut incorrect = Vec::new();
    let mut missing = Vec::new();

    for key in keys_written {
        let expected = model.get_value(key);
        let actual = get(key);

        match (expected, actual) {
            (Some(exp), Some(act)) if exp == &act => {}
            (None, None) => {}
            (Some(_), None) => missing.push(key.clone()),
            (None, Some(_)) => incorrect.push(key.clone()),
            (Some(_), Some(_)) => incorrect.push(key.clone()),
        }
    }

    if missing.is_empty() && incorrect.is_empty() {
        report.add(InvariantResult::pass("recovery_verification"));
    } else {
        let mut all_bad = missing.clone();
        all_bad.extend(incorrect.clone());
        report.add(InvariantResult::fail_with_keys(
            "recovery_verification",
            format!(
                "{} missing, {} incorrect of {} checked",
                missing.len(),
                incorrect.len(),
                keys_written.len()
            ),
            all_bad,
        ));
    }

    report
}

// ============================================================================
// Serialization Checking for MVCC/SSI
// ============================================================================

/// A recorded transaction operation for serialization checking.
#[derive(Debug, Clone)]
pub enum TxnOperation {
    /// Read a key and got a value (or None).
    Read {
        key: Vec<u8>,
        value: Option<Vec<u8>>,
    },
    /// Wrote a key with a value (or None for delete).
    Write {
        key: Vec<u8>,
        value: Option<Vec<u8>>,
    },
}

/// A completed transaction for serialization checking.
#[derive(Debug, Clone)]
pub struct CompletedTransaction {
    /// Transaction ID.
    pub txn_id: u64,
    /// Begin timestamp.
    pub begin_ts: u64,
    /// Commit timestamp (0 if aborted).
    pub commit_ts: u64,
    /// Whether the transaction committed.
    pub committed: bool,
    /// Operations performed.
    pub operations: Vec<TxnOperation>,
}

/// Dependency type between transactions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DependencyType {
    /// Write-Write: T2 overwrote a value written by T1
    WriteWrite,
    /// Write-Read: T2 read a value written by T1
    WriteRead,
    /// Read-Write (anti-dependency): T2 wrote a value that T1 read
    ReadWrite,
}

/// An edge in the serialization graph.
#[derive(Debug, Clone)]
pub struct DependencyEdge {
    /// Source transaction (depends on target).
    pub from: u64,
    /// Target transaction.
    pub to: u64,
    /// Type of dependency.
    pub dep_type: DependencyType,
    /// Key involved.
    pub key: Vec<u8>,
}

/// Serialization checker for MVCC transactions.
///
/// Builds a dependency graph between committed transactions and checks
/// for cycles. A cycle indicates a non-serializable schedule.
///
/// # Algorithm
///
/// For each pair of committed transactions (T1, T2) where T1.commit_ts < T2.commit_ts:
/// - WW: If T2 wrote a key that T1 wrote → T1 -> T2
/// - WR: If T2 read a value that T1 wrote → T1 -> T2
/// - RW: If T1 read a value that T2 wrote (and T1 didn't see T2's write) → T1 -> T2
///
/// Then check for cycles using DFS.
#[derive(Debug, Default)]
pub struct SerializationChecker {
    /// Completed transactions (committed and aborted).
    transactions: Vec<CompletedTransaction>,
    /// Dependency edges between committed transactions.
    edges: Vec<DependencyEdge>,
}

impl SerializationChecker {
    /// Create a new serialization checker.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a completed transaction.
    pub fn record_transaction(&mut self, txn: CompletedTransaction) {
        self.transactions.push(txn);
    }

    /// Build the dependency graph from recorded transactions.
    ///
    /// This must be called after all transactions are recorded.
    pub fn build_dependency_graph(&mut self) {
        self.edges.clear();

        // Get only committed transactions, sorted by commit timestamp
        let mut committed: Vec<_> = self.transactions.iter().filter(|t| t.committed).collect();
        committed.sort_by_key(|t| t.commit_ts);

        // For each pair of transactions
        for i in 0..committed.len() {
            for j in (i + 1)..committed.len() {
                let t1 = committed[i];
                let t2 = committed[j];

                // Get write sets
                let t1_writes = Self::get_write_set(t1);
                let t2_writes = Self::get_write_set(t2);
                let t1_reads = Self::get_read_set(t1);
                let t2_reads = Self::get_read_set(t2);

                // Check WW: T2 overwrote what T1 wrote
                for key in t1_writes.keys() {
                    if t2_writes.contains_key(key) {
                        self.edges.push(DependencyEdge {
                            from: t1.txn_id,
                            to: t2.txn_id,
                            dep_type: DependencyType::WriteWrite,
                            key: key.clone(),
                        });
                    }
                }

                // Check WR: T2 read what T1 wrote
                for key in t1_writes.keys() {
                    if t2_reads.contains_key(key) {
                        // Check if T2 actually saw T1's write (T2.begin_ts >= T1.commit_ts)
                        if t2.begin_ts >= t1.commit_ts {
                            self.edges.push(DependencyEdge {
                                from: t1.txn_id,
                                to: t2.txn_id,
                                dep_type: DependencyType::WriteRead,
                                key: key.clone(),
                            });
                        }
                    }
                }

                // Check RW (anti-dependency): T1 read something, T2 wrote it
                // This creates edge T1 -> T2 if T1 didn't see T2's write
                for key in t1_reads.keys() {
                    if t2_writes.contains_key(key) {
                        // T1 read before T2 committed (snapshot isolation)
                        if t1.begin_ts < t2.commit_ts {
                            self.edges.push(DependencyEdge {
                                from: t1.txn_id,
                                to: t2.txn_id,
                                dep_type: DependencyType::ReadWrite,
                                key: key.clone(),
                            });
                        }
                    }
                }

                // Also check RW in reverse: T2 read something, T1 wrote it
                // Creates edge T2 -> T1 if T2 didn't see T1's write
                for key in t2_reads.keys() {
                    if t1_writes.contains_key(key) {
                        // T2's snapshot was taken before T1 committed
                        if t2.begin_ts < t1.commit_ts {
                            self.edges.push(DependencyEdge {
                                from: t2.txn_id,
                                to: t1.txn_id,
                                dep_type: DependencyType::ReadWrite,
                                key: key.clone(),
                            });
                        }
                    }
                }
            }
        }
    }

    fn get_write_set(
        txn: &CompletedTransaction,
    ) -> std::collections::HashMap<Vec<u8>, Option<Vec<u8>>> {
        let mut writes = std::collections::HashMap::new();
        for op in &txn.operations {
            if let TxnOperation::Write { key, value } = op {
                writes.insert(key.clone(), value.clone());
            }
        }
        writes
    }

    fn get_read_set(
        txn: &CompletedTransaction,
    ) -> std::collections::HashMap<Vec<u8>, Option<Vec<u8>>> {
        let mut reads = std::collections::HashMap::new();
        for op in &txn.operations {
            if let TxnOperation::Read { key, value } = op {
                reads.insert(key.clone(), value.clone());
            }
        }
        reads
    }

    /// Check for cycles in the dependency graph.
    ///
    /// Returns Ok(()) if serializable, Err with cycle info if not.
    pub fn check_serializability(&self) -> Result<(), SerializationViolation> {
        // Build adjacency list
        let mut adj: std::collections::HashMap<u64, Vec<u64>> = std::collections::HashMap::new();
        let mut all_txns: std::collections::HashSet<u64> = std::collections::HashSet::new();

        for edge in &self.edges {
            adj.entry(edge.from).or_default().push(edge.to);
            all_txns.insert(edge.from);
            all_txns.insert(edge.to);
        }

        // DFS for cycle detection
        let mut visited = std::collections::HashSet::new();
        let mut rec_stack = std::collections::HashSet::new();
        let mut cycle_path = Vec::new();

        for &start in &all_txns {
            if !visited.contains(&start)
                && Self::has_cycle_dfs(start, &adj, &mut visited, &mut rec_stack, &mut cycle_path)
            {
                return Err(SerializationViolation {
                    cycle: cycle_path,
                    edges: self.edges.clone(),
                });
            }
        }

        Ok(())
    }

    fn has_cycle_dfs(
        node: u64,
        adj: &std::collections::HashMap<u64, Vec<u64>>,
        visited: &mut std::collections::HashSet<u64>,
        rec_stack: &mut std::collections::HashSet<u64>,
        path: &mut Vec<u64>,
    ) -> bool {
        visited.insert(node);
        rec_stack.insert(node);
        path.push(node);

        if let Some(neighbors) = adj.get(&node) {
            for &neighbor in neighbors {
                if !visited.contains(&neighbor) {
                    if Self::has_cycle_dfs(neighbor, adj, visited, rec_stack, path) {
                        return true;
                    }
                } else if rec_stack.contains(&neighbor) {
                    // Found a cycle
                    path.push(neighbor);
                    return true;
                }
            }
        }

        path.pop();
        rec_stack.remove(&node);
        false
    }

    /// Get the number of recorded transactions.
    pub fn transaction_count(&self) -> usize {
        self.transactions.len()
    }

    /// Get the number of dependency edges.
    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    /// Clear all recorded data.
    pub fn clear(&mut self) {
        self.transactions.clear();
        self.edges.clear();
    }
}

/// A serialization violation with cycle information.
#[derive(Debug, Clone)]
pub struct SerializationViolation {
    /// Transaction IDs forming the cycle.
    pub cycle: Vec<u64>,
    /// All dependency edges for debugging.
    pub edges: Vec<DependencyEdge>,
}

impl std::fmt::Display for SerializationViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Non-serializable schedule detected. Cycle: ")?;
        for (i, txn_id) in self.cycle.iter().enumerate() {
            if i > 0 {
                write!(f, " -> ")?;
            }
            write!(f, "T{}", txn_id)?;
        }
        Ok(())
    }
}

/// Run a serialization check and return an InvariantResult.
pub fn check_serializability(checker: &SerializationChecker) -> InvariantResult {
    match checker.check_serializability() {
        Ok(()) => InvariantResult::pass("serializability"),
        Err(violation) => InvariantResult::fail("serializability", format!("{}", violation)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durability_check_passes() {
        let mut model = Model::new();
        model.put(b"key1", b"value1");
        model.put(b"key2", b"value2");

        let result = check_durability(&model, |key| match key {
            b"key1" => Some(Bytes::from_static(b"value1")),
            b"key2" => Some(Bytes::from_static(b"value2")),
            _ => None,
        });

        assert!(result.passed);
    }

    #[test]
    fn durability_check_fails_on_mismatch() {
        let mut model = Model::new();
        model.put(b"key1", b"value1");

        let result = check_durability(&model, |_| None);

        assert!(!result.passed);
        assert_eq!(result.violated_keys.len(), 1);
    }

    #[test]
    fn read_your_writes_passes() {
        let result = check_read_your_writes(
            b"key",
            Some(&Bytes::from_static(b"value")),
            Some(Bytes::from_static(b"value")),
        );
        assert!(result.passed);
    }

    #[test]
    fn read_your_writes_fails() {
        let result = check_read_your_writes(
            b"key",
            Some(&Bytes::from_static(b"expected")),
            Some(Bytes::from_static(b"actual")),
        );
        assert!(!result.passed);
    }

    #[test]
    fn scan_order_passes() {
        let results = vec![
            (b"a".to_vec(), Bytes::from_static(b"1")),
            (b"b".to_vec(), Bytes::from_static(b"2")),
            (b"c".to_vec(), Bytes::from_static(b"3")),
        ];
        let result = check_scan_order(&results);
        assert!(result.passed);
    }

    #[test]
    fn scan_order_fails() {
        let results = vec![
            (b"b".to_vec(), Bytes::from_static(b"2")),
            (b"a".to_vec(), Bytes::from_static(b"1")),
        ];
        let result = check_scan_order(&results);
        assert!(!result.passed);
    }

    #[test]
    fn invariant_report() {
        let mut report = InvariantReport::new();
        report.add(InvariantResult::pass("test1"));
        report.add(InvariantResult::pass("test2"));

        assert!(report.all_passed());
        assert_eq!(report.total_checks, 2);
        assert_eq!(report.passed, 2);
        assert_eq!(report.failed, 0);

        report.add(InvariantResult::fail("test3", "oops"));
        assert!(!report.all_passed());
        assert_eq!(report.failed, 1);
    }

    #[test]
    fn invariant_checker() {
        let mut checker = InvariantChecker::new(InvariantConfig::default());

        checker.record(1, &InvariantResult::pass("test1"));
        checker.record(2, &InvariantResult::fail("test2", "bad"));

        assert!(checker.has_violations());
        assert_eq!(checker.violations().len(), 1);
        assert_eq!(checker.violations()[0].op_num, 2);
    }

    #[test]
    fn full_verification_passes() {
        let mut model = Model::new();
        model.put(b"a", b"1");
        model.put(b"b", b"2");
        model.delete(b"c");

        let report = full_verification(&model, |key| match key {
            b"a" => Some(Bytes::from_static(b"1")),
            b"b" => Some(Bytes::from_static(b"2")),
            _ => None,
        });

        assert!(report.all_passed());
    }

    #[test]
    fn serialization_checker_no_transactions() {
        let checker = SerializationChecker::new();
        assert!(checker.check_serializability().is_ok());
    }

    #[test]
    fn serialization_checker_single_transaction() {
        let mut checker = SerializationChecker::new();
        checker.record_transaction(CompletedTransaction {
            txn_id: 1,
            begin_ts: 1,
            commit_ts: 2,
            committed: true,
            operations: vec![TxnOperation::Write {
                key: b"x".to_vec(),
                value: Some(b"1".to_vec()),
            }],
        });
        checker.build_dependency_graph();
        assert!(checker.check_serializability().is_ok());
    }

    #[test]
    fn serialization_checker_serializable_schedule() {
        // T1: write x
        // T2: read x (sees T1's write), write y
        // This is serializable: T1 -> T2
        let mut checker = SerializationChecker::new();
        checker.record_transaction(CompletedTransaction {
            txn_id: 1,
            begin_ts: 1,
            commit_ts: 2,
            committed: true,
            operations: vec![TxnOperation::Write {
                key: b"x".to_vec(),
                value: Some(b"1".to_vec()),
            }],
        });
        checker.record_transaction(CompletedTransaction {
            txn_id: 2,
            begin_ts: 3, // After T1 committed
            commit_ts: 4,
            committed: true,
            operations: vec![
                TxnOperation::Read {
                    key: b"x".to_vec(),
                    value: Some(b"1".to_vec()),
                },
                TxnOperation::Write {
                    key: b"y".to_vec(),
                    value: Some(b"2".to_vec()),
                },
            ],
        });
        checker.build_dependency_graph();
        assert!(checker.check_serializability().is_ok());
    }

    #[test]
    fn serialization_checker_aborted_transaction_ignored() {
        // T1: write x, commits
        // T2: write x, aborts (should be ignored)
        let mut checker = SerializationChecker::new();
        checker.record_transaction(CompletedTransaction {
            txn_id: 1,
            begin_ts: 1,
            commit_ts: 2,
            committed: true,
            operations: vec![TxnOperation::Write {
                key: b"x".to_vec(),
                value: Some(b"1".to_vec()),
            }],
        });
        checker.record_transaction(CompletedTransaction {
            txn_id: 2,
            begin_ts: 1,
            commit_ts: 0,
            committed: false, // Aborted!
            operations: vec![TxnOperation::Write {
                key: b"x".to_vec(),
                value: Some(b"2".to_vec()),
            }],
        });
        checker.build_dependency_graph();
        // Only 1 committed transaction, so no cycles possible
        assert!(checker.check_serializability().is_ok());
        assert_eq!(checker.edge_count(), 0);
    }

    #[test]
    fn serialization_checker_detects_write_skew_cycle() {
        // Classic write skew anomaly (non-serializable under SI without SSI):
        // T1: read x (sees 0), read y (sees 0), write x = 1
        // T2: read x (sees 0), read y (sees 0), write y = 1
        // Both start at same time, both commit
        //
        // Dependency edges:
        // - T1 -> T2 (RW on y: T1 read y, T2 wrote y)
        // - T2 -> T1 (RW on x: T2 read x, T1 wrote x)
        // This forms a cycle: T1 -> T2 -> T1
        let mut checker = SerializationChecker::new();

        // T1: read x, read y, write x
        checker.record_transaction(CompletedTransaction {
            txn_id: 1,
            begin_ts: 1,
            commit_ts: 3, // Commits at ts 3
            committed: true,
            operations: vec![
                TxnOperation::Read {
                    key: b"x".to_vec(),
                    value: Some(b"0".to_vec()),
                },
                TxnOperation::Read {
                    key: b"y".to_vec(),
                    value: Some(b"0".to_vec()),
                },
                TxnOperation::Write {
                    key: b"x".to_vec(),
                    value: Some(b"1".to_vec()),
                },
            ],
        });

        // T2: read x, read y, write y
        checker.record_transaction(CompletedTransaction {
            txn_id: 2,
            begin_ts: 1,  // Same begin time as T1
            commit_ts: 4, // Commits after T1
            committed: true,
            operations: vec![
                TxnOperation::Read {
                    key: b"x".to_vec(),
                    value: Some(b"0".to_vec()),
                },
                TxnOperation::Read {
                    key: b"y".to_vec(),
                    value: Some(b"0".to_vec()),
                },
                TxnOperation::Write {
                    key: b"y".to_vec(),
                    value: Some(b"1".to_vec()),
                },
            ],
        });

        checker.build_dependency_graph();

        // Should have RW edges in both directions
        assert!(checker.edge_count() >= 2);

        // Should detect the cycle
        let result = checker.check_serializability();
        assert!(result.is_err(), "Should detect write skew cycle");

        let violation = result.unwrap_err();
        assert!(
            violation.cycle.len() >= 2,
            "Cycle should contain at least 2 transactions"
        );
    }

    #[test]
    fn serialization_checker_three_txn_cycle() {
        // A cycle involving three transactions:
        // T1: read x, write y
        // T2: read y, write z
        // T3: read z, write x
        //
        // Edges: T1 -> T2 (WR on y), T2 -> T3 (WR on z), T3 -> T1 (WR on x)
        // But wait, we need RW edges for cycle. Let me think...
        //
        // Actually with proper snapshot isolation:
        // T1 writes y, T2 reads y (WR: T1 -> T2)
        // T2 writes z, T3 reads z (WR: T2 -> T3)
        // T3 writes x, T1 reads x (but T1 committed before T3, so no WR here)
        //
        // For a cycle, we need:
        // T1: read z, write y (commits at 2)
        // T2: read y (sees nothing), write z (commits at 3)
        // T3: read y (sees T1's), read z (sees nothing), write x (commits at 4)
        //
        // Hmm, this is getting complex. Let me use a simpler formulation
        // where all txns start at same time and have overlapping reads/writes.

        let mut checker = SerializationChecker::new();

        // T1: read z, write x (commits first)
        checker.record_transaction(CompletedTransaction {
            txn_id: 1,
            begin_ts: 1,
            commit_ts: 2,
            committed: true,
            operations: vec![
                TxnOperation::Read {
                    key: b"z".to_vec(),
                    value: None,
                },
                TxnOperation::Write {
                    key: b"x".to_vec(),
                    value: Some(b"1".to_vec()),
                },
            ],
        });

        // T2: read x (sees nothing - started before T1 committed), write y
        checker.record_transaction(CompletedTransaction {
            txn_id: 2,
            begin_ts: 1,
            commit_ts: 3,
            committed: true,
            operations: vec![
                TxnOperation::Read {
                    key: b"x".to_vec(),
                    value: None,
                },
                TxnOperation::Write {
                    key: b"y".to_vec(),
                    value: Some(b"2".to_vec()),
                },
            ],
        });

        // T3: read y (sees nothing - started before T2 committed), write z
        checker.record_transaction(CompletedTransaction {
            txn_id: 3,
            begin_ts: 1,
            commit_ts: 4,
            committed: true,
            operations: vec![
                TxnOperation::Read {
                    key: b"y".to_vec(),
                    value: None,
                },
                TxnOperation::Write {
                    key: b"z".to_vec(),
                    value: Some(b"3".to_vec()),
                },
            ],
        });

        checker.build_dependency_graph();

        // RW edges: T2 read x, T1 wrote x -> T2 -> T1? No wait...
        // T2 read x (value None), T1 wrote x (value 1) and T1.commit_ts=2
        // T2.begin_ts=1 < T1.commit_ts=2, so RW edge T2 -> T1
        //
        // T3 read y (value None), T2 wrote y and T2.commit_ts=3
        // T3.begin_ts=1 < T2.commit_ts=3, so RW edge T3 -> T2
        //
        // T1 read z (value None), T3 wrote z and T3.commit_ts=4
        // T1.begin_ts=1 < T3.commit_ts=4, so RW edge T1 -> T3
        //
        // Cycle: T1 -> T3 -> T2 -> T1

        let result = checker.check_serializability();
        assert!(result.is_err(), "Should detect 3-transaction cycle");
    }
}
