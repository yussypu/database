# Development Journal

## 2026-05-25: Phase 6 Stage 1 benchmark harness complete

Stage 1 adds the benchmark harness infrastructure for comparing crackeddb
against RocksDB, LMDB, and sled.

Components:
- `crates/bench/` workspace crate with release-pinned dependencies
- Backend trait abstracting the four databases
- YCSB workloads A-F with scrambled Zipfian distribution
- TPC-C-shaped new order transaction workload
- Metrics collection: throughput, latency percentiles (HDRHistogram), write/space amp
- CLI with `ycsb`, `tpcc`, and `smoke` subcommands
- `scripts/bench_smoke.sh` runs all 28 combinations

Dependency compatibility notes (Rust 1.75 MSRV):
- rocksdb pinned to 0.20.1 (0.22 requires newer Rust)
- lmdb-rkv used instead of heed (lmdb-master-sys requires cargo:: syntax from 1.77)
- zstd-sys pinned to 2.0.9 to avoid rustc-hash 2.1.2 (requires 1.77)

Test coverage:
- 4 backend smoke tests
- 3 YCSB distribution/workload tests
- 2 TPC-C invariant tests
- 2 metrics collection tests
- 2 runner integration tests

All 28 smoke test combinations pass. This is harness-only; no real numbers
collected yet (that's Stage 2).

## 2026-05-25: Phase 3.6 version GC implemented

Phase 3.6 adds version garbage collection during compaction (ADR-027).

Key decisions:
- GC runs only during compaction, no background task
- Watermark = min(begin_ts) of active transactions, or current_ts if none
- Keep all versions > watermark (for active transactions)
- Keep newest version <= watermark (for new transactions)
- Discard older versions <= watermark
- Tombstones kept unconditionally (Phase 6 will add tombstone GC)

Implementation:
- SSITransactionManager.min_active_begin_ts() exposes watermark
- MergeIterator.new_with_gc(watermark) applies GC during merge
- LsmEngine.compact_with_gc(watermark_fn) runs GC-aware compaction
- Db.compact_with_gc() exposes GC through public API

Tests:
- 5 watermark unit tests in mvcc/ssi.rs
- 3 GC unit tests in storage/compaction.rs
- 2 integration tests in kv/lib.rs

The long-running transaction safety property is verified by
gc_preserves_versions_for_active_txn: starting a transaction,
writing new versions, running GC, and asserting the old snapshot
is still visible.

## 2026-05-25: ADR-020 superseded by ADR-023, stage 5b closes

The May journal described ADR-020 as documenting a txn_id collision across crash recovery. Stage 5b's verification revealed that the underlying integration the bug supposedly described didn't exist - VersionStore never wrote txn_ids to the WAL, so max_txn_id always returned 0, so the recovery fix was a no-op against a system that didn't exist in the configured shape.

The bug the integration test actually caught (and that ADR-023 now documents) was SSI's next_ts not being synced with engine.sequence() on Db::open. Same root cause area, different mechanism. Real bug, real fix, just attributed to the wrong line of code in May.

The receipts list as of stage 5b close:
- ADR-007: tombstone bug (phase 1, real)
- ADR-023: SSI timestamp sync (phase 3.7, real, supersedes ADR-020's narrative)
- ADR-026: WAL magic prefix (stage 5b, real)
- ADR-025: stage 5 regression caught in stage 5b (meta-receipt)

Stage 5b also revealed that the verification methodology has a known failure mode: an integration commit can ship "method exists with the right name and signature, body does the wrong thing" without the verification cycle catching it. The fix going forward is to write at least one test per integration that observes the *result* across the integration boundary (e.g., ssi_commit_writes_wal_records reads the WAL after a commit and asserts records exist). This gate is now permanent.
