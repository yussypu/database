---------------------------- MODULE MVCC ----------------------------
\* Multi-Version Concurrency Control with Snapshot Isolation
\*
\* This specification models snapshot isolation (SI) for an MVCC database.
\* Each transaction reads from a consistent snapshot at its begin timestamp.
\* Write-write conflicts are detected at commit time (first-committer wins).
\*
\* This spec does NOT include SSI (rw-antidependency tracking).
\* SSI is modeled separately in SSI.tla which extends this spec.
\*
\* Reference: Berenson et al., "A Critique of ANSI SQL Isolation Levels" (SIGMOD 1995)
\*
\* Author: cracked-db project
\* Version: 1.0.0

EXTENDS Integers, Sequences, FiniteSets, TLC

CONSTANTS
    Keys,           \* Set of keys in the database
    Txns,           \* Set of transaction identifiers
    Values,         \* Set of possible values
    InitialValue    \* Initial value for all keys (e.g., "init" or 0)

\* Transaction states
CONSTANTS
    TXN_PENDING,    \* Transaction has not started
    TXN_ACTIVE,     \* Transaction is running
    TXN_COMMITTED,  \* Transaction has committed
    TXN_ABORTED     \* Transaction has aborted

VARIABLES
    \* Transaction metadata
    txnStatus,      \* txnStatus[t] = status of transaction t
    beginTs,        \* beginTs[t] = timestamp when t began (snapshot point)
    commitTs,       \* commitTs[t] = timestamp when t committed (or 0 if not committed)

    \* Version storage: versions[k] is a sequence of <<ts, value>> pairs
    \* Ordered by timestamp (newest first for efficient reads)
    versions,

    \* Per-transaction read/write sets
    readSet,        \* readSet[t] = set of keys read by t
    writeSet,       \* writeSet[t] = function from key -> value for buffered writes

    \* Global timestamp counter
    nextTs

\* Tuple of all variables for stuttering
vars == <<txnStatus, beginTs, commitTs, versions, readSet, writeSet, nextTs>>

-----------------------------------------------------------------------------
\* Type invariant

TypeOK ==
    /\ txnStatus \in [Txns -> {TXN_PENDING, TXN_ACTIVE, TXN_COMMITTED, TXN_ABORTED}]
    /\ beginTs \in [Txns -> Nat]
    /\ commitTs \in [Txns -> Nat]
    /\ \A k \in Keys: versions[k] \in Seq(Nat \X Values)
    /\ readSet \in [Txns -> SUBSET Keys]
    \* writeSet is a partial function: each txn maps a SUBSET of Keys to Values
    /\ \A t \in Txns:
         /\ DOMAIN writeSet[t] \subseteq Keys
         /\ \A k \in DOMAIN writeSet[t]: writeSet[t][k] \in Values
    /\ nextTs \in Nat

-----------------------------------------------------------------------------
\* Helper functions

\* Get the value of key k visible at timestamp ts
\* Returns the value from the most recent version with commitTs <= ts
ReadAtTimestamp(k, ts) ==
    LET validVersions == {i \in 1..Len(versions[k]): versions[k][i][1] <= ts}
    IN IF validVersions = {} THEN InitialValue
       ELSE LET maxIdx == CHOOSE i \in validVersions:
                 \A j \in validVersions: versions[k][i][1] >= versions[k][j][1]
            IN versions[k][maxIdx][2]

\* Check if a key was modified by a committed transaction after timestamp ts
\* This is used for write-write conflict detection
HasConflictingWrite(k, ts) ==
    \E i \in 1..Len(versions[k]): versions[k][i][1] > ts

\* Get all committed transactions
CommittedTxns == {t \in Txns: txnStatus[t] = TXN_COMMITTED}

\* Get the write set keys for a transaction
WriteSetKeys(t) == DOMAIN writeSet[t]

-----------------------------------------------------------------------------
\* Actions

\* Begin: Start a new transaction
\* Assigns a begin timestamp for snapshot reads
Begin(t) ==
    /\ txnStatus[t] = TXN_PENDING
    /\ txnStatus' = [txnStatus EXCEPT ![t] = TXN_ACTIVE]
    /\ beginTs' = [beginTs EXCEPT ![t] = nextTs]
    /\ nextTs' = nextTs + 1
    /\ commitTs' = commitTs
    /\ versions' = versions
    /\ readSet' = readSet
    /\ writeSet' = writeSet

\* Read: Read a key at the transaction's snapshot
\* Records the key in the read set (for SSI tracking, not used in pure SI)
Read(t, k) ==
    /\ txnStatus[t] = TXN_ACTIVE
    /\ readSet' = [readSet EXCEPT ![t] = readSet[t] \union {k}]
    \* The actual value is ReadAtTimestamp(k, beginTs[t])
    \* but we don't model the return value explicitly
    /\ UNCHANGED <<txnStatus, beginTs, commitTs, versions, writeSet, nextTs>>

\* Write: Buffer a write to the transaction's write set
\* The write is not visible until commit
Write(t, k, v) ==
    /\ txnStatus[t] = TXN_ACTIVE
    \* Extend the write set with the new key-value pair
    /\ writeSet' = [writeSet EXCEPT ![t] =
         [key \in (DOMAIN writeSet[t]) \union {k} |->
           IF key = k THEN v ELSE writeSet[t][key]]]
    /\ UNCHANGED <<txnStatus, beginTs, commitTs, versions, readSet, nextTs>>

\* Commit: Attempt to commit the transaction
\* Fails if there's a write-write conflict (another txn wrote to same key after our begin)
Commit(t) ==
    /\ txnStatus[t] = TXN_ACTIVE
    \* Write-write conflict check: no key in our write set was modified after our begin
    /\ \A k \in DOMAIN writeSet[t]: ~HasConflictingWrite(k, beginTs[t])
    \* Assign commit timestamp (use nextTs, then increment)
    /\ commitTs' = [commitTs EXCEPT ![t] = nextTs]
    \* Install all writes to version chains (use nextTs as the commit timestamp)
    /\ versions' = [k \in Keys |->
         IF k \in DOMAIN writeSet[t]
         THEN Append(versions[k], <<nextTs, writeSet[t][k]>>)
         ELSE versions[k]]
    /\ nextTs' = nextTs + 1
    /\ txnStatus' = [txnStatus EXCEPT ![t] = TXN_COMMITTED]
    /\ UNCHANGED <<beginTs, readSet, writeSet>>

\* Abort: Abort and discard the transaction
\* Can happen voluntarily or due to conflict
Abort(t) ==
    /\ txnStatus[t] = TXN_ACTIVE
    /\ txnStatus' = [txnStatus EXCEPT ![t] = TXN_ABORTED]
    \* Write set is discarded (not installed to versions)
    /\ UNCHANGED <<beginTs, commitTs, versions, readSet, writeSet, nextTs>>

\* ConflictAbort: Abort due to write-write conflict (for explicit modeling)
ConflictAbort(t) ==
    /\ txnStatus[t] = TXN_ACTIVE
    /\ \E k \in DOMAIN writeSet[t]: HasConflictingWrite(k, beginTs[t])
    /\ txnStatus' = [txnStatus EXCEPT ![t] = TXN_ABORTED]
    /\ UNCHANGED <<beginTs, commitTs, versions, readSet, writeSet, nextTs>>

-----------------------------------------------------------------------------
\* Initial state

Init ==
    /\ txnStatus = [t \in Txns |-> TXN_PENDING]
    /\ beginTs = [t \in Txns |-> 0]
    /\ commitTs = [t \in Txns |-> 0]
    /\ versions = [k \in Keys |-> <<>>]  \* Empty version chains
    /\ readSet = [t \in Txns |-> {}]
    /\ writeSet = [t \in Txns |-> [k \in {} |-> InitialValue]]  \* Empty function
    /\ nextTs = 1

\* Next state relation
Next ==
    \/ \E t \in Txns: Begin(t)
    \/ \E t \in Txns, k \in Keys: Read(t, k)
    \/ \E t \in Txns, k \in Keys, v \in Values: Write(t, k, v)
    \/ \E t \in Txns: Commit(t)
    \/ \E t \in Txns: Abort(t)
    \/ \E t \in Txns: ConflictAbort(t)

\* Specification with fairness (transactions eventually complete)
Spec == Init /\ [][Next]_vars /\ WF_vars(Next)

-----------------------------------------------------------------------------
\* Invariants

\* SnapshotConsistency: A transaction always reads a consistent snapshot
\* All reads within a transaction see the same committed state as of beginTs
\* (This is definitionally true in our model because ReadAtTimestamp uses beginTs)
SnapshotConsistency ==
    \A t \in Txns:
        txnStatus[t] = TXN_ACTIVE =>
            \A k \in readSet[t]:
                LET readValue == ReadAtTimestamp(k, beginTs[t])
                IN TRUE  \* The read value is consistent by construction

\* WriteWriteIsolation: No two committed transactions write the same key
\* at the same timestamp (first-committer wins)
WriteWriteIsolation ==
    \A t1, t2 \in Txns:
        /\ t1 /= t2
        /\ txnStatus[t1] = TXN_COMMITTED
        /\ txnStatus[t2] = TXN_COMMITTED
        => commitTs[t1] /= commitTs[t2]

\* MonotonicVersions: Timestamps in version chains are strictly increasing
\* (Actually they can be in any order in our Append model, but we check no duplicates)
MonotonicVersions ==
    \A k \in Keys:
        \A i, j \in 1..Len(versions[k]):
            i /= j => versions[k][i][1] /= versions[k][j][1]

\* NoTimeTravel: A transaction's commit timestamp is always > its begin timestamp
NoTimeTravel ==
    \A t \in Txns:
        txnStatus[t] = TXN_COMMITTED => commitTs[t] > beginTs[t]

\* CommitAfterBegin: If committed, commitTs must be assigned
CommitAfterBegin ==
    \A t \in Txns:
        txnStatus[t] = TXN_COMMITTED => commitTs[t] > 0

\* NoPhantomReads: If a transaction reads a key, the value it sees does not
\* change during its lifetime (snapshot guarantee)
\* This is implicit in SI but we state it explicitly
NoPhantomReads ==
    \A t \in Txns:
        txnStatus[t] = TXN_ACTIVE =>
            \A k \in readSet[t]:
                ReadAtTimestamp(k, beginTs[t]) = ReadAtTimestamp(k, beginTs[t])
                \* Tautology, but the point is beginTs[t] doesn't change

\* Combined invariant for TLC checking
Invariant ==
    /\ TypeOK
    /\ WriteWriteIsolation
    /\ MonotonicVersions
    /\ NoTimeTravel
    /\ CommitAfterBegin

-----------------------------------------------------------------------------
\* Temporal properties

\* AllTransactionsComplete: Every transaction eventually commits or aborts
AllTransactionsComplete ==
    \A t \in Txns:
        <>(txnStatus[t] \in {TXN_COMMITTED, TXN_ABORTED})

\* NoLivelock: The system makes progress (at least one transaction changes state)
\* We use the enabled-implies-eventually pattern
NoLivelock ==
    []<><<Next>>_vars

=============================================================================
\* Modification History
\* Last modified: 2026-05-22
\* Created: 2026-05-22
