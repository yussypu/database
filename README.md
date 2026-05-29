crackeddb (in development)
An embedded OLTP database in Rust. Transactions are serializable. The WAL, MVCC, and SSI protocols are specified in TLA+ and machine checked. Every nondeterminism source routes through one trait, so bugs reproduce byte for byte from a seed. Every architectural decision lives in DECISIONS.md next to a regression test.
the demo
Two transactions, two keys, one invariant: at least one doctor stays on duty.
$ target/release/write_skew_demo
write skew demo (concurrent)
----------------------------
initial: alice=on, bob=on
T1 (thread A): read both, write alice=off
T2 (thread B): read both, write bob=off
invariant: at least one stays on

crackeddb   T1 ssi_abort=false  T2 ssi_abort=true
crackeddb   alice=off  bob=on   held
rocksdb     T1=ok  T2=ok
rocksdb     alice=off  bob=off  VIOLATED
Two threads, two databases, the same scenario, run side by side. RocksDB in its strongest serializable transaction mode (TransactionDB Pessimistic) commits both transactions, producing a state no serial schedule could produce. crackeddb detects the rw-antidependency cycle at commit time and aborts one transaction. The invariant holds.
Reproducible 5 of 5 across runs. Source: crates/bench/src/bin/write_skew_demo.rs.
what this is
A single file embedded database in Rust. Combines three things:

learned indexes piecewise linear models trained on keys, based on PGM-index (VLDB 2020). Smaller and faster than B-trees for many workloads.
deterministic simulation testing every nondeterminism source (clock, file IO, RNG, scheduling) goes through one trait. The database runs single threaded from a seed. Bugs are reproducible to the byte.
formal verification the WAL, MVCC, and SSI protocols are specified in TLA+ and checked with TLC. Specs live in the repo next to the code.

Shaped like SQLite but with serializable transactions, learned indexes, and a simulation harness.
receipts
Four bugs the simulator caught, each with a permanent regression test.

ADR-007 Tombstone in L0 not suppressing older L1 values after compaction. Seed 0xDEADBEEF, cycle 185. Regression: tombstone_regression_seed_0xdeadbeef_cycle_185.
ADR-023 SSI timestamp counter not synced after crash recovery, allowing the next transaction to commit at a timestamp earlier than a committed predecessor.
ADR-026 WAL transaction record magic prefix collision with legacy single-key records during recovery.
ADR-025 Stage 5b's MVCC and storage integration was reported done but did not ship. Caught during a verification audit, redone correctly, documented as a meta receipt.

Each ADR explains the bug, the fix, and the property the regression test checks. Full architectural history: DECISIONS.md.
quickstart
git clone https://github.com/yussypu/database
cd database
cargo build --release
The simulator finds and shrinks a bug:
cargo run --release --bin crackeddb -- sim demo
An interactive shell with named concurrent transactions:
cargo run --release --bin crackeddb -- shell --path /tmp/playground
> put alice on
ok ts=2
> begin t1
[txn t1]> begin t2
[txn t2]> get alice
on
[txn t2]> use t1
[txn t1]> put alice off
[txn t1]> commit
ok ts=4
[txn t2]> put alice updated
[txn t2]> commit
aborted_for_ssi (retry the operation)
The write skew demo:
cargo build --release --bin write_skew_demo
target/release/write_skew_demo
api
rustuse kv::{Db, Options};
use runtime::{Path, RealEnv};

fn main() -> Result<(), kv::Error> {
    let db = Db::open(RealEnv::new(), Path::new("/tmp/db"), Options::default())?;

    let mut txn = db.begin();
    txn.put(b"alice", b"on")?;
    txn.put(b"bob", b"on")?;
    let outcome = txn.commit()?;
    if outcome.aborted_for_ssi {
        // retry
    }

    let mut txn = db.begin();
    let value = txn.get(b"alice")?;
    assert_eq!(value.as_deref(), Some(b"on".as_slice()));

    for entry in txn.scan(b"a".as_ref()..b"z".as_ref())? {
        let (key, val) = entry?;
        // ...
    }
    txn.rollback();

    Ok(())
}
Transactions are serializable. Reads see a consistent snapshot at begin_ts. Writes go through the WAL with per commit fsync. Conflicts surface as CommitOutcome { aborted_for_ssi: true } rather than errors. Application code retries.
architecture
cli/        binary: shell, sim replay, sim find-and-shrink, sim demo
bench/      benchmark harness with crackeddb, rocksdb, lmdb, sled adapters
sim/        simulation harness, fault injection, shrinker, invariants
kv/         public API: Db, Txn, Scan, CommitOutcome, Error
mvcc/       SSI: read set tracking, reader tracker, dangerous structure
learned/    PGM index, learned bloom filters
storage/    LSM engine: WAL, memtable, SSTables, compaction, recovery
runtime/    the Env trait: deterministic IO, time, RNG, scheduling
runtime/ is the foundation. No crate above it may use std::time, std::fs, std::thread::sleep, or rand::random directly. CI enforces this. The trait has two implementations: RealEnv for production and SimEnv for the simulator.
verification
Three machine checked specs.

specs/Storage.tla WAL append, fsync, crash, recover. Invariant: no acknowledged write is ever lost.
specs/MVCC.tla Transaction begin, read, write, commit, abort. Invariant: snapshot isolation holds.
specs/SSI.tla rw-antidependency tracking and dangerous structure detection. Invariant: every committed schedule is serializable. Verified across 24.6M distinct states.

Each spec is referenced from the implementation file that realizes it, by name and by action.
benchmarks
Measured against RocksDB (TransactionDB Pessimistic), LMDB (heed, single writer), and sled (transactional) on a Hetzner CCX33: 8 dedicated AMD EPYC cores, 32 GB RAM, 240 GB NVMe. 8 workers, 60 second warmup, 120 second measurement, 3 runs per combination, median reported. Each backend runs in its native durability mode (see crates/bench/README.md for the per backend table).
TPC-C new order, 1 GB.
BackendThroughput (ops/sec)p50 (us)p99 (us)Abortscrackeddb417322558684714,685rocksdb42,8512276710lmdb20,47453690sled9,642114919790
TPC-C new order, 10 GB.
BackendThroughput (ops/sec)Abortscrackeddb5577,962rocksdb26,2440lmdb44615,768,325sleddeadlocked, excluded—
The aborts column is the headline. crackeddb is the only backend reporting serializability conflicts on a workload designed to produce them. Throughput pays the cost of detection. At 10 GB, LMDB's single writer architecture collapses with 15.7 million aborts. crackeddb's throughput is roughly constant from 1 GB to 10 GB.
YCSB, 1 GB scale, crackeddb vs RocksDB.
WorkloadcrackeddbRocksDBgapC (100% reads)130,257915,0647xB (95% reads)9,861765,96478xA (50/50)6,199567,80192x
Read throughput is within an order of magnitude of a production database. The gap on workloads with any meaningful write percentage is dominated by per commit fsync. Group commit would close most of it and is not yet implemented.
Methodology, per backend durability modes, and known failure modes (LMDB MDB_MAP_FULL on 10 GB YCSB, sled deadlock on sustained concurrent writes) in crates/bench/README.md. Raw results: bench-results.jsonl.
what this is not

A distributed database. Single node, embedded.
A SQL database. Key value with transactions. SQL is a future layer.
A production system. No replication, no online backup, no operational tooling, no client libraries beyond the Rust crate.
A high throughput OLTP engine. The per commit fsync ceiling is visible in every write heavy workload. Group commit is the obvious next optimization.

If you need distributed SQL, use CockroachDB or TiDB. If you need an embedded key value store today, use RocksDB or LMDB. crackeddb is for the combination above: serializable transactions, formal specs alongside the code, deterministic simulation, real receipts.
status
In development. v1 is when:

All seven crates feature complete and documented
All three TLA+ specs checked in CI on every PR
Simulator runs 1M+ seeds nightly without invariant violation
Group commit lands, the YCSB write gap closes to something defensible
Recovery passes the full fault injection matrix at 10 GB scale

Until v1, breaking changes happen freely. The verification commitments do not change.
reading
If you want to understand what this project is built on:

Will Wilson, Testing Distributed Systems with Deterministic Simulation (Strange Loop 2014).
O'Neil et al, The Log Structured Merge Tree (1996).
Ferragina and Vinciguerra, The PGM Index (VLDB 2020).
Cahill, Röhm, Fekete, Serializable Isolation for Snapshot Databases (SIGMOD 2008).
Hillel Wayne, Practical TLA+.

license
MIT OR Apache-2.0.
