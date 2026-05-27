//! Multi-Version Concurrency Control with Serializable Snapshot Isolation.
//!
//! This crate implements MVCC with SSI for cracked-db:
//!
//! - Version chains: Each key has a chain of versions ordered by commit timestamp
//! - Snapshot reads: Transactions read at their begin-timestamp
//! - SSI: Serializable Snapshot Isolation with rw-antidependency tracking
//!
//! # Stage 5b Integration
//!
//! As of Stage 5b, VersionStore is engine-backed. All versioned data is stored
//! directly in the LSM engine, not in an in-memory cache. See ADR-025.
//!
//! # TLA+ Spec Reference
//!
//! See `specs/MVCC.tla` for the snapshot isolation specification.
//! See `specs/SSI.tla` for the SSI specification.
//!
//! # References
//!
//! Cahill, Röhm, Fekete, "Serializable Isolation for Snapshot Databases"
//! (SIGMOD 2008)
//!
//! # Example
//!
//! ```ignore
//! use std::sync::Arc;
//! use runtime::{SimEnv, SimEnvConfig};
//! use storage::{Engine, EngineConfig};
//! use mvcc::transaction::TransactionManager;
//! use mvcc::version::VersionStore;
//!
//! let env = SimEnv::new(SimEnvConfig::with_seed(42));
//! env.create_dir_all(runtime::Path::new("/db")).unwrap();
//! let engine = Arc::new(
//!     Engine::open(env, runtime::Path::new("/db"), EngineConfig::default()).unwrap(),
//! );
//! let store = Arc::new(VersionStore::new(engine));
//! let mgr = TransactionManager::new(store);
//!
//! // Start a transaction
//! let mut txn = mgr.begin();
//!
//! // Write some data
//! mgr.write(&mut txn, b"key1", b"value1").unwrap();
//!
//! // Read data (sees own writes)
//! let value = mgr.read(&mut txn, b"key1").unwrap();
//! assert_eq!(value.as_deref(), Some(b"value1".as_slice()));
//!
//! // Commit the transaction
//! mgr.commit(&mut txn).unwrap();
//! ```

pub mod ssi;
pub mod transaction;
pub mod version;

// Re-export main types for convenience
pub use transaction::{Transaction, TransactionManager, TxnError, TxnId, TxnResult, TxnStatus};
pub use version::VersionStore;

// SSI types
pub use ssi::{SSIError, SSIResult, SSITransaction, SSITransactionManager};

/// Re-export runtime for convenience.
pub use runtime;
