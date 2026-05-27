# Architecture Decision Records

This document records architectural decisions made during development. Each entry explains *why* a tradeoff was made, written for someone who joins the project in 18 months.

---

## ADR-001: SSI Dangerous Structure Detection Algorithm

**Date:** 2026-05-22
**Phase:** 3 (MVCC + SSI)
**Status:** Accepted

### Context

SSI (Serializable Snapshot Isolation) requires detecting "dangerous structures" - chains of two consecutive rw-antidependency edges that could lead to non-serializable schedules.

The original SSI paper (Cahill, Röhm, Fekete 2008) describes two approaches:
1. Track all rw-edges between transactions (memory-intensive)
2. Use summarized flags: `inConflict` and `outConflict` per transaction

### Decision

We implement **both approaches**:
- `rwEdges[t1][t2]` matrix tracks all edges (needed for serializability checking in tests)
- `inConflict[t]` / `outConflict[t]` flags provide O(1) dangerous structure check at commit time

The flags are set when:
- `inConflict[t]` = TRUE when a committed transaction has an rw-edge TO t
- `outConflict[t]` = TRUE when t has an rw-edge to a committed transaction

### Consequences

- **Pro:** Fast commit-time check (just check both flags)
- **Pro:** Full edge tracking enables post-hoc serializability verification
- **Con:** Memory usage is O(n²) for edge matrix, O(n) for flags
- **Con:** Flags may become stale if not carefully maintained

### Implementation Notes

The TLA+ spec revealed a subtle bug: flags weren't being updated when the edge source/target committed after the edge was created. The fix is to check actual `rwEdges` matrix in addition to flags, or update flags retroactively.

---

## ADR-002: SSI Reader Tracking with Global Timestamp Cutoff

**Date:** 2026-05-22
**Phase:** 3 (MVCC + SSI)
**Status:** Accepted

### Context

When a transaction commits, we need to find all transactions that read keys we're writing (to create rw-edges). The question is: what timestamp cutoff do we use?

### Decision

Use the **global timestamp counter** (read atomically before acquiring other locks) as the cutoff, not `txn.begin_ts + 1`.

### Rationale

Using `txn.begin_ts + 1` misses concurrent readers that started at the same timestamp or between begin and commit. By reading the global timestamp at commit time, we catch all active transactions.

### Consequences

- **Pro:** Correctly detects all concurrent readers
- **Pro:** Fixes write skew bugs where T2 commits first
- **Con:** Slightly more conservative (may flag edges that don't matter)

---

## ADR-003: Concurrent Transaction Stress Testing

**Date:** 2026-05-22
**Phase:** 3 (MVCC + SSI)
**Status:** Accepted

### Context

Initial stress tests ran transactions sequentially, resulting in 0% SSI abort rate - the tests weren't actually exercising conflict detection.

### Decision

Restructure stress tests to create **concurrent transactions in batches**:
1. Start 4 transactions at once (overlapping snapshots)
2. Interleave their operations (read-heavy rounds then write-heavy rounds)
3. Commit all at end of batch

### Consequences

- **Pro:** Actually exercises SSI (15-20% abort rate)
- **Pro:** Creates realistic rw-antidependencies
- **Con:** Test is less deterministic per-operation (but still deterministic overall via seed)

---

## ADR-004: TLA+ SSI Specification Structure

**Date:** 2026-05-22
**Phase:** 4 (TLA+ Specs)
**Status:** Accepted

### Context

The SSI spec needs to verify that no dangerous structures exist among committed transactions. The original spec had `SSICorrectness == TRUE` as a placeholder.

### Decision

Implement comprehensive dangerous structure detection with three cases in `SSICommit`:

1. **Case 1:** Committing transaction is the pivot (has outConflict + incoming edge from committed txn)
2. **Case 2:** A committed reader would become a pivot (has existing incoming edge from committed txn)
3. **Case 3:** Committing completes a chain where we're at the start (outConflict with target that has outgoing edge to another committed txn)

### Consequences

- **Pro:** TLC verification catches all dangerous structure scenarios
- **Pro:** Spec matches actual SSI implementation behavior
- **Con:** More complex SSICommit action (but correctness > simplicity for specs)

---

## ADR-005: Version Chain Storage Using BTreeMap

**Date:** 2026-05-22
**Phase:** 3 (MVCC + SSI)
**Status:** Superseded by ADR-025

### Context

Each key needs a version chain mapping commit timestamps to values. Options:
1. `Vec<(ts, value)>` - simple, O(n) lookup
2. `BTreeMap<ts, value>` - sorted, O(log n) lookup
3. `HashMap<ts, value>` - unordered, O(1) lookup

### Decision

Use **BTreeMap** keyed by commit timestamp.

### Rationale

- MVCC reads need to find the most recent version with `commit_ts <= snapshot_ts`
- BTreeMap's `range(..=ts).next_back()` provides this efficiently
- BTreeMap maintains order naturally as versions are added

### Consequences

- **Pro:** O(log n) snapshot reads
- **Pro:** Efficient range queries for GC
- **Con:** Higher memory overhead than Vec for small chains

**Note:** This decision was superseded by ADR-025 which replaced the in-memory BTreeMap with engine-backed storage.

---

## ADR-006: Drop async from Env trait

**Date:** 2026-05-21

**Status:** Accepted

**Context:**
Async spawn/sleep were unused and incorrectly implemented (no real scheduler,
no real wakers, deadlock-prone). Code review found the entire async surface dead:
- SimEnv::spawn polls synchronously and spin-loops with thread::yield_now on Pending
- The no-op waker means SimSleepFuture can never wake even though advance_time calls waker.wake()
- RealEnv::spawn spawns a thread that busy-polls with thread::sleep(100us) and a no-op waker
- Nothing in the codebase actually calls env.spawn() or env.sleep()

**Decision:**
Make Env fully synchronous. spawn returns a JoinHandle that wraps a real OS thread
in RealEnv and a cooperative task in SimEnv (to be implemented when first needed).
sleep is synchronous.

**Consequences:**
- Simpler runtime
- Lose async fn ergonomics — fine, we weren't using them
- Phase 5 (simulation harness) will model concurrency via the driver explicitly
  interleaving sync operations from a deterministic schedule, not via task interleaving
- spawn is unimplemented!() until something actually needs it

---

## ADR-007: LookupResult Enum for Tombstone-Aware Lookups

**Date:** 2026-05-21

**Status:** Accepted

**Context:**
During Phase 1 acceptance testing (1000 crash cycles), the simulator found a bug at cycle 185:
a key that had been deleted was returning its old value after crash recovery.

Root cause analysis revealed that `memtable.get()` returned `Option<Bytes>`:
- `Some(value)` when the key was found with a value
- `None` when the key was not found

The problem: `None` was returned for *both* "key not found" *and* "delete tombstone found".
The engine's `get_at()` method couldn't distinguish these cases, so when a tombstone existed
in the memtable but an older value existed in an SSTable, it would incorrectly search the
SSTable and return the stale value.

**Decision:**
Introduce a `LookupResult` enum with three variants:
```rust
pub enum LookupResult {
    Found(Bytes),  // Key found with this value
    Deleted,       // Key found but marked as deleted (tombstone)
    NotFound,      // Key not present in this data structure
}
```

Add a `lookup()` method to memtable that returns `LookupResult`. Update the engine's
`get_at()` to stop searching when `LookupResult::Deleted` is encountered.

**Rationale:**
- Type-safe distinction between absence and deletion
- Compiler enforces handling of all three cases
- Bug class eliminated at the type level
- Same pattern will be needed for SSTable lookups

**Consequences:**
- Slightly more verbose calling code (match instead of if-let)
- Original `get()` method retained for backward compatibility in tests
- This is exactly the kind of bug deterministic simulation testing is designed to find

**Bug Details:**
- Seed: 0xDEADBEEF
- Cycle: 185
- Key: k017
- Expected: None (deleted)
- Got: Some(b"v2968_c184") (stale value from SSTable)

---

## ADR-008: Defer Learned Bloom Filters to Phase 2b

**Date:** 2026-05-22

**Status:** Accepted

**Context:**
The Phase 2 implementation included a `LearnedBloomFilter` and `AdaptiveBloomFilter` that were supposed to implement the sandwiched learned bloom filter per Mitzenmacher (2018). Code review found these implementations are fundamentally broken:

1. The "learned model" is just a simple frequency check that provides no filtering benefit
2. The sandwiching logic (prefix filter → model → backup filter) doesn't actually reduce false positives
3. The "adaptive" filter just wraps the broken learned filter

The project spec explicitly allows falling back to classical bloom filters when the learned model isn't worth its memory.

**Decision:**
Delete `LearnedBloomFilter` and `AdaptiveBloomFilter`. Use only the classical `BloomFilter` in Phase 2. Defer learned bloom filters to Phase 2b when we have time to implement Mitzenmacher's algorithm correctly.

**Rationale:**
- The current implementation provides no benefit over classical bloom filters
- Shipping broken code is worse than shipping simpler correct code
- The fallback path is explicitly allowed in the spec
- Better to get Phase 2 correct than to ship false claims

**Consequences:**
- Simpler codebase (fewer types)
- No learned bloom filter benefit in Phase 2
- Clear TODO marker for Phase 2b implementation

---

## ADR-009: Bloom Filters on All SSTable Keys

**Date:** 2026-05-22

**Status:** Accepted

**Context:**
The Phase 2 SSTable implementation built bloom filters from block first keys only (used for PGM training). This is wrong—bloom filters need all keys to provide useful membership filtering.

Additionally, the bloom filter was never queried from the engine's read path. It was built but unused.

**Decision:**
1. Build bloom filter from ALL keys inserted into the SSTable, not just block first keys
2. Add `may_contain()` check in engine before reading SSTable blocks
3. Store bloom offset and size in the SSTable footer

**Rationale:**
- Bloom filters only work if they contain all keys
- Without querying the filter, it provides no benefit
- The footer format change allows skipping SSTables entirely on negative lookups

**Consequences:**
- Larger bloom filter per SSTable (scales with key count, not block count)
- Faster negative lookups (skip entire SSTable if bloom says no)
- Footer format change requires bumping FOOTER_MAGIC

---

## ADR-010: Numerically Stable PGM Prediction

**Date:** 2026-05-22

**Status:** Accepted

**Context:**
The original PGM `predict()` used the form:
```rust
slope * (key as f64) + intercept
```
where `intercept = start_pos - slope * key_start`. For keys near the upper end of u64 range, `slope * key` and `intercept` are large near-cancelling values whose addition loses precision in the f64 mantissa (53 bits). This caused predicted positions to drift outside the epsilon bound, which forced `sstable.rs::find_block_for_key` to "expand by 1 for safety" — masking the real issue.

**Decision:**
Rewrite Segment to store `{key_start, start_pos, slope}` and predict via:
```rust
let offset = key.saturating_sub(key_start) as f64;
slope * offset + (start_pos as f64)
```
The u128 subtraction happens at exact precision; the cast to f64 only loses precision on the (small) difference, not on the absolute key magnitudes. The `slope * offset` product stays bounded by the segment span, avoiding the catastrophic cancellation in the original form.

**Rationale:**
- No division means no division-by-zero risk
- Multiplication is more numerically stable than division
- The segment already knows its start position; computing it from intercept is unnecessary indirection

**Consequences:**
- Segment struct changes from `{slope, intercept}` to `{key_start, start_pos, slope}`
- Existing SSTables will need to be regenerated (file format change)

---

## ADR-011: Widen PGM Key Digest to u128

**Date:** 2026-05-22

**Status:** Accepted

**Context:**
The current `key_to_u64()` function takes only the first 8 bytes of a key. For keys that share the first 8 bytes (e.g., same-prefix keys like "user_1", "user_2", ..., "user_999999"), all keys hash to the same u64, causing degenerate PGM models with zero or near-zero slopes.

This caused the bug where SSTable lookups returned None for valid keys—the PGM prediction was meaningless because all keys in a block had the same digest.

**Decision:**
Change key digest from u64 to u128, reading the first 16 bytes of each key.

**Rationale:**
- 16 bytes captures more prefix diversity
- Still fast (two u64 loads and shifts)
- Eliminates the same-prefix collision for reasonable key lengths
- Keys shorter than 16 bytes are zero-padded (preserving sort order)

**Consequences:**
- PgmIndex and BlockIndex now use u128 internally
- Segment struct uses `key_start: u128`
- More accurate predictions for keys with shared prefixes
- File format change (SSTables must be regenerated)

---

## ADR-015: Internal Key Encoding for MVCC

**Date:** 2026-05-22
**Phase:** 3.5 (MVCC↔Storage Integration)
**Status:** Accepted

### Context

The LSM storage engine stores keys in sorted order. MVCC requires storing multiple versions of each key, identified by `(user_key, commit_ts)`. We need an encoding scheme that:
1. Groups all versions of a key together
2. Orders versions newest-first (most recent version at top)
3. Supports efficient prefix-seeking for snapshot reads

### Decision

Encode internal keys as: `user_key || (u64::MAX - commit_ts)` in big-endian byte order.

Format: `[user_key_bytes...][8-byte inverted timestamp]`

### Rationale

- **Inverted timestamp:** Subtracting from `u64::MAX` makes larger timestamps sort first. When seeking to a key at snapshot_ts, we seek to `user_key || (u64::MAX - snapshot_ts)` and take the first entry with inverted_ts >= (u64::MAX - snapshot_ts).
- **Big-endian:** Ensures lexicographic byte comparison matches numeric comparison for the timestamp portion.
- **Suffix encoding:** Placing timestamp at the end allows efficient prefix iteration over all versions of a key.

### Example

For user_key `"foo"` at commit_ts 100:
- Inverted timestamp: `u64::MAX - 100 = 18446744073709551515`
- Encoded as: `[0x66, 0x6f, 0x6f, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x9B]`

### Consequences

- **Pro:** Natural sort order for MVCC reads (newest first per key)
- **Pro:** Prefix seeking works with LSM engine bloom filters
- **Pro:** Simple implementation (concat + XOR)
- **Con:** 8 bytes overhead per key
- **Con:** Requires separator between user_key and timestamp if user_key is variable-length

### Implementation Notes

For variable-length user keys, we prepend a 4-byte length prefix OR use a length-prefixed encoding scheme. The current implementation assumes fixed-width keys for simplicity.

---

## ADR-016: WAL Record Format for MVCC

**Date:** 2026-05-22
**Phase:** 3.5 (MVCC↔Storage Integration)
**Status:** Accepted

### Context

The current WAL stores simple `(key, value)` records. For MVCC integration, we need to store:
1. Transaction boundaries (begin/commit/abort)
2. Version writes with commit timestamps
3. Enough information for crash recovery to rebuild MVCC state

### Decision

Extend the WAL record format with a type tag and structured payloads:

```
Record Types:
  0x01 TxnBegin   { txn_id: u64, begin_ts: u64 }
  0x02 TxnCommit  { txn_id: u64, commit_ts: u64 }
  0x03 TxnAbort   { txn_id: u64 }
  0x04 MVCCWrite  { txn_id: u64, key_len: u32, key: [u8], value_len: u32, value: [u8] }
  0x05 MVCCDelete { txn_id: u64, key_len: u32, key: [u8] }
```

### Rationale

- **Explicit boundaries:** `TxnBegin`/`TxnCommit` records allow recovery to identify transaction extents and commit timestamps.
- **txn_id linkage:** Each write records its owning `txn_id` so recovery can group writes by transaction.
- **Separate delete marker:** Tombstones are distinguished from null values.

### Recovery Algorithm

1. Scan WAL forward, building `pending: HashMap<txn_id, Vec<Write>>`
2. On `TxnCommit`: apply all pending writes to memtable with commit_ts, remove from pending
3. On `TxnAbort`: discard pending writes
4. At end: discard any remaining pending (uncommitted at crash time)

### Consequences

- **Pro:** Crash recovery is atomic per-transaction
- **Pro:** Supports read-your-writes within transaction (pending writes)
- **Pro:** Clear audit trail in WAL
- **Con:** More complex recovery logic
- **Con:** WAL records larger due to txn_id overhead

---

## ADR-017: GC/Compaction Filter for MVCC Versions

**Date:** 2026-05-22
**Phase:** 3.5 (MVCC↔Storage Integration)
**Status:** Accepted

### Context

MVCC accumulates old versions. Without GC, storage grows unbounded. We need to determine:
1. When a version can be safely deleted
2. How to integrate GC with LSM compaction
3. What the GC watermark should be

### Decision

Use a **compaction filter** approach with a configurable GC watermark:

1. Track `oldest_active_ts`: the minimum begin_ts of any active transaction
2. During compaction, for each key:
   - Keep the most recent version with commit_ts <= oldest_active_ts (snapshot baseline)
   - Keep all versions with commit_ts > oldest_active_ts (may be needed by active txns)
   - Delete older versions (superseded and unreachable)

### Watermark Calculation

```
gc_watermark = min(
    oldest_active_transaction.begin_ts,
    persisted_snapshot_ts  // if any long-running backup
)
```

### Example

For versions at timestamps [100, 80, 60, 40, 20] with gc_watermark = 50:
- Keep: [100, 80, 60] (all above or at watermark)
- Delete: [40, 20] (superseded and below watermark)

### Consequences

- **Pro:** Storage bounded by active transaction span
- **Pro:** Integrates naturally with LSM compaction (no separate GC thread)
- **Pro:** Respects long-running transactions and snapshots
- **Con:** Long-running transactions delay GC (mitigate with transaction timeout)
- **Con:** Compaction must decode internal keys to extract timestamps

### Implementation Notes

The compaction filter receives internal keys and can decode the timestamp suffix to apply the GC policy. Tombstones are kept until compaction reaches the bottommost level and the version is below gc_watermark.

### Implementation Status

**GC mechanism designed; implementation deferred to Phase 3.6.**

Phase 3.5 focuses on MVCC↔Storage integration (internal key encoding, WAL format, crash recovery). GC requires additional work:
- Tracking oldest active transaction across the system
- Integrating watermark calculation with compaction scheduling
- Handling tombstone retention at bottommost level

These are Phase 3.6 scope.

---

## ADR-018: Variable-Length User Keys May Not Group Consecutively

**Date:** 2026-05-23
**Phase:** 3.5 (MVCC↔Storage Integration)
**Status:** Accepted

### Context

With the internal key encoding `user_key || (u64::MAX - commit_ts)` (ADR-015), two user keys where one is a prefix of the other can interleave in encoded sort order.

### Example

For keys "foo" and "foop":
- `encode("foo", 0)` = `"foo" || [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]`
- `encode("foop", 0)` = `"foop" || [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]`

Byte-by-byte comparison at position 3: `0xFF` vs `'p'` (0x70). Since `0xFF > 0x70`, we get `encode("foo", 0) > encode("foop", 0)`.

This means a short key with a low timestamp (high inverted value) can sort AFTER a longer key.

### Decision

Accept this limitation for v1. Document it. The practical impact is that prefix scans cannot assume "all versions of key K appear consecutively" when there exist longer keys that start with K.

### Mitigations

1. Use `min_internal_key_for_user_key(k)` and `max_internal_key_for_user_key(k)` as range bounds for prefix scans.
2. If prefix-overlap proves problematic, options for v2:
   - Add a length-prefix byte to the encoding
   - Disallow user keys that are prefixes of other user keys
   - Use a separator byte smaller than any valid key byte (e.g., `\0` if keys are ASCII)

### Consequences

- **Pro:** Simple encoding, no length prefix overhead
- **Pro:** Most workloads don't have prefix-overlap patterns
- **Con:** Cannot iterate "all versions of K" without explicit bounds
- **Con:** Bloom filter prefix checks require care

---

## ADR-019: Empty Value Bytes Represent Tombstones

**Date:** 2026-05-24
**Phase:** 3.5 (MVCC↔Storage Integration)
**Status:** Accepted

### Context

The WAL and storage layer need a consistent way to represent deletions (tombstones) in the value field. Since `Bytes` cannot be `None` in Rust's type system without wrapping in `Option`, we need a sentinel value.

### Decision

An empty byte slice (`&[]` or `Bytes::new()`) in the value position represents a tombstone/deletion. This applies to:

1. **WAL records:** `WalPayload::Kv { value: Bytes::new(), .. }` means delete
2. **WAL transactional records:** `WalPayload::TxnWrite { value: Bytes::new(), .. }` means delete
3. **Recovery:** Empty value triggers `memtable.delete_with_seq()` instead of `put_with_seq()`

### Rationale

- **Simplicity:** No need for `Option<Bytes>` wrapper everywhere
- **Efficiency:** Zero allocation for tombstone marker
- **Precedent:** RocksDB uses similar approach with `kTypeDeletion` record types; we encode it in the value

### Consequences

- **Pro:** Uniform handling across WAL, memtable, SSTable layers
- **Pro:** No special case needed in serialization
- **Con:** Cannot store actual empty values (acceptable trade-off; real databases rarely need empty values)
- **Con:** Must document this convention clearly

### Code References

- `crates/storage/src/engine.rs:write()` - uses `value.unwrap_or(&[])`
- `crates/storage/src/engine.rs:recover()` - checks `if value.is_empty()`
- `crates/storage/src/wal.rs` - WAL payload encoding

---

## ADR-020: Transaction ID Restoration Across Crash Recovery

**Date:** 2026-05-24
**Phase:** 3.5 (MVCC↔Storage Integration)
**Status:** Accepted

### Context

After a crash, `SSITransactionManager` was initialized with `next_txn_id: 1`, causing transaction ID collisions with pre-crash transactions. This triggered false serializability violations in the checker because different logical transactions shared the same ID.

### Example

Pre-crash: T1, T2, T3 commit successfully.
Crash.
Post-crash: Manager restarts at `next_txn_id: 1`.
New transactions get IDs 1, 2, 3 — colliding with pre-crash T1, T2, T3.
SerializationChecker sees "T3" twice with different operations, triggering a spurious cycle.

### Decision

Track `max_txn_id` in the storage engine during WAL recovery, and initialize `SSITransactionManager` with `next_txn_id = max_txn_id + 1`.

**Implementation:**

1. `LsmEngine` maintains `max_txn_id: AtomicU64`
2. During `recover()`, scan all WAL records (TxnBegin, TxnWrite, TxnCommit, TxnAbort) and update `max_txn_id` to the maximum observed `txn_id`
3. `LsmEngine::max_txn_id()` returns this value
4. `SSITransactionManager::new()` initializes `next_txn_id` to `max(1, max_txn_id + 1)`

### Rationale

- **Correctness:** Transaction IDs must be globally unique within a database instance, including across crash boundaries
- **Symmetry:** Mirrors the existing `max_commit_ts` recovery pattern already implemented for timestamps
- **Simplicity:** Leverages existing WAL record structure (txn_id already present in records)

### Consequences

- **Pro:** Proper serializability verification across crash recovery
- **Pro:** Enables `txn_stress_with_crashes` test without ID offset workarounds
- **Pro:** Consistent with `max_commit_ts` pattern for timestamp recovery
- **Con:** Slightly more state tracked in engine (one additional atomic)

### Code References

- `crates/storage/src/engine.rs:recover()` - tracks max txn_id from WAL
- `crates/storage/src/engine.rs:max_txn_id()` - accessor method
- `crates/mvcc/src/ssi.rs:SSITransactionManager::new()` - initializes from max_txn_id

---

## ADR-021: CLI as Primary Demo Interface

**Date:** 2026-05-24
**Phase:** 5 (Simulation Harness)
**Status:** Accepted

### Context

The simulation harness needs a user facing entry point for:
1. Replaying specific seeds to reproduce bugs
2. Searching for failures and shrinking them
3. Demonstrating the shrinker to new users

Options considered:
1. Integration tests only (hidden from users)
2. Library API with examples (requires Rust knowledge)
3. CLI binary with subcommands (accessible to all)

### Decision

Add a `cli` crate with a `crackeddb` binary. Subcommands:
- `sim replay --seed=0x...` replays a specific seed
- `sim find-and-shrink --seeds=N` searches and shrinks
- `sim demo` runs synthetic bug injection demo

### Rationale

The CLI serves multiple purposes:
- README demo command (`crackeddb sim demo`) runs in under 1 second
- Bug reports can include seed for one command reproduction
- CI can run `find-and-shrink` to search for regressions
- Users can explore without writing Rust code

### Consequences

- **Pro:** Accessible demo for README and talks
- **Pro:** Deterministic reproduction via seed
- **Pro:** CI integration for regression testing
- **Con:** Additional crate to maintain
- **Con:** Binary size increases (clap dependency)

### Implementation Notes

Uses clap derive for argument parsing. Exit codes: 0 success, 1 failure found, 2 bad input. Output avoids em dashes and marketing language.

---

## ADR-022: kv Public API Design

**Date:** 2026-05-25
**Phase:** 3.7 (kv public API)
**Status:** Accepted

### Context

Phases 0 through 5 built the internals. The kv crate has been a stub. Users cannot currently open the database. This ADR locks the public API surface.

### Decisions

**Db is Clone via Arc\<DbInner\>.** Cloning is cheap (one Arc bump). Multiple Db handles can exist concurrently; each can produce its own transactions. There is one underlying engine and one SSITransactionManager per opened path.

**Db::open takes an explicit Env.** Rationale: deterministic simulation is a first class feature of this database. Hiding the env behind a default RealEnv would obscure the abstraction the rest of the project is built on. The user types `RealEnv::new()` in their first line.

**Txn holds Arc\<DbInner\> internally and is not generic over a lifetime.** Each method on Txn does at most one Arc clone on a hot path. Transactions are Send (so they can move across threads) but not Sync (so two threads cannot operate on the same transaction simultaneously).

**commit returns Result\<CommitOutcome, Error\>.** CommitOutcome carries the commit timestamp and a boolean indicating whether an SSI abort occurred. The semantics are:

- If commit returns `Ok(CommitOutcome { aborted_for_ssi: false, .. })`: success, data was committed
- If commit returns `Ok(CommitOutcome { aborted_for_ssi: true, commit_ts: 0 })`: SSI told you to retry, no data was written
- If commit returns `Err(...)`: the database is in trouble, do not retry blindly

SSI conflicts are NOT Errors. They surface as `Ok(CommitOutcome { aborted_for_ssi: true })` because they tell the user "retry the transaction" which is fundamentally different from "something went wrong, the database may be in trouble."

**Error type is a public enum** in kv::Error with:

- `Io(io::Error)`: anything from the storage layer
- `Corruption(String)`: data on disk is wrong
- `AlreadyOpen`: a Db for this path is already open
- `NotFound`: path does not exist on open
- `InvalidArgument(String)`: bad arguments to put/get/scan

SSI conflicts are NOT in Error. They are in CommitOutcome. This is strict.

**Iteration uses a separate Scan type** returned by Txn::scan. Scan implements `Iterator<Item = Result<(Bytes, Bytes), Error>>`. The iterator holds a borrow on the Txn (so it is lifetime scoped to the Txn). Range bounds use `std::ops::RangeBounds<&[u8]>`.

**Options is a builder pattern struct** marked `#[non_exhaustive]` so we can add fields without breaking semver:

```rust
Options::default()
    .with_block_size(4096)
    .with_wal_segment_size(64 * 1024 * 1024)
    .with_use_pgm_index(true)
```

### Public API Surface

Exhaustive list of public items in kv:

- `Db`, `Db::open`, `Db::begin`, `Db::path`, `Db::flush`, `Db::close`
- `Txn`, `Txn::get`, `Txn::put`, `Txn::delete`, `Txn::scan`, `Txn::commit`, `Txn::rollback`
- `Scan` (iterator)
- `CommitOutcome { pub commit_ts: u64, pub aborted_for_ssi: bool }`
- `Options` (builder)
- `Error` (enum)
- `Result = std::result::Result<T, Error>`

Nothing else is public. Internal types (DbInner, the wrapped engine and SSI manager) are `pub(crate)` at most.

### Consequences

Users can open the database with one line, run transactions, see whether SSI aborted them, and retry. The API surface is small enough to keep the simulator the source of truth and avoid feature creep.

The decision to put SSI aborts in CommitOutcome rather than Error means every commit site has to match on the outcome and check the flag. This is slightly more verbose than `?` propagation but makes retry semantics explicit, which matters for a database that promises serializable transactions.

### Alternatives Considered

- **Lifetime parameterized Txn<'db>.** Rejected because it forces every API user to thread lifetimes through their code, and the Arc cost is measured in nanoseconds.
- **Implicit RealEnv.** Rejected because the determinism story is the project's headline feature.
- **SSI conflicts in Error.** Rejected because retry semantics are structurally different from error semantics.

---

## ADR-023: SSI Timestamp Synchronization on Crash Recovery

**Date:** 2026-05-25
**Phase:** 3.7 (kv integration)
**Status:** Accepted

### Context

The database has two timestamp domains:
1. **SSI timestamps** (`SSITransactionManager.next_ts`): used for MVCC snapshots and conflict detection
2. **Storage sequence numbers** (`LsmEngine.sequence`): used for versioning in memtable/SSTables

During normal operation, both start at 1 and increment independently. After a crash:
- The storage engine recovers from WAL and restores its sequence to the highest replayed value
- The SSI manager is freshly constructed, starting at 1

This creates a bug: if data was committed at storage sequence 100, and the SSI manager starts at 1, new transactions with `begin_ts=1` cannot see data at sequence 100 (because `get_at(key, 1)` only sees versions with seq <= 1).

### Decision

Add `SSITransactionManager::new_with_start_ts(versions, start_ts)` to allow the kv layer to synchronize timestamps on open:

```rust
let start_ts = engine.sequence();
let ssi_manager = SSITransactionManager::new_with_start_ts(version_store, start_ts);
```

### Rationale

The fix is minimal: one new constructor with a starting timestamp parameter. The kv layer (which owns both the engine and SSI manager) is the right place to perform synchronization.

Alternative considered: have SSI manager persist its timestamp. Rejected because:
- VersionStore is in-memory only (intentionally)
- Adding persistence to SSI would duplicate storage layer's job
- The engine already persists sequence in MANIFEST

### Consequences

- **Pro:** Crash recovery is correct (new transactions see all recovered data)
- **Pro:** Minimal change (17 lines in ssi.rs, 2 lines in db.rs)
- **Con:** kv layer must remember to synchronize on open

### Related

- ADR-002 (timestamp coordination during reader tracking)
- ADR-022 (kv API design, particularly Db::open)

### Test

`sim::txn_stress::txn_stress_with_crashes_via_kv` verifies that data written before a crash is visible after recovery.

---

## ADR-024: Two-Tier Storage Architecture (VersionStore + Engine)

**Date:** 2026-05-25
**Phase:** 3.7 (kv integration)
**Status:** Superseded by ADR-025

### Context

The project spec and TLA+ specs (MVCCStorage.tla) describe an architecture where MVCC writes directly to the storage engine. However, the actual implementation has two separate storage tiers:

1. **VersionStore** (in `mvcc::version`): Purely in-memory `RwLock<BTreeMap<Bytes, VersionChain>>`
2. **LsmEngine** (in `storage::engine`): On-disk LSM tree with WAL

The question arose whether this is a "double-write bug" or intentional architecture.

### Investigation

Three checks were performed:

1. **VersionStore struct fields**: ONLY `chains: RwLock<BTreeMap<...>>`. No engine field.
2. **install_writes implementation**: Only writes to internal BTreeMap. No engine calls.
3. **Test `commit_writes_exactly_one_version_per_key_in_engine`**: Found exactly 1 version per key in engine, written by kv layer's `engine.put()`.

### Decision

The two-tier architecture was initially considered **intentional and correct**.

### Superseded

**This ADR was superseded by ADR-025.** The two-tier architecture was identified as a regression from the original spec. Phase 3.5 Stage 5b rewrote VersionStore to be engine-backed, eliminating the in-memory BTreeMap layer.

---

## ADR-025: VersionStore Engine Integration (Stage 5b)

**Date:** 2026-05-25
**Phase:** 3.5 (MVCC + Storage Integration)
**Status:** Accepted

### Context

In May, Phase 3.5 Stage 5 closed with four commits claiming to integrate VersionStore with the LSM engine. The commits added methods named `wal_append_txn_begin`, `wal_append_txn_write`, `wal_append_txn_commit`, and `install_writes` to VersionStore. The methods exist with correct signatures but their bodies operate only on an in-memory BTreeMap; no engine writes happen on the MVCC path.

The discrepancy was discovered in Phase 3.7 while investigating an apparent "double-write" in the kv layer. The investigation revealed that kv was the ONLY path to disk, because MVCC's claimed engine integration didn't exist.

The actual VersionStore on main was:

```rust
pub struct VersionStore {
    chains: RwLock<BTreeMap<Bytes, VersionChain>>,
}
```

Purely in-memory. `install_writes` calls `add_version` on a BTreeMap entry. No engine field, no WAL writes from MVCC, no `put_versioned` calls.

This ADR documents the regression and the redo.

### Decisions

1. **VersionStore is rewritten to be engine-backed.** The in-memory BTreeMap goes away. Reads route through `Engine::get_at`, writes route through `Engine::put_versioned`, conflict detection through `Engine::has_write_after`.

2. **The wal_append_txn_* methods on VersionStore now actually call the corresponding methods on Engine.** The current versions are no-ops against the in-memory map and will be deleted.

3. **The kv layer's Txn::commit stops calling engine.put directly.** Commits route entirely through `SSITransactionManager::commit`, which writes to WAL and then to the engine. The kv layer's role is API surface and lifetime management, not persistence.

4. **Stage 5b's verification includes a test that reads the WAL after commit and asserts expected records are present.** This is the test that would have caught Stage 5's regression. It is now a permanent CI gate.

### Why Stage 5 didn't catch this

The verification cycle for Stage 5 checked that the methods existed with the right names and signatures. It did not check that the method bodies performed engine writes. A method named `wal_append_txn_write` that just mutates a BTreeMap passes a "the API surface is correct" review without passing a "the integration is real" review.

**The verification gap:** no test in Stage 5 read the WAL after a commit and asserted the expected records were present. A test of that shape would have caught the regression immediately.

Stage 5b adds that test (Part D: `ssi_commit_writes_wal_records`) and makes it a permanent CI gate.

### Consequences

- MVCCStorage.tla composition now corresponds to running code
- Crash recovery restores MVCC state from WAL (max_commit_ts, max_txn_id)
- txn_stress_with_crashes tests real MVCC crash recovery
- kv layer is simpler (no direct engine writes)
- Read latency potentially higher (every snapshot read now goes to storage, not in-memory map). Measured in Part F.
- Future GC (Phase 3.6) operates on the integrated layer, not two separate tiers

### Alternatives Considered

**Two-tier design (keep BTreeMap as a hot cache in front of engine):** Possible but adds complexity. Defer to Phase 6 if benchmarks show read latency is a problem.

**Acknowledge two-tier as final design:** Rejected because MVCCStorage.tla doesn't match code, and the project's value proposition is verified-against-spec.

### Acknowledged spec/code differences

MVCCStorage.tla models WAL_BEGIN and WAL_ABORT records as explicit actions. The Rust code optimizes both out:
- **WAL_BEGIN:** begin is implicit in the first TxnWrite record
- **WAL_ABORT:** abort discards the transaction without writing a record; recovery sees no TxnCommit and naturally discards

Both optimizations preserve correctness. The spec is intentionally more general than the code. Future spec refinement may make this explicit by adding an "optimized" variant of MVCCStorage.tla, but for now the gap is documented here.

---

## ADR-026: WAL Magic Prefix for Transaction Records

**Date:** 2026-05-25
**Phase:** 3.5 (MVCC + Storage Integration)
**Status:** Accepted

### Context

Stage 5b introduced transaction records in the WAL for crash recovery:
- `TxnBegin(txn_id)`: marks transaction start
- `TxnWrite(txn_id, key, value)`: buffered write
- `TxnCommit(txn_id, commit_ts)`: marks commit with timestamp
- `TxnAbort(txn_id)`: marks abort

These records needed to coexist with legacy KV records (Put/Delete) which have the format:
```
[seq(8 bytes)][key_len(4 bytes)][key][type(1 byte)][value_len(4 bytes)][value]
```

The first 8 bytes of a KV record are a sequence number starting at 1. Transaction IDs also start at 1. During recovery, the decoder needs to distinguish between:
- `[1][...]` = legacy KV record with seq=1
- `[1][...]` = TxnBegin record with txn_id=1

Without differentiation, the decoder cannot reliably parse the WAL after a crash.

### The Bug

Initial Stage 5b implementation used the same format prefix for both:
```rust
fn encode_txn_begin_record(txn_id: u64) -> Vec<u8> {
    let mut buf = Vec::with_capacity(9);
    buf.extend_from_slice(&txn_id.to_le_bytes());  // 8 bytes
    buf.push(WAL_TYPE_TXN_BEGIN);                  // 1 byte
    buf
}
```

A `TxnBegin(1)` record starts with `[1, 0, 0, 0, 0, 0, 0, 0, ...]`
A legacy KV record with seq=1 starts with `[1, 0, 0, 0, 0, 0, 0, 0, ...]`

The decoder would misparse transaction records as KV records, causing:
- Lost transactions after crash
- Corrupted recovery state
- Silent data loss

### Decision

Prefix all transaction records with `u64::MAX` (0xFFFFFFFFFFFFFFFF) as a magic number:

```rust
const WAL_TXN_MAGIC: u64 = u64::MAX;
const WAL_TYPE_TXN_BEGIN: u8 = 0x01;
const WAL_TYPE_TXN_WRITE: u8 = 0x02;
const WAL_TYPE_TXN_COMMIT: u8 = 0x03;
const WAL_TYPE_TXN_ABORT: u8 = 0x04;

fn encode_txn_begin_record(txn_id: u64) -> Vec<u8> {
    let mut buf = Vec::with_capacity(17);
    buf.extend_from_slice(&WAL_TXN_MAGIC.to_le_bytes());  // 8 bytes: magic
    buf.push(WAL_TYPE_TXN_BEGIN);                         // 1 byte: type
    buf.extend_from_slice(&txn_id.to_le_bytes());         // 8 bytes: txn_id
    buf
}
```

### Rationale

`u64::MAX` as a sequence number is impossible in normal operation:
- Sequence starts at 1 and increments by 1 per operation
- Even at 1 billion ops/second, reaching u64::MAX takes ~584 years
- No collision risk with legacy records

The magic prefix allows deterministic disambiguation:
1. Read first 8 bytes as u64
2. If value == u64::MAX: parse as transaction record
3. Otherwise: parse as legacy KV record

### Consequences

- **Pro:** Reliable WAL recovery after crash
- **Pro:** Forward compatible (old WALs still readable)
- **Pro:** No migration needed for existing data
- **Con:** Transaction records are 8 bytes larger (17 vs 9 bytes for TxnBegin)
- **Con:** Slightly more complex parsing logic

### Verification

The `ssi_commit_writes_wal_records` test verifies:
1. Transaction records are written to WAL with magic prefix
2. Recovery correctly parses and replays transaction records
3. max_commit_ts and max_txn_id are restored from WAL

---

## ADR-027: Version Garbage Collection During Compaction

**Date:** 2026-05-25
**Phase:** 3.6 (Version GC)
**Status:** Accepted

### Context

MVCC version chains grow unboundedly without GC. A key written N times has N versions on disk regardless of whether any transaction can still read the old ones. Reads walk longer chains, storage grows, benchmarks against RocksDB/LMDB will look slow for reasons unrelated to the engine's design.

Phase 3.6 adds version GC. Phase 6 benchmarks come after.

### Decisions

GC runs inside the existing leveled compaction path. When compaction reads a key's version chain to merge SSTables, it also drops versions whose commit_ts is below the current watermark. No new background task.

The watermark is min(begin_ts) over all currently-active transactions, or next_ts if no transactions are active. Versions strictly older than the watermark are unreachable by any future read.

SSITransactionManager exposes `min_active_begin_ts()` returning the watermark. The compaction code reads this once at the start of each compaction job and uses it as a fixed cutoff for that job. Transactions that begin during compaction are correctly served because their begin_ts > the watermark, so they only need versions that compaction is keeping anyway.

### Watermark Math

For each user_key encountered during compaction:
1. Collect all versions sorted by commit_ts descending (newest first)
2. Find the first version with commit_ts <= watermark (call it V)
3. Keep V (it's the version any future snapshot read at watermark-or-later will need)
4. Keep all versions with commit_ts > V.commit_ts (they're newer than V, may be needed by future transactions)
5. Drop all versions older than V (no transaction can ever ask for them)

Special case: if V is a tombstone (deletion), keep it for now; tombstone-collapsing is a separate optimization deferred to Phase 6 polish.

### Why Not Background GC

- Compaction already touches every key. Adding GC is essentially free.
- One less concurrent thing for the simulator to model.
- GC pacing tied to write throughput is correct for OLTP: workloads that write a lot generate more dead versions and trigger more compaction.
- Read-only workloads don't write, don't compact, don't GC — and don't need to, because their version chains aren't growing.

### Why min(begin_ts) Is the Right Watermark

- Any transaction with begin_ts >= watermark can only need versions with commit_ts <= begin_ts, which means commit_ts can be anywhere from 0 up to begin_ts.
- For commit_ts > watermark: keep, may be needed.
- For commit_ts <= watermark: keep at most ONE such version per key (the newest), because that's what any read at begin_ts in [watermark, next_ts] will see.
- Versions older than the newest-below-watermark are unreachable: a read at any begin_ts >= watermark sees the newest-below-watermark; a read at a smaller begin_ts can't exist because watermark is the minimum.

### Long-Running Transactions

A long-running transaction with low begin_ts holds the watermark down, preventing GC of versions in its snapshot range. This is correct behavior — the transaction needs those versions. It's also a known operational hazard (analytics queries can pin GC indefinitely). Phase 3.6 accepts this trade-off. Phase 6+ may add a "snapshot too old" mechanism (per PostgreSQL) that aborts transactions whose snapshot has been pinned too long, but that's an API change requiring its own ADR.

### Tombstone Handling (and Phase 6 Deferral)

When a key is deleted, MVCC writes a tombstone (version with value=None). The tombstone must be kept as long as older versions of that key exist elsewhere in the LSM (it suppresses them on read). When all older versions are GC'd, the tombstone itself becomes droppable.

Phase 3.6 keeps tombstones unconditionally. Tombstone-collapsing requires coordinating across LSM levels and is deferred.

### Consequences

- Version chains bounded by concurrent snapshot count, not by write count
- Compaction does more work per key (read whole chain, decide what to drop)
- Need a new test class: GC correctness (under various active-txn patterns)
- Hot keys benchmarked in Phase 6 will look reasonable
- Long-running txns hold GC back; document this in user-facing docs eventually

---

## ADR-028: Scan Implementation and Phantom Write Limitation

**Date:** 2025-05-25
**Phase:** 6 (Benchmarks and Polish)
**Status:** Accepted

### Context

ADR-022 specified scan as part of the kv public API but left it Unimplemented in Phase 3.7 because the engine lacked snapshot-aware range iteration and the semantic edges around range reads needed thought. Phase 6 benchmarks (YCSB workload E) require scan, so this ADR locks the design.

### Decisions

1. **Scan iterates the merged view** of (memtable + immutable memtables + SSTable levels) in user_key order. For each user_key in the range, the newest version with commit_ts <= snapshot_ts is selected. Tombstones at that version suppress the key (iterator skips it).

2. **The txn's buffered writes are layered on top of the storage view:**
   - Buffered put: iterator yields the buffered value instead of storage
   - Buffered delete: iterator skips the key
   - Range covers a key with no buffered write: storage view is returned

3. **Read set tracking:** each user_key returned by the iterator is added to the txn's read set, same as a point get(). This makes the keys returned participate in SSI conflict detection.

4. **Phantom writes are NOT detected.** If a scan returns no keys in range [a, b) and a concurrent txn commits an insert of key c where a <= c < b, the scan's txn can still commit even though re-running the scan would now return c. This matches postgres REPEATABLE READ / CockroachDB SSI behavior. Predicate locking would close this gap but is outside the project's scope and outside what the TLA+ SSI spec models.

5. **Range bounds use `std::ops::RangeBounds<&[u8]>`**, fixing the ADR-022 drift where the stub used `RangeBounds<Vec<u8>>`.

### Consequences

- YCSB workload E can run against crackeddb.
- The serializability story holds for keys returned. Range read anomalies (phantom writes) are a known gap consistent with industry SI/SSI implementations.
- The TLA+ spec doesn't model range reads. The phantom limitation means no spec divergence: code and spec agree that only point reads create rw-edges.

### Alternatives Considered

- **Range-read tracking (next_key_after the scan's end bound also recorded as a phantom marker):** more correct, but extends beyond TLA+ spec and requires predicate locking semantics the project doesn't claim. Deferred to a hypothetical Phase 7.
- **Predicate locks:** out of scope. Would require redesigning SSI to track predicates not just keys.

---

*Add new decisions above this line.*
