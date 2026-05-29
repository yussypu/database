# polish

small cleanup items, none blocking. add things as they come up. delete things
as they get done.

## shell ux

- after commit/rollback, if other txns are still open, auto-switch to the
  most-recently-used one (or at minimum, print a hint that other txns exist
  and how to switch). currently the prompt drops to `>` which is annoying
  when juggling concurrent txns.

## docs / adr

- ADR-018: the merge dedup logic in compaction.rs assumes consecutive
  grouping of versions per user_key, which ADR-018 explicitly warns may not
  hold for variable-length keys with byte overlap. add a TODO to either
  length-prefix the user_key in internal_key encoding or document the
  assumption explicitly in ADR-018.
- ADR-001: note the SSIAbort spec/code divergence. spec has SSIAbort as a
  separate action; code folds dangerous-structure detection into commit only.
  valid refinement, but worth documenting.
- ADR-022: scan signature drift. ADR says `RangeBounds<&[u8]>`, code uses
  `RangeBounds<Vec<u8>>`. fix one or the other.
- ADR-027: misleading "tombstones kept unconditionally" comment in
  compaction.rs:447. the code doesn't actually check for tombstones; the
  comment claims a stronger property than the code implements.
- demo footer in cli/src/main.rs should mention the shell now that it
  exists.

## code

- the `unsafe impl Send for Txn` in crates/kv/src/txn.rs needs a SAFETY
  comment explaining why each field is Send.
- wal.rs is a generic byte-stream log; the u64::MAX magic prefix and
  transaction record encoding live in engine.rs. add a one-line comment in
  wal.rs pointing at engine.rs for the higher-level record format.
- recovery in engine.rs silently discards orphan writes (TxnWrite without
  TxnBegin) and uncommitted transactions (TxnBegin + writes + no
  TxnCommit/TxnAbort). correct behavior, but no log line. add tracing::warn.
- engine.rs:1160 uses HashMap for pending_txn_writes during recovery.
  determinism-safe because only point lookups, but project convention is
  BTreeMap. swap for consistency.
- more MergeIterator multi-version tests. current coverage is 2 versions
  per key in a 3-key dataset. add: 5+ versions per key, mid-stream and
  end-of-stream boundary cases, multiple keys with multiple versions each.

## ci

- switch acceptance test invocations to `cargo test --exact <name>` so
  the dropped-#[ignore] / test-renaming pattern can't make tests filter
  out silently. add a smoke job that asserts the named acceptance tests
  exist via `cargo test --list | grep` before the acceptance suite runs.


