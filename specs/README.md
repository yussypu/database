# TLA+ Specifications

This directory contains TLA+ specifications for the core protocols in cracked-db.
Each spec is machine-checked with TLC and referenced from the corresponding implementation code.

## Specs

### Storage.tla (~200 lines) - TODO

**State:** log entries, durable position, committed position, in-memory state

**Actions:**
- `Append`: Append a record to the log
- `Fsync`: Persist pending writes to durable storage
- `Crash`: Simulate a crash (lose in-memory state)
- `Recover`: Restore state from durable storage

**Invariant:** After `Recover`, durable state equals committed state at crash time.
No acknowledged write is lost.

### MVCC.tla (~400 lines) - TODO

**State:** active transactions, committed transactions, per-key version chains, read/write sets

**Actions:**
- `Begin`: Start a new transaction
- `Read`: Read a key at the transaction's snapshot
- `Write`: Buffer a write to the transaction's write set
- `Commit`: Attempt to commit the transaction
- `Abort`: Abort and roll back the transaction

**Invariant:** Snapshot isolation holds. No transaction sees a partial state of any other transaction.

### SSI.tla (~500-700 lines) - TODO

**State:** rw-antidependency edges between transactions, commit order

**Actions:**
- Track rw-edges when a transaction reads a version that another overwrites
- Detect dangerous structures (T1 → T2 → T3 with T3 committing first)
- Abort transactions to break cycles

**Invariant:** The commit order is serializable (no cycles in the dependency graph).

## Running the Specs

Install TLA+ Toolbox or use the command-line TLC:

```bash
# Check a spec
tlc Storage.tla -config Storage.cfg

# Check with specific number of workers
tlc -workers 4 SSI.tla -config SSI.cfg
```

## References

- Lamport, "Specifying Systems" (TLA+ reference)
- Hillel Wayne, "Practical TLA+" (practical guide)
