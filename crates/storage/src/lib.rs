//! LSM storage engine for cracked-db.
//!
//! This crate provides the core storage layer:
//! - Write-Ahead Log (WAL)
//! - Memtable (concurrent skiplist)
//! - SSTables (immutable sorted files)
//! - Compaction (leveled)
//! - Recovery
//!
//! All I/O goes through the [`runtime::Env`] trait for deterministic testing.

pub mod compaction;
pub mod engine;
pub mod error;
pub mod group_commit;
pub mod memtable;
pub mod sstable;
pub mod wal;

pub use error::{Error, Result};

// Re-export main engine types
pub use engine::{EngineConfig, EngineScan, LsmEngine as Engine};

// Re-export group commit types for monitoring
pub use group_commit::{GroupCommitConfig, GroupCommitStats};

// Re-export WAL reader for testing
pub use wal::{WalReader, WalRecord};

/// Re-export runtime for convenience.
pub use runtime;
