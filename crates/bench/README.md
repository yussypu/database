# crackeddb-bench

Benchmark harness for comparing crackeddb against RocksDB, LMDB, and sled.

## Usage

```bash
# Run a specific YCSB workload
cargo run --release --package crackeddb-bench --bin bench -- \
    ycsb --backend crackeddb --workload a --record-count 10000 --operation-count 10000 --path /tmp/bench

# Run TPC-C new order workload
cargo run --release --package crackeddb-bench --bin bench -- \
    tpcc --backend rocksdb --warehouses 1 --orders 1000 --path /tmp/bench

# Run smoke tests (all 28 combinations)
cargo run --release --package crackeddb-bench --bin bench -- smoke
```

## Workloads

### YCSB (Yahoo! Cloud Serving Benchmark)
- **Workload A**: 50% read, 50% update (Zipfian)
- **Workload B**: 95% read, 5% update (Zipfian)
- **Workload C**: 100% read (Zipfian)
- **Workload D**: 95% read latest, 5% insert
- **Workload E**: 95% scan, 5% insert (Zipfian)
- **Workload F**: 50% read, 50% read-modify-write (Zipfian)

### TPC-C (simplified)
- New order transaction only
- Tests multi-key read-modify-write patterns
- Exercises SSI conflict detection under contention

## Durability Modes

**Important**: The backends have different default durability guarantees, which
significantly affects benchmark numbers. When comparing results, consider whether
the comparison is fair given these differences.

| Backend   | Sync per Commit | Notes |
|-----------|-----------------|-------|
| crackeddb | **Yes**         | WAL fsync on every commit. Strictest durability. |
| rocksdb   | No              | WriteOptions sync=false by default. Data buffered in OS cache. |
| lmdb      | **Yes**         | commit() calls msync/fsync by default. |
| sled      | No              | sled deadlocks on explicit flush() under sustained concurrent writes (issues #1134, #1152). Auto-flushes every 500ms. Benchmarks reflect batched-durability mode. |

### Implications

- **crackeddb vs lmdb**: Fair comparison; both sync per commit.
- **crackeddb vs rocksdb**: Unfair; rocksdb is faster because it defers sync.
- **crackeddb vs sled**: Unfair on durability grounds. crackeddb syncs every commit; sled batches every 500ms. sled's higher throughput reflects this architectural choice, not a pure performance advantage. The writeup MUST call this out explicitly.

### Stage 2 Plan

Stage 2 will either:
1. Enable sync-per-commit on all backends for fair comparison, or
2. Run benchmarks in two modes ("fast" and "durable") and publish both

## Metrics

Each run outputs JSON with:
- `throughput_ops_sec`: Operations per second
- `latency_us`: p50, p90, p99, p999, max, mean (microseconds)
- `space_amp`: Disk size / logical data size
- `commits_success`: Number of successful commits
- `commits_aborted`: Number of aborted commits (SSI conflicts)

## Known Limitations (Stage 1)

1. **Serial execution only**: TPC-C runs transactions one at a time, so conflict
   detection isn't exercised. Stage 2 may add concurrent execution.

2. **No write amplification tracking**: The `write_amp` field is always null.
   Requires integration with each backend's internal metrics.

3. **No recovery time measurement**: The `recovery_ms` field is always null.
   Requires explicit crash-recovery testing.
