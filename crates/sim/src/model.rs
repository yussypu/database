//! Reference database (oracle) for simulation testing.
//!
//! The model is a simple HashMap-based key-value store that tracks the expected
//! state of the database. During simulation, every operation is applied to both
//! the real database and the model. After each operation and crash, the model
//! is used to verify that the real database returns the correct values.
//!
//! # Design
//!
//! - All operations are applied to both model and system under test (SUT)
//! - Only synced/committed operations are tracked in the model
//! - After crash, model state should match SUT state exactly

use bytes::Bytes;
use std::collections::BTreeMap;

/// The reference model for verification.
///
/// This is a simplified view of what the database should contain.
/// We use BTreeMap for deterministic iteration order.
#[derive(Clone, Debug)]
pub struct Model {
    /// The current key-value state.
    /// None value means the key was deleted (tombstone).
    state: BTreeMap<Vec<u8>, Option<Bytes>>,

    /// History of all committed operations for debugging.
    history: Vec<ModelOp>,

    /// Operation counter for ordering.
    op_count: u64,
}

/// A recorded operation in the model.
#[derive(Clone, Debug)]
pub struct ModelOp {
    /// Operation sequence number.
    pub seq: u64,
    /// The operation type.
    pub kind: OpKind,
    /// The key involved.
    pub key: Vec<u8>,
    /// The value (for Put operations).
    pub value: Option<Bytes>,
}

/// The kind of operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpKind {
    Put,
    Delete,
    Get,
}

impl Default for Model {
    fn default() -> Self {
        Self::new()
    }
}

impl Model {
    /// Creates a new empty model.
    pub fn new() -> Self {
        Self {
            state: BTreeMap::new(),
            history: Vec::new(),
            op_count: 0,
        }
    }

    /// Records a put operation.
    pub fn put(&mut self, key: &[u8], value: &[u8]) {
        let op = ModelOp {
            seq: self.op_count,
            kind: OpKind::Put,
            key: key.to_vec(),
            value: Some(Bytes::copy_from_slice(value)),
        };
        self.history.push(op);
        self.state
            .insert(key.to_vec(), Some(Bytes::copy_from_slice(value)));
        self.op_count += 1;
    }

    /// Records a delete operation.
    pub fn delete(&mut self, key: &[u8]) {
        let op = ModelOp {
            seq: self.op_count,
            kind: OpKind::Delete,
            key: key.to_vec(),
            value: None,
        };
        self.history.push(op);
        self.state.insert(key.to_vec(), None);
        self.op_count += 1;
    }

    /// Gets the expected value for a key.
    ///
    /// Returns:
    /// - `Some(Some(value))` if key exists with a value
    /// - `Some(None)` if key was explicitly deleted
    /// - `None` if key was never written
    pub fn get(&self, key: &[u8]) -> Option<Option<&Bytes>> {
        self.state.get(key).map(|v| v.as_ref())
    }

    /// Gets the expected value, treating deleted and never-written as the same (None).
    pub fn get_value(&self, key: &[u8]) -> Option<&Bytes> {
        self.state.get(key).and_then(|v| v.as_ref())
    }

    /// Returns all keys that currently have a value (not deleted).
    pub fn live_keys(&self) -> Vec<Vec<u8>> {
        self.state
            .iter()
            .filter_map(|(k, v)| if v.is_some() { Some(k.clone()) } else { None })
            .collect()
    }

    /// Returns all keys that have been touched (including deleted).
    pub fn all_keys(&self) -> Vec<Vec<u8>> {
        self.state.keys().cloned().collect()
    }

    /// Returns the number of live (non-deleted) keys.
    pub fn live_count(&self) -> usize {
        self.state.values().filter(|v| v.is_some()).count()
    }

    /// Returns the total operation count.
    pub fn op_count(&self) -> u64 {
        self.op_count
    }

    /// Returns the operation history.
    pub fn history(&self) -> &[ModelOp] {
        &self.history
    }

    /// Clears the model state (but not history).
    pub fn clear(&mut self) {
        self.state.clear();
    }

    /// Creates a snapshot of the current state.
    pub fn snapshot(&self) -> ModelSnapshot {
        ModelSnapshot {
            state: self.state.clone(),
            op_count: self.op_count,
        }
    }

    /// Restores from a snapshot.
    pub fn restore(&mut self, snapshot: &ModelSnapshot) {
        self.state = snapshot.state.clone();
        self.op_count = snapshot.op_count;
    }

    /// Iterates over all key-value pairs in sorted order.
    pub fn iter(&self) -> impl Iterator<Item = (&Vec<u8>, &Option<Bytes>)> {
        self.state.iter()
    }

    /// Returns a scan result for a key range.
    pub fn scan(&self, start: &[u8], end: &[u8]) -> Vec<(Vec<u8>, Bytes)> {
        self.state
            .range(start.to_vec()..end.to_vec())
            .filter_map(|(k, v)| v.as_ref().map(|val| (k.clone(), val.clone())))
            .collect()
    }
}

/// A snapshot of model state for checkpoint/restore.
#[derive(Clone, Debug)]
pub struct ModelSnapshot {
    state: BTreeMap<Vec<u8>, Option<Bytes>>,
    op_count: u64,
}

impl ModelSnapshot {
    /// Returns the expected value for a key.
    pub fn get(&self, key: &[u8]) -> Option<&Bytes> {
        self.state.get(key).and_then(|v| v.as_ref())
    }

    /// Iterates over all live key-value pairs.
    pub fn iter_live(&self) -> impl Iterator<Item = (&Vec<u8>, &Bytes)> {
        self.state
            .iter()
            .filter_map(|(k, v)| v.as_ref().map(|val| (k, val)))
    }
}

/// Result of comparing model to actual database.
#[derive(Debug)]
pub struct ComparisonResult {
    /// Keys that have correct values.
    pub matching: usize,
    /// Keys where the value differs.
    pub mismatches: Vec<Mismatch>,
    /// Keys missing from actual that should exist.
    pub missing_from_actual: Vec<Vec<u8>>,
    /// Keys in actual that shouldn't exist.
    pub unexpected_in_actual: Vec<Vec<u8>>,
}

/// A single mismatch between model and actual.
#[derive(Debug)]
pub struct Mismatch {
    pub key: Vec<u8>,
    pub expected: Option<Bytes>,
    pub actual: Option<Bytes>,
}

impl ComparisonResult {
    /// Returns true if the model matches the actual state perfectly.
    pub fn is_ok(&self) -> bool {
        self.mismatches.is_empty()
            && self.missing_from_actual.is_empty()
            && self.unexpected_in_actual.is_empty()
    }

    /// Returns a summary string.
    pub fn summary(&self) -> String {
        if self.is_ok() {
            format!("OK: {} keys match", self.matching)
        } else {
            format!(
                "MISMATCH: {} matching, {} mismatches, {} missing, {} unexpected",
                self.matching,
                self.mismatches.len(),
                self.missing_from_actual.len(),
                self.unexpected_in_actual.len()
            )
        }
    }
}

/// Compares model state against actual database reads.
pub fn compare_model<F>(model: &Model, mut get_actual: F) -> ComparisonResult
where
    F: FnMut(&[u8]) -> Option<Bytes>,
{
    let mut result = ComparisonResult {
        matching: 0,
        mismatches: Vec::new(),
        missing_from_actual: Vec::new(),
        unexpected_in_actual: Vec::new(),
    };

    for (key, expected) in model.iter() {
        let actual = get_actual(key);

        match (expected, &actual) {
            (Some(exp), Some(act)) if exp == act => {
                result.matching += 1;
            }
            (None, None) => {
                // Both agree key is deleted/missing
                result.matching += 1;
            }
            (Some(_), None) => {
                result.missing_from_actual.push(key.clone());
            }
            (None, Some(act)) => {
                // Key was unexpected in actual - this is phantom data
                result.unexpected_in_actual.push(key.clone());
                result.mismatches.push(Mismatch {
                    key: key.clone(),
                    expected: None,
                    actual: Some(act.clone()),
                });
            }
            (Some(exp), Some(act)) => {
                result.mismatches.push(Mismatch {
                    key: key.clone(),
                    expected: Some(exp.clone()),
                    actual: Some(act.clone()),
                });
            }
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_put_get() {
        let mut model = Model::new();

        model.put(b"key1", b"value1");
        model.put(b"key2", b"value2");

        assert_eq!(
            model.get_value(b"key1"),
            Some(&Bytes::from_static(b"value1"))
        );
        assert_eq!(
            model.get_value(b"key2"),
            Some(&Bytes::from_static(b"value2"))
        );
        assert_eq!(model.get_value(b"key3"), None);
    }

    #[test]
    fn model_delete() {
        let mut model = Model::new();

        model.put(b"key", b"value");
        assert_eq!(model.get_value(b"key"), Some(&Bytes::from_static(b"value")));

        model.delete(b"key");
        assert_eq!(model.get_value(b"key"), None);

        // get() distinguishes between deleted and never-written
        assert_eq!(model.get(b"key"), Some(None)); // explicitly deleted
        assert_eq!(model.get(b"nonexistent"), None); // never written
    }

    #[test]
    fn model_overwrite() {
        let mut model = Model::new();

        model.put(b"key", b"v1");
        model.put(b"key", b"v2");
        model.put(b"key", b"v3");

        assert_eq!(model.get_value(b"key"), Some(&Bytes::from_static(b"v3")));
        assert_eq!(model.op_count(), 3);
    }

    #[test]
    fn model_snapshot_restore() {
        let mut model = Model::new();

        model.put(b"key1", b"value1");
        model.put(b"key2", b"value2");

        let snapshot = model.snapshot();

        model.put(b"key3", b"value3");
        model.delete(b"key1");

        assert_eq!(model.get_value(b"key1"), None);
        assert_eq!(
            model.get_value(b"key3"),
            Some(&Bytes::from_static(b"value3"))
        );

        model.restore(&snapshot);

        assert_eq!(
            model.get_value(b"key1"),
            Some(&Bytes::from_static(b"value1"))
        );
        assert_eq!(model.get_value(b"key3"), None);
    }

    #[test]
    fn comparison_matching() {
        let mut model = Model::new();
        model.put(b"key1", b"value1");
        model.put(b"key2", b"value2");

        let result = compare_model(&model, |key| match key {
            b"key1" => Some(Bytes::from_static(b"value1")),
            b"key2" => Some(Bytes::from_static(b"value2")),
            _ => None,
        });

        assert!(result.is_ok());
        assert_eq!(result.matching, 2);
    }

    #[test]
    fn comparison_mismatch() {
        let mut model = Model::new();
        model.put(b"key1", b"value1");

        let result = compare_model(&model, |key| match key {
            b"key1" => Some(Bytes::from_static(b"wrong_value")),
            _ => None,
        });

        assert!(!result.is_ok());
        assert_eq!(result.mismatches.len(), 1);
    }
}
