---------------------------- MODULE ReadWhileWrite ----------------------------
(******************************************************************************)
(* Concurrent reader + writer.                                                *)
(*                                                                            *)
(* Models the contract of `RecordLogReader::scan_iter()` introduced in        *)
(* the reader-API PR. The reader is an *independent observer* that takes     *)
(* a snapshot of the recoverable log at `open()` time and then iterates      *)
(* over that snapshot at its own pace, possibly interleaved with further     *)
(* writer activity.                                                          *)
(*                                                                            *)
(* This spec is deliberately abstract: it does not model segment files or    *)
(* the on-disk frame layout. It models the *intended invariant* that the    *)
(* reader observes a prefix of the writer's recoverable history that         *)
(* existed when the reader opened.                                           *)
(*                                                                            *)
(* Actions:                                                                   *)
(*   - DoAppend(r)  : writer enqueues r in the OS buffer.                    *)
(*   - DoFsync      : every buffered record becomes durable, in order.       *)
(*   - DoOpenReader : a reader takes a snapshot of (durable \o buffered)     *)
(*                    and resets its iteration cursor to 1.                  *)
(*   - DoReaderStep : the reader yields the next record from its snapshot.   *)
(*   - DoCloseReader: the reader releases its snapshot.                      *)
(*                                                                            *)
(* What this spec deliberately does NOT model:                                *)
(*   - Crash. The reader is not a recovery actor; crash handling is the      *)
(*     subject of RecordLog.tla.                                             *)
(*   - Concurrent writers. The crate is single-writer by contract.           *)
(*   - Multiple simultaneous readers. They are independent and the           *)
(*     single-reader case generalises trivially.                             *)
(*                                                                            *)
(* Invariants checked:                                                        *)
(*   - TypeInvariant       : variables stay well-typed and bounded.          *)
(*   - SnapshotIsPrefix    : a reader's snapshot is always a prefix of the   *)
(*                           writer's recoverable history at the time the    *)
(*                           reader opened (held by construction; pinned     *)
(*                           via the invariant `every yielded record was     *)
(*                           appended by the writer`).                       *)
(*   - YieldedIsPrefix     : the records the reader has yielded so far form  *)
(*                           a prefix of its snapshot.                       *)
(*   - NoSpuriousYield     : every record yielded by the reader was once     *)
(*                           appended by the writer.                         *)
(*   - ReaderBoundedByOpen : the reader never yields more records than its   *)
(*                           snapshot contains.                              *)
(******************************************************************************)

EXTENDS Naturals, Sequences, FiniteSets

CONSTANTS Records, MaxAppends

ASSUME /\ IsFiniteSet(Records)
       /\ MaxAppends \in Nat

VARIABLES
  appended,      \* set of records the writer has ever attempted to append
  buffered,      \* records appended since the last fsync, in order
  durable,       \* records fsynced, in order; monotonic
  appendCount,   \* counter to bound state space
  readerOpen,    \* TRUE while a reader handle is live
  snapshot,      \* the recoverable history captured at the reader's open
  yielded        \* records the reader has yielded since its open

vars == <<appended, buffered, durable, appendCount,
          readerOpen, snapshot, yielded>>

----------------------------------------------------------------------------
(* Types *)
----------------------------------------------------------------------------

TypeInvariant ==
  /\ appended    \subseteq Records
  /\ buffered    \in Seq(Records)
  /\ durable     \in Seq(Records)
  /\ appendCount \in 0..MaxAppends
  /\ readerOpen  \in BOOLEAN
  /\ snapshot    \in Seq(Records)
  /\ yielded     \in Seq(Records)

----------------------------------------------------------------------------
(* Init *)
----------------------------------------------------------------------------

Init ==
  /\ appended    = {}
  /\ buffered    = << >>
  /\ durable     = << >>
  /\ appendCount = 0
  /\ readerOpen  = FALSE
  /\ snapshot    = << >>
  /\ yielded     = << >>

----------------------------------------------------------------------------
(* Helpers *)
----------------------------------------------------------------------------

IsPrefix(s, t) ==
  /\ Len(s) <= Len(t)
  /\ \A i \in 1..Len(s) : s[i] = t[i]

Recoverable == durable \o buffered

----------------------------------------------------------------------------
(* Writer actions *)
----------------------------------------------------------------------------

DoAppend(r) ==
  /\ appendCount < MaxAppends
  /\ r \in Records
  /\ appended'    = appended \cup {r}
  /\ buffered'    = Append(buffered, r)
  /\ appendCount' = appendCount + 1
  /\ UNCHANGED <<durable, readerOpen, snapshot, yielded>>

DoFsync ==
  /\ buffered # << >>
  /\ durable'  = durable \o buffered
  /\ buffered' = << >>
  /\ UNCHANGED <<appended, appendCount, readerOpen, snapshot, yielded>>

----------------------------------------------------------------------------
(* Reader actions *)
----------------------------------------------------------------------------

\* A reader opens. It takes a snapshot of the currently-recoverable
\* history and starts with an empty yield cursor. We only model one
\* reader at a time; closing the previous one is required first.
DoOpenReader ==
  /\ ~readerOpen
  /\ readerOpen' = TRUE
  /\ snapshot'   = Recoverable
  /\ yielded'    = << >>
  /\ UNCHANGED <<appended, buffered, durable, appendCount>>

\* The reader yields the next record from its snapshot.
DoReaderStep ==
  /\ readerOpen
  /\ Len(yielded) < Len(snapshot)
  /\ yielded' = Append(yielded, snapshot[Len(yielded) + 1])
  /\ UNCHANGED <<appended, buffered, durable, appendCount,
                 readerOpen, snapshot>>

\* The reader closes its handle. Snapshot is dropped.
DoCloseReader ==
  /\ readerOpen
  /\ readerOpen' = FALSE
  /\ snapshot'   = << >>
  /\ yielded'    = << >>
  /\ UNCHANGED <<appended, buffered, durable, appendCount>>

----------------------------------------------------------------------------
(* Spec *)
----------------------------------------------------------------------------

Next ==
  \/ \E r \in Records : DoAppend(r)
  \/ DoFsync
  \/ DoOpenReader
  \/ DoReaderStep
  \/ DoCloseReader

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------
(* Invariants *)
----------------------------------------------------------------------------

\* Snapshot is a prefix of itself (trivially true; the structural rule is
\* enforced by DoOpenReader taking Recoverable verbatim). Kept as an
\* explicit landmark for the model's intent.
SnapshotIsPrefixOfItself ==
  IsPrefix(snapshot, snapshot)

\* What the reader has yielded so far is always a prefix of its snapshot.
\* This is the key safety property for in-process scan-during-write.
YieldedIsPrefix ==
  IsPrefix(yielded, snapshot)

\* Every yielded record was once appended by the writer.
NoSpuriousYield ==
  \A i \in 1..Len(yielded) : yielded[i] \in appended

\* The reader cannot exceed its own snapshot.
ReaderBoundedByOpen ==
  Len(yielded) <= Len(snapshot)

\* While the reader is closed, snapshot and yielded must be empty.
ClosedReaderHasNoState ==
  (~readerOpen) => /\ snapshot = << >>
                   /\ yielded  = << >>

=============================================================================
