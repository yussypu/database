# crackeddb

An embedded OLTP database written in Rust. It exists to work through one question: how do serializable transactions, deterministic simulation, and formal specification fit together in a single storage engine. It is not finished and it is not meant for production. The interesting parts are the constraints it holds itself to, not the throughput numbers.

Transactions are serializable. The WAL, MVCC, and SSI protocols are specified in TLA+ and machine checked with TLC. Every source of nondeterminism routes through one trait, so a bug found from a seed reproduces byte for byte. Every architectural decision is written down in DECISIONS.md next to the regression test that pins it.

## Three ideas it puts in one place

**Learned indexes.** Piecewise linear models trained on the key distribution, following the PGM index (VLDB 2020). For some workloads these are smaller and faster than B trees.

**Deterministic simulation testing.** Clock, file IO, RNG, and scheduling all go through one trait. Under the simulator the database runs single threaded from a seed, so any bug the simulator finds is reproducible exactly and can be shrunk to a minimal failing schedule.

**Formal verification.** The WAL, MVCC, and SSI protocols are specified in TLA+ and checked with TLC. The specs live in the repo and each one is referenced from the implementation file that realizes it.

Roughly the shape of SQLite, with serializable transactions, learned indexes, and a simulation harness bolted on.

## A concrete example: write skew

Two transactions, two keys, one invariant: at least one doctor stays on duty.

```
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
```

Same scenario, two databases, side by side. RocksDB in its strongest serializable mode (TransactionDB Pessimistic) commits both transactions and lands in a state no serial schedule could produce. crackeddb detects the rw antidependency cycle at commit time and aborts one transaction, so the invariant holds. Reproducible 5 of 5 across runs. Source: crates/bench/src/bin/write_skew_demo.rs.

## Bugs the simulator caught

Each of these came out of the simulator and has a permanent regression test. They are the reason the project exists.

* **ADR-007.** A tombstone in L0 failed to suppress an older L1 value after compaction. Seed 0xDEADBEEF, cycle 185. Test: tombstone_regression_seed_0xdeadbeef_cycle_185.
* **ADR-023.** The SSI timestamp counter was not synced after crash recovery, so the next transaction could commit at a timestamp earlier than a committed predecessor.
* **ADR-026.** A WAL transaction record magic prefix collided with legacy single key records during recovery.
* **ADR-025.** Stage 5b's MVCC and storage integration was reported done but never shipped. Caught in a verification audit, redone, and documented as a meta receipt so the failure mode is on record.

Each ADR records the bug, the fix, and the property the regression test checks. Full history in DECISIONS.md.

## Quickstart

```
git clone https://github.com/yussypu/database
cd database
cargo build --release
```

The simulator finds and shrinks a bug:

```
cargo run --release --bin crackeddb -- sim demo
```

An interactive shell with named concurrent transactions:

```
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
```

The write skew demo:

```
cargo build --release --bin write_skew_demo
target/release/write_skew_demo
```

## API

```rust
use kv::{Db, Options};
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
```

Reads see a consistent snapshot taken at begin_ts. Writes go through the WAL with an fsync per commit. Conflicts surface as CommitOutcome with aborted_for_ssi set true rather than as errors, and the caller retries.

## Architecture

```
cli/        binary: shell, sim replay, sim find-and-shrink, sim demo
bench/      benchmark harness with crackeddb, rocksdb, lmdb, sled adapters
sim/        simulation harness, fault injection, shrinker, invariants
kv/         public API: Db, Txn, Scan, CommitOutcome, Error
mvcc/       SSI: read set tracking, reader tracker, dangerous structure
learned/    PGM index, learned bloom filters
storage/    LSM engine: WAL, memtable, SSTables, compaction, recovery
runtime/    the Env trait: deterministic IO, time, RNG, scheduling
```

runtime/ is the foundation. No crate above it may touch std::time, std::fs, std::thread::sleep, or rand::random directly, and CI enforces this. The trait has two implementations: RealEnv for production and SimEnv for the simulator.

## Verification

Three machine checked specs.

* **specs/Storage.tla.** WAL append, fsync, crash, recover. Invariant: no acknowledged write is ever lost.
* **specs/MVCC.tla.** Transaction begin, read, write, commit, abort. Invariant: snapshot isolation holds.
* **specs/SSI.tla.** rw antidependency tracking and dangerous structure detection. Invariant: every committed schedule is serializable. Verified across 24.6M distinct states.

Each spec is referenced from the implementation file that realizes it, by name and by action.

## Benchmarks

Measured against RocksDB (TransactionDB Pessimistic), LMDB (heed, single writer), and sled (transactional) on a Hetzner CCX33: 8 dedicated AMD EPYC cores, 32 GB RAM, 240 GB NVMe. 8 workers, 60 second warmup, 120 second measurement, 3 runs per combination, median reported. Each backend runs in its native durability mode.

crackeddb is slower than the mature engines on every write workload, which is expected. It pays for an fsync on every commit and it is the only backend that actually does the work of detecting serializability conflicts. The number worth reading is the abort column, not the throughput column.

TPC-C new order, 1 GB.

| Backend   | Throughput (ops/sec) | p50 (us) | p99 (us) | Aborts  |
|-----------|---------------------:|---------:|---------:|--------:|
| crackeddb |                4,173 |    2,255 |    8,684 | 714,685 |
| rocksdb   |               42,851 |       22 |      767 |      10 |
| lmdb      |               20,474 |       53 |       69 |       0 |
| sled      |                9,642 |    1,149 |    1,979 |       0 |

TPC-C new order, 10 GB.

| Backend   | Throughput (ops/sec) | Aborts      |
|-----------|---------------------:|------------:|
| crackeddb |                  557 |       7,962 |
| rocksdb   |               26,244 |           0 |
| lmdb      |                  446 |  15,768,325 |
| sled      |  deadlocked, excluded |          n/a |

crackeddb is the only backend reporting serializability conflicts on a workload designed to produce them. Its throughput stays roughly constant from 1 GB to 10 GB. LMDB's single writer architecture falls apart at 10 GB with 15.7 million aborts.

YCSB, 1 GB scale, crackeddb vs RocksDB.

| Workload        | crackeddb | RocksDB | gap |
|-----------------|----------:|--------:|----:|
| C (100% reads)  |   130,257 | 915,064 |  7x |
| B (95% reads)   |     9,861 | 765,964 | 78x |
| A (50/50)       |     6,199 | 567,801 | 92x |

Read throughput is within an order of magnitude of a mature engine. The gap on anything with write traffic is almost entirely the per commit fsync. Group commit would close most of it and is not implemented yet.

Methodology, per backend durability modes, and known failure modes (LMDB MDB_MAP_FULL on 10 GB YCSB, sled deadlock under sustained concurrent writes) are in crates/bench/README.md. Raw results in bench-results.jsonl.

## Scope

What it is not, stated plainly so the benchmarks are read in context:

* Not distributed. Single node, embedded.
* Not SQL. Key value with transactions. A SQL layer is a later problem.
* Not production. No replication, no online backup, no operational tooling, no client libraries beyond the Rust crate.
* Not a fast OLTP engine. The fsync per commit ceiling shows up in every write heavy result. Group commit is the obvious next step.

## Status

In development. v1 is reached when:

* All seven crates are feature complete and documented.
* All three TLA+ specs run in CI on every PR.
* The simulator runs 1M+ seeds nightly with no invariant violation.
* Group commit lands and the YCSB write gap closes to something defensible.
* Recovery passes the full fault injection matrix at 10 GB scale.

Until then, breaking changes happen freely. The verification commitments do not.

## Reading

The work this is built on:

* Will Wilson, Testing Distributed Systems with Deterministic Simulation (Strange Loop 2014).
* O'Neil et al, The Log Structured Merge Tree (1996).
* Ferragina and Vinciguerra, The PGM Index (VLDB 2020).
* Cahill, Rohm, Fekete, Serializable Isolation for Snapshot Databases (SIGMOD 2008).
* Hillel Wayne, Practical TLA+.

## License

MIT OR Apache-2.0.
