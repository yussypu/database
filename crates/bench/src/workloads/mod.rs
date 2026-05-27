//! Workload generators for benchmarking.
//!
//! Implements YCSB workloads A-F and a TPC-C-shaped OLTP workload.

pub mod tpcc;
pub mod ycsb;

pub use tpcc::{
    execute_new_order, keys as tpcc_keys, ExecutionResult, NewOrderTxn, OrderItem, TpccConfig,
    TpccGenerator,
};
pub use ycsb::{Operation, YcsbConfig, YcsbGenerator, YcsbWorkload};
