# crackeddb (in development)

An embedded OLTP database where every index is learned, every execution is deterministically replayable, and the core protocols are machine verified.

## what is this

A single file embedded database in Rust. Combines three things:

1. **learned indexes** piecewise linear models trained on keys, based on PGM-index (VLDB 2020). Smaller and faster than B-trees for many workloads.
2. **deterministic simulation testing** every nondeterminism source (clock, file IO, RNG, scheduling) goes through one trait. The database runs single threaded from a seed. Bugs are reproducible to the byte.
3. **formal verification** the WAL, MVCC, and SSI protocols are specified in TLA+ and checked with TLC. Specs live in the repo next to the code.

Shaped like SQLite but with serializable transactions, learned indexes, and a simulation harness.

---

## demo

Replay a bug the simulator found, deterministically, on any machine:

```
$ crackeddb sim demo
searching scenario=Chaos workload_len=50 max_seeds=1
failure found at seed=0x00000000DEADBEEF after 1 seeds (126us)

original failure
  kind SerializabilityCycle(txns=[1,2,3])
  scenario Chaos
  workload_len 50

shrinking
  axis=partial_write_prob before=0.30 after=0.00 (accepted)
  axis=slow_write_prob before=0.20 after=0.00 (accepted)
  axis=clock_skew_prob before=0.40 after=0.00 (accepted)
  axis=process_pause_prob before=0.20 after=0.00 (accepted)
  axis=disk_full_threshold before=5242880 after=0 (accepted)
  axis=workload_len before=50 after=15 (accepted)

minimal reproducer
  seed 0x00000000DEADBEEF
  scenario None
  workload_len 15
  replays 37

reproduce with
  crackeddb sim replay --seed=0xDEADBEEF --scenario=None --workload-len=15

synthetic bug injected at op 15 for this demo. real bugs caught by the
simulator are documented in DECISIONS.md (ADR-007, ADR-020).
```

This is not a recording. It is the actual execution. Same seed, same trace.

## try it

```rust
use kv::{Db, Options};
use runtime::RealEnv;

fn main() -> Result<(), kv::Error> {
    // Explicit Env: deterministic simulation is a first-class feature
    let env = RealEnv::new();
    let db = Db::open(env, "/tmp/crackeddb", Options::default())?;

    // Write transaction
    let mut txn = db.begin();
    txn.put(b"user:42", b"alice")?;
    txn.put(b"user:43", b"bob")?;
    let outcome = txn.commit()?;

    // SSI conflict? Retry.
    if outcome.aborted_for_ssi {
        println!("Transaction aborted due to serialization conflict, retrying...");
    }

    // Read transaction
    let mut txn = db.begin();
    let value = txn.get(b"user:42")?;
    assert_eq!(value.as_deref(), Some(b"alice".as_slice()));
    txn.rollback();

    Ok(())
}
```

Transactions are serializable via SSI. Reads see a consistent snapshot. Writes go through WAL with group commit. Conflicts are detected at commit time and surface as `CommitOutcome { aborted_for_ssi: true }`, not as errors.

## architecture

```
sim/       simulation harness, fault injection, invariants, shrinker
kv/        public API: get / put / scan / transaction
mvcc/      multiversion concurrency control, serializable snapshot isolation
learned/   PGM index, sandwiched learned bloom filters
storage/   LSM engine: WAL, memtable, SSTables, compaction, recovery
runtime/   the Env trait: deterministic IO, time, RNG, scheduling
cli/       binary with sim replay, find-and-shrink, demo commands
```

`runtime/` is the foundation. No crate above it may use `std::time`, `std::fs`, `std::thread`, `std::sync::Mutex`, or `rand::random` directly. CI enforces this. The trait has two implementations: `RealEnv` for production and `SimEnv` for simulation.

See [`DECISIONS.md`](DECISIONS.md) for architectural decisions and [`specs/`](specs/) for TLA+ specifications.

## what this is not

- A distributed database. Single node, embedded.
- A SQL database. Key value with transactions. SQL is a future layer.
- A general purpose multi tenant system. One process, one database.

If you need distributed SQL use CockroachDB or TiDB. If you need embedded KV today use RocksDB or LMDB. This is for people who care about the combination above.

## verification

Three TLA+ specifications, machine checked with TLC:

- [`specs/Storage.tla`](specs/Storage.tla) WAL append, fsync, crash, recover. Invariant: no acknowledged write lost.
- [`specs/MVCC.tla`](specs/MVCC.tla) transaction begin, read, write, commit, abort. Invariant: snapshot isolation holds.
- [`specs/SSI.tla`](specs/SSI.tla) rw antidependency tracking and dangerous structure detection. Invariant: commit order is serializable.

Each spec is referenced from its implementation by a code comment. CI runs TLC on all three on every PR.

## building

```
git clone https://github.com/yussypu/crackeddb
cd crackeddb
cargo build --release
```

Requires Rust 1.75 or later. No system dependencies.

Run the test suite:

```
cargo test --all
```

Run the simulator demo:

```
cargo run --release --bin crackeddb -- sim demo
```

Replay a specific seed:

```
cargo run --release --bin crackeddb -- sim replay --seed=0xDEADBEEF
```

Find failures and shrink them:

```
cargo run --release --bin crackeddb -- sim find-and-shrink --seeds=1000
```

## interactive shell

An interactive REPL for exploring transactions and SSI behavior:

```
cargo run --release --bin crackeddb -- shell --path=/tmp/mydb
```

Commands in auto-commit mode:

```
crackeddb> put mykey myvalue
ok
crackeddb> get mykey
myvalue
crackeddb> delete mykey
ok
crackeddb> info
begin_ts: 5
active_txns: 0
crackeddb> watermark
3
```

Explicit transaction mode for SSI demos:

```
crackeddb> begin t1
ok: began transaction t1
crackeddb> begin t2
ok: began transaction t2
crackeddb> use t1
ok: switched to t1
crackeddb> put counter 100
ok
crackeddb> use t2
ok: switched to t2
crackeddb> get counter
(not found)
crackeddb> put counter 200
ok
crackeddb> commit
ok: committed at ts=5
crackeddb> use t1
ok: switched to t1
crackeddb> commit
ok: aborted for ssi conflict, retry
```

Keys and values can be hex-encoded with 0x prefix: `put 0xDEADBEEF 0xCAFE`

Check the TLA+ specs (requires Java and TLC):

```
./scripts/check-specs.sh
```

## status

Under active development. v1 is when:

- All six crates implemented per spec
- All three TLA+ specs machine checked, committed, referenced from code
- Simulator runs 1M+ seeds nightly with no invariant violations
- YCSB A-F published against RocksDB, LMDB, sled
- Recovery passes the full fault injection matrix

Until v1, breaking changes happen freely. The architectural commitments above stay.

## reading

If you want to understand what this project is built on:

1. Will Wilson, Testing Distributed Systems w/ Deterministic Simulation (Strange Loop 2014)
2. O'Neil et al, The Log Structured Merge Tree (1996)
3. Ferragina and Vinciguerra, The PGM index (VLDB 2020)
4. Cahill, Rohm, Fekete, Serializable Isolation for Snapshot Databases (SIGMOD 2008)
5. Hillel Wayne, Practical TLA+

## license

MIT OR Apache-2.0
