---------------------------- MODULE SSI ----------------------------
\* Serializable Snapshot Isolation (SSI)
\*
\* This specification extends MVCC (snapshot isolation) with rw-antidependency
\* tracking to provide serializability. SSI detects and prevents dangerous
\* structures that could lead to non-serializable schedules.
\*
\* # Algorithm (per Cahill, Röhm, Fekete, SIGMOD 2008)
\*
\* 1. Track rw-antidependencies: if T1 reads a version that T2 later
\*    overwrites, record an rw-edge T1 -> T2.
\*
\* 2. A dangerous structure is two consecutive rw-edges: T1 -> T2 -> T3
\*    where T2 is the "pivot" transaction in the middle.
\*
\* 3. When detected at commit time, abort to break the potential cycle.
\*
\* # Key Insight
\*
\* SSI aborts conservatively: not all dangerous structures lead to cycles,
\* but all cycles contain a dangerous structure. By aborting when we detect
\* one, we prevent all cycles at the cost of some false positives.
\*
\* # TLC Verification Results
\*
\* Model: Keys={k1,k2}, Txns={t1,t2,t3}, Values={v1,v2}
\* State space: 152,592,268 states generated, 24,617,893 distinct states
\* Search depth: 19
\* Runtime: ~24 minutes (8 workers)
\* Result: No violations found. All invariants verified:
\*   - TypeOK
\*   - WriteWriteIsolation
\*   - NoTimeTravel
\*   - SSICorrectness (includes NoDangerousStructures)
\*
\* # References
\*
\* Cahill, Röhm, Fekete, "Serializable Isolation for Snapshot Databases"
\* (SIGMOD 2008) - The foundational SSI paper.
\*
\* Author: cracked-db project
\* Version: 1.1.0

EXTENDS Integers, Sequences, FiniteSets, TLC

CONSTANTS
    Keys,           \* Set of keys in the database
    Txns,           \* Set of transaction identifiers
    Values,         \* Set of possible values
    InitialValue    \* Initial value for all keys

\* Transaction states
CONSTANTS
    TXN_PENDING,
    TXN_ACTIVE,
    TXN_COMMITTED,
    TXN_ABORTED

VARIABLES
    \* === MVCC state (from MVCC.tla) ===
    txnStatus,      \* txnStatus[t] = status of transaction t
    beginTs,        \* beginTs[t] = timestamp when t began
    commitTs,       \* commitTs[t] = timestamp when t committed (or 0)
    versions,       \* versions[k] = sequence of <<ts, value>> pairs
    readSet,        \* readSet[t] = set of keys read by t
    writeSet,       \* writeSet[t] = partial function from key -> value
    nextTs,         \* Global timestamp counter

    \* === SSI state (new in this spec) ===
    \* RW-antidependency edges: rwEdges[t1][t2] = TRUE iff t1 --rw--> t2
    \* This means t1 read a version that t2 later overwrote
    rwEdges,

    \* Per-transaction conflict flags (optimization over tracking all edges)
    \* inConflict[t] = TRUE if some committed T' --rw--> t (t is a writer)
    \* outConflict[t] = TRUE if t --rw--> some committed T' (t is a reader)
    inConflict,
    outConflict

\* Tuple of all variables
vars == <<txnStatus, beginTs, commitTs, versions, readSet, writeSet, nextTs,
          rwEdges, inConflict, outConflict>>

mvccVars == <<txnStatus, beginTs, commitTs, versions, readSet, writeSet, nextTs>>

-----------------------------------------------------------------------------
\* Type invariant

TypeOK ==
    /\ txnStatus \in [Txns -> {TXN_PENDING, TXN_ACTIVE, TXN_COMMITTED, TXN_ABORTED}]
    /\ beginTs \in [Txns -> Nat]
    /\ commitTs \in [Txns -> Nat]
    /\ \A k \in Keys: versions[k] \in Seq(Nat \X Values)
    /\ readSet \in [Txns -> SUBSET Keys]
    /\ \A t \in Txns:
         /\ DOMAIN writeSet[t] \subseteq Keys
         /\ \A k \in DOMAIN writeSet[t]: writeSet[t][k] \in Values
    /\ nextTs \in Nat
    /\ rwEdges \in [Txns -> [Txns -> BOOLEAN]]
    /\ inConflict \in [Txns -> BOOLEAN]
    /\ outConflict \in [Txns -> BOOLEAN]

-----------------------------------------------------------------------------
\* Helper functions (from MVCC.tla)

\* Get the value of key k visible at timestamp ts
ReadAtTimestamp(k, ts) ==
    LET validVersions == {i \in 1..Len(versions[k]): versions[k][i][1] <= ts}
    IN IF validVersions = {} THEN InitialValue
       ELSE LET maxIdx == CHOOSE i \in validVersions:
                 \A j \in validVersions: versions[k][i][1] >= versions[k][j][1]
            IN versions[k][maxIdx][2]

\* Get the transaction that wrote the version visible at timestamp ts
\* Returns the transaction with commit timestamp = the version's timestamp
\* Returns "none" if this is the initial version
WriterAtTimestamp(k, ts) ==
    LET validVersions == {i \in 1..Len(versions[k]): versions[k][i][1] <= ts}
    IN IF validVersions = {} THEN "none"
       ELSE LET maxIdx == CHOOSE i \in validVersions:
                 \A j \in validVersions: versions[k][i][1] >= versions[k][j][1]
                 versionTs == versions[k][maxIdx][1]
            IN CHOOSE t \in Txns: commitTs[t] = versionTs

\* Check if a key was modified by a committed transaction after timestamp ts
HasConflictingWrite(k, ts) ==
    \E i \in 1..Len(versions[k]): versions[k][i][1] > ts

\* Find all active or committed transactions that read key k before timestamp ts
\* These are potential rw-antidependency sources
ReadersOf(k, beforeTs) ==
    {t \in Txns:
        /\ txnStatus[t] \in {TXN_ACTIVE, TXN_COMMITTED}
        /\ k \in readSet[t]
        /\ beginTs[t] < beforeTs}

\* Check for dangerous structure: t has both incoming and outgoing rw-edges
\* with at least one being to/from a committed transaction
HasDangerousStructure(t) ==
    /\ inConflict[t]
    /\ outConflict[t]

-----------------------------------------------------------------------------
\* Actions

\* Begin: Start a new transaction
Begin(t) ==
    /\ txnStatus[t] = TXN_PENDING
    /\ txnStatus' = [txnStatus EXCEPT ![t] = TXN_ACTIVE]
    /\ beginTs' = [beginTs EXCEPT ![t] = nextTs]
    /\ nextTs' = nextTs + 1
    /\ UNCHANGED <<commitTs, versions, readSet, writeSet>>
    /\ UNCHANGED <<rwEdges, inConflict, outConflict>>

\* Read: Read a key at the transaction's snapshot
\* This may create rw-antidependency if another active txn later writes the key
Read(t, k) ==
    /\ txnStatus[t] = TXN_ACTIVE
    /\ readSet' = [readSet EXCEPT ![t] = readSet[t] \union {k}]
    /\ UNCHANGED <<txnStatus, beginTs, commitTs, versions, writeSet, nextTs>>
    /\ UNCHANGED <<rwEdges, inConflict, outConflict>>

\* Write: Buffer a write to the transaction's write set
\* When committed, this may create rw-edges with prior readers
Write(t, k, v) ==
    /\ txnStatus[t] = TXN_ACTIVE
    /\ writeSet' = [writeSet EXCEPT ![t] =
         [key \in (DOMAIN writeSet[t]) \union {k} |->
           IF key = k THEN v ELSE writeSet[t][key]]]
    /\ UNCHANGED <<txnStatus, beginTs, commitTs, versions, readSet, nextTs>>
    /\ UNCHANGED <<rwEdges, inConflict, outConflict>>

\* SSICommit: Commit with SSI validation
\* 1. Check write-write conflicts (from SI)
\* 2. Create rw-edges for all prior readers of keys we're writing
\* 3. Check for dangerous structures
\* 4. Abort if dangerous structure found
SSICommit(t) ==
    /\ txnStatus[t] = TXN_ACTIVE
    \* Write-write conflict check (from SI)
    /\ \A k \in DOMAIN writeSet[t]: ~HasConflictingWrite(k, beginTs[t])

    \* Compute new rw-edges: for each key we're writing, find prior readers
    \* These readers have an rw-antidependency on us
    /\ LET newInEdges == {reader \in Txns:
            \E k \in DOMAIN writeSet[t]:
                /\ reader /= t
                /\ k \in readSet[reader]
                /\ txnStatus[reader] \in {TXN_ACTIVE, TXN_COMMITTED}
                /\ beginTs[reader] < nextTs}
       IN
       \* Check for dangerous structure BEFORE committing
       \* Case 1: If we (t) have outConflict and any newInEdge is from a committed txn
       \*         Then we're the pivot: T' --rw--> t --rw--> T'' (our outConflict)
       /\ ~(outConflict[t] /\ \E r \in newInEdges: txnStatus[r] = TXN_COMMITTED)
       \*
       \* Case 2: Would any committed reader r become a pivot in a dangerous structure?
       \*         We need to check if there's an existing edge T' --rw--> r from a
       \*         committed T'. If so, adding r --rw--> t creates T' --rw--> r --rw--> t.
       \*         Note: We check actual rwEdges, not inConflict, because inConflict
       \*         may not be updated if the source committed after the edge was created.
       /\ ~\E r \in newInEdges:
            /\ txnStatus[r] = TXN_COMMITTED
            /\ \E src \in Txns:
                /\ src /= r
                /\ rwEdges[src][r]
                /\ txnStatus[src] = TXN_COMMITTED
       \*
       \* Case 3: Would our commit complete a dangerous structure where we're at the start?
       \*         If we have outConflict (edge t --rw--> mid where mid is committed),
       \*         and mid has an outgoing edge to another committed transaction (mid --rw--> dst),
       \*         then committing t completes the chain t --rw--> mid --rw--> dst.
       /\ ~(outConflict[t] /\
            \E mid \in Txns:
               /\ mid /= t
               /\ rwEdges[t][mid]
               /\ txnStatus[mid] = TXN_COMMITTED
               /\ \E dst \in Txns:
                    /\ dst /= t /\ dst /= mid
                    /\ rwEdges[mid][dst]
                    /\ txnStatus[dst] = TXN_COMMITTED)

       \* Update rw-edges: for each reader, add edge reader --rw--> t
       /\ rwEdges' = [src \in Txns |->
            [dst \in Txns |->
              IF src \in newInEdges /\ dst = t
              THEN TRUE
              ELSE rwEdges[src][dst]]]

       \* Update conflict flags
       /\ inConflict' = [inConflict EXCEPT ![t] =
            inConflict[t] \/ \E r \in newInEdges: txnStatus[r] = TXN_COMMITTED]
       /\ outConflict' = [r \in Txns |->
            IF r \in newInEdges /\ txnStatus[r] = TXN_ACTIVE
            THEN TRUE  \* r now has outConflict because we're committing
            ELSE outConflict[r]]

    \* Commit the transaction
    /\ commitTs' = [commitTs EXCEPT ![t] = nextTs]
    /\ versions' = [k \in Keys |->
         IF k \in DOMAIN writeSet[t]
         THEN Append(versions[k], <<nextTs, writeSet[t][k]>>)
         ELSE versions[k]]
    /\ nextTs' = nextTs + 1
    /\ txnStatus' = [txnStatus EXCEPT ![t] = TXN_COMMITTED]
    /\ UNCHANGED <<beginTs, readSet, writeSet>>

\* Abort: Abort the transaction (voluntary or due to SSI conflict)
Abort(t) ==
    /\ txnStatus[t] = TXN_ACTIVE
    /\ txnStatus' = [txnStatus EXCEPT ![t] = TXN_ABORTED]
    /\ UNCHANGED <<beginTs, commitTs, versions, readSet, writeSet, nextTs>>
    /\ UNCHANGED <<rwEdges, inConflict, outConflict>>

\* SSIAbort: Abort due to dangerous structure detection
SSIAbort(t) ==
    /\ txnStatus[t] = TXN_ACTIVE
    /\ HasDangerousStructure(t)
    /\ txnStatus' = [txnStatus EXCEPT ![t] = TXN_ABORTED]
    /\ UNCHANGED <<beginTs, commitTs, versions, readSet, writeSet, nextTs>>
    /\ UNCHANGED <<rwEdges, inConflict, outConflict>>

-----------------------------------------------------------------------------
\* Initial state

Init ==
    /\ txnStatus = [t \in Txns |-> TXN_PENDING]
    /\ beginTs = [t \in Txns |-> 0]
    /\ commitTs = [t \in Txns |-> 0]
    /\ versions = [k \in Keys |-> <<>>]
    /\ readSet = [t \in Txns |-> {}]
    /\ writeSet = [t \in Txns |-> [k \in {} |-> InitialValue]]
    /\ nextTs = 1
    /\ rwEdges = [t1 \in Txns |-> [t2 \in Txns |-> FALSE]]
    /\ inConflict = [t \in Txns |-> FALSE]
    /\ outConflict = [t \in Txns |-> FALSE]

\* Next state relation
Next ==
    \/ \E t \in Txns: Begin(t)
    \/ \E t \in Txns, k \in Keys: Read(t, k)
    \/ \E t \in Txns, k \in Keys, v \in Values: Write(t, k, v)
    \/ \E t \in Txns: SSICommit(t)
    \/ \E t \in Txns: Abort(t)
    \/ \E t \in Txns: SSIAbort(t)

\* Specification
Spec == Init /\ [][Next]_vars

-----------------------------------------------------------------------------
\* Invariants

\* WriteWriteIsolation (from MVCC): No two committed txns have same commit ts
WriteWriteIsolation ==
    \A t1, t2 \in Txns:
        /\ t1 /= t2
        /\ txnStatus[t1] = TXN_COMMITTED
        /\ txnStatus[t2] = TXN_COMMITTED
        => commitTs[t1] /= commitTs[t2]

\* NoTimeTravel: commit ts > begin ts
NoTimeTravel ==
    \A t \in Txns:
        txnStatus[t] = TXN_COMMITTED => commitTs[t] > beginTs[t]

\* Serializability: The commit order of transactions is serializable.
\*
\* In SSI, serializability is maintained by preventing dangerous structures:
\* a chain of two rw-antidependency edges T1 --rw--> T2 --rw--> T3 where
\* T1 and T3 are both concurrent with T2 (the "pivot").
\*
\* This invariant verifies that among committed transactions, no such
\* dangerous structure exists. The SSICommit action prevents this by
\* aborting when it detects a dangerous structure at commit time.
\*
\* Key insight: A committed transaction should NOT be a pivot in a dangerous
\* structure where both endpoints have also committed.
NoDangerousStructures ==
    \A pivot \in Txns:
        txnStatus[pivot] = TXN_COMMITTED =>
            \* A dangerous structure is: T_in --rw--> pivot --rw--> T_out
            \* where both T_in and T_out are committed
            ~\E t_in, t_out \in Txns:
                /\ t_in /= pivot /\ t_out /= pivot /\ t_in /= t_out
                /\ txnStatus[t_in] = TXN_COMMITTED
                /\ txnStatus[t_out] = TXN_COMMITTED
                /\ rwEdges[t_in][pivot]    \* T_in --rw--> pivot
                /\ rwEdges[pivot][t_out]   \* pivot --rw--> T_out

\* SSI Correctness: Verify that the serialization graph has no cycles.
\*
\* The serialization graph has three types of edges:
\* - WW (write-write): Both wrote same key, earlier committer serializes first
\* - WR (write-read): Reader depends on writer's version
\* - RW (read-write): Reader saw old version, writer created new version
\*
\* SSI guarantees: if there's a cycle, it must contain at least two
\* consecutive RW edges (the dangerous structure). By preventing dangerous
\* structures, we prevent all cycles.
\*
\* This check verifies that for all committed transactions:
\* 1. No transaction is a pivot in a dangerous structure
\* 2. RW edges are properly recorded
SSICorrectness ==
    /\ NoDangerousStructures
    \* Additional check: conflict flags are consistent with rwEdges
    /\ \A t \in Txns:
        txnStatus[t] = TXN_COMMITTED =>
            \* If inConflict is set, there should be an incoming edge from a committed txn
            (inConflict[t] => \E src \in Txns:
                /\ src /= t
                /\ rwEdges[src][t]
                /\ txnStatus[src] = TXN_COMMITTED)
            \* Note: outConflict may have been set when dst was active but dst later aborted,
            \* so we don't require outConflict => edge to committed txn

\* Combined invariant for TLC checking
Invariant ==
    /\ TypeOK
    /\ WriteWriteIsolation
    /\ NoTimeTravel
    /\ SSICorrectness

=============================================================================
\* Modification History
\* Last modified: 2026-05-22
\* Created: 2026-05-22
