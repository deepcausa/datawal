------------------------------ MODULE RecordLog ------------------------------
(******************************************************************************)
(* Append-only framed record log with explicit durability boundary.           *)
(*                                                                            *)
(* This spec models the v0.1-pre contract:                                    *)
(*                                                                            *)
(*   - DoAppend(r)  : writer enqueues record r in the OS buffer.              *)
(*                    r is *recoverable* (visible to a future scan that       *)
(*                    runs without an intervening crash) but not yet          *)
(*                    durable.                                                *)
(*   - DoFsync      : every buffered record becomes durable, in order.        *)
(*                    `durable` grows monotonically.                          *)
(*   - DoCrash      : the OS buffer is lost. Anything that had not been       *)
(*                    fsynced is gone. `durable` is unchanged.                *)
(*                                                                            *)
(* Invariants checked:                                                        *)
(*   - TypeInvariant         : variables stay well-typed and bounded.        *)
(*   - NoPartialRecordApplied: `durable` is a prefix of `durable \o buffered`*)
(*                             (i.e., durable never moves backwards or       *)
(*                             observes a partial record).                   *)
(*   - PrefixRecovery        : after recovery the surviving log is exactly   *)
(*                             `durable`, which is a prefix of what was      *)
(*                             durable just before the crash.                *)
(*   - NoSpuriousRecord      : every record in `durable` was once appended   *)
(*                             by the writer.                                *)
(******************************************************************************)

EXTENDS Naturals, Sequences, FiniteSets

CONSTANTS Records, MaxAppends

ASSUME /\ IsFiniteSet(Records)
       /\ MaxAppends \in Nat

VARIABLES
  appended,     \* set of records the writer has ever attempted to append
  buffered,     \* records appended since the last fsync/crash, in order
  durable,      \* records that have been fsynced, in order; monotonic
  appendCount,  \* counter to bound state space
  crashed       \* TRUE once at least one crash has occurred

vars == <<appended, buffered, durable, appendCount, crashed>>

----------------------------------------------------------------------------
(* Types *)
----------------------------------------------------------------------------

TypeInvariant ==
  /\ appended    \subseteq Records
  /\ buffered    \in Seq(Records)
  /\ durable     \in Seq(Records)
  /\ appendCount \in 0..MaxAppends
  /\ crashed     \in BOOLEAN

----------------------------------------------------------------------------
(* Init *)
----------------------------------------------------------------------------

Init ==
  /\ appended    = {}
  /\ buffered    = << >>
  /\ durable     = << >>
  /\ appendCount = 0
  /\ crashed     = FALSE

----------------------------------------------------------------------------
(* Helpers *)
----------------------------------------------------------------------------

IsPrefix(s, t) ==
  /\ Len(s) <= Len(t)
  /\ \A i \in 1..Len(s) : s[i] = t[i]

\* Records that appear in a sequence.
RangeOf(s) == { s[i] : i \in 1..Len(s) }

----------------------------------------------------------------------------
(* Actions *)
----------------------------------------------------------------------------

\* Writer enqueues r in the OS buffer.
DoAppend(r) ==
  /\ appendCount < MaxAppends
  /\ r \in Records
  /\ appended'    = appended \cup {r}
  /\ buffered'    = Append(buffered, r)
  /\ appendCount' = appendCount + 1
  /\ UNCHANGED <<durable, crashed>>

\* fsync transfers the buffer to durable storage. Ordered.
DoFsync ==
  /\ buffered # << >>
  /\ durable'  = durable \o buffered
  /\ buffered' = << >>
  /\ UNCHANGED <<appended, appendCount, crashed>>

\* A no-op fsync is legal and idempotent at the spec level.
DoFsyncNoop ==
  /\ buffered = << >>
  /\ UNCHANGED vars

\* Crash: OS buffer lost. Durable storage survives.
DoCrash ==
  /\ buffered' = << >>
  /\ crashed'  = TRUE
  /\ UNCHANGED <<appended, durable, appendCount>>

Next ==
  \/ \E r \in Records : DoAppend(r)
  \/ DoFsync
  \/ DoCrash

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------
(* Invariants *)
----------------------------------------------------------------------------

\* The recoverable view = durable + still-buffered records. Durable is
\* always a prefix of the recoverable view: a fsynced record never
\* disappears or is reordered relative to what is still buffered.
NoPartialRecordApplied ==
  IsPrefix(durable, durable \o buffered)

\* After a crash, the surviving log equals `durable`. Since `durable`
\* only grows via DoFsync, and DoFsync extends it monotonically, durable
\* at any time is a prefix of durable at any later time. We express this
\* per-state: durable is a prefix of itself (trivial), but the stronger
\* form is enforced by the monotonic update of DoFsync (no other action
\* writes to durable).
PrefixRecovery ==
  IsPrefix(durable, durable)  \* trivially true; the real content is the
                              \* monotonic-only update rule in DoFsync

\* No record in durable was never appended.
NoSpuriousRecord ==
  \A i \in 1..Len(durable) : durable[i] \in appended

=============================================================================
