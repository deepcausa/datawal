--------------------------- MODULE Compaction ---------------------------
(******************************************************************************)
(* DataWal `compact_to` semantics.                                            *)
(*                                                                            *)
(* The source log is an append-only sequence of records:                      *)
(*   <<"put", k, v>>  or  <<"del", k>>                                        *)
(*                                                                            *)
(* `compact_to` produces a *new* compacted log (an out_dir in the real impl)  *)
(* containing exactly one put per live key, in some deterministic order, and  *)
(* no tombstones.                                                             *)
(*                                                                            *)
(* The keydir projection (last-write-wins) is reused from the same recipe as  *)
(* KeydirProjection.tla, but defined as an operator over any log argument so  *)
(* we can compare projection(log) with projection(compactedLog).              *)
(*                                                                            *)
(* Invariants checked:                                                        *)
(*   - TypeInvariant                                                          *)
(*   - CompactionPreservesLiveState : projection(log) = projection(compacted) *)
(*   - NoDeletedKeyResurrection     : no key whose final source record is a   *)
(*                                    del appears in the compacted log       *)
(*   - ExportCleanCorrectness       : the compacted log contains no del       *)
(*                                    records and at most one put per key     *)
(******************************************************************************)

EXTENDS Naturals, Sequences, FiniteSets

CONSTANTS Keys, Values, MaxOps

ASSUME /\ IsFiniteSet(Keys)
       /\ IsFiniteSet(Values)
       /\ MaxOps \in Nat

VARIABLES log, compactedLog, compacted

vars == <<log, compactedLog, compacted>>

----------------------------------------------------------------------------
(* Records *)
----------------------------------------------------------------------------

PutRec(k, v) == <<"put", k, v>>
DelRec(k)    == <<"del", k>>

Record == { PutRec(k, v) : k \in Keys, v \in Values }
       \cup { DelRec(k) : k \in Keys }

KeyOf(rec) == rec[2]

NULL == "<<none>>"

----------------------------------------------------------------------------
(* LWW projection over an arbitrary log argument *)
----------------------------------------------------------------------------

LastIndexOfKeyIn(L, k) ==
  LET idxs == { i \in 1..Len(L) : KeyOf(L[i]) = k } IN
    IF idxs = {} THEN 0
    ELSE CHOOSE i \in idxs : \A j \in idxs : i >= j

IsLiveIn(L, k) ==
  LET i == LastIndexOfKeyIn(L, k) IN
    /\ i > 0
    /\ L[i][1] = "put"

LiveValueIn(L, k) ==
  LET i == LastIndexOfKeyIn(L, k) IN
    L[i][3]

KeydirOf(L) ==
  [ k \in Keys |-> IF IsLiveIn(L, k) THEN LiveValueIn(L, k) ELSE NULL ]

----------------------------------------------------------------------------
(* Init *)
----------------------------------------------------------------------------

TypeInvariant ==
  /\ log \in Seq(Record)
  /\ Len(log) <= MaxOps
  /\ compactedLog \in Seq(Record)
  /\ compacted \in BOOLEAN

Init ==
  /\ log = << >>
  /\ compactedLog = << >>
  /\ compacted = FALSE

----------------------------------------------------------------------------
(* Actions *)
----------------------------------------------------------------------------

DoPut(k, v) ==
  /\ ~ compacted
  /\ Len(log) < MaxOps
  /\ k \in Keys
  /\ v \in Values
  /\ log' = Append(log, PutRec(k, v))
  /\ UNCHANGED <<compactedLog, compacted>>

DoDel(k) ==
  /\ ~ compacted
  /\ Len(log) < MaxOps
  /\ k \in Keys
  /\ log' = Append(log, DelRec(k))
  /\ UNCHANGED <<compactedLog, compacted>>

\* `compact_to` collapses the source log into one put per live key.
\* Modelled deterministically: for each live key, append a single
\* <<"put", k, LiveValueIn(log, k)>> record. Order is fixed by CHOOSE so
\* the action is a function of `log`.
LiveKeys == { k \in Keys : IsLiveIn(log, k) }

\* Build a sequence containing exactly one put per live key.
\* We do not commit to a particular ordering across runs; the spec only
\* requires the resulting projection to match the source.
\* Implemented by an inductive construction over a CHOOSE-ordering.
RECURSIVE BuildCompacted(_)
BuildCompacted(S) ==
  IF S = {} THEN << >>
  ELSE
    LET k == CHOOSE x \in S : TRUE IN
      <<PutRec(k, LiveValueIn(log, k))>> \o BuildCompacted(S \ {k})

DoCompact ==
  /\ ~ compacted
  /\ compactedLog' = BuildCompacted(LiveKeys)
  /\ compacted' = TRUE
  /\ UNCHANGED log

Next ==
  \/ \E k \in Keys, v \in Values : DoPut(k, v)
  \/ \E k \in Keys : DoDel(k)
  \/ DoCompact

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------
(* Invariants *)
----------------------------------------------------------------------------

\* The compacted log projects to the same keydir as the source log.
CompactionPreservesLiveState ==
  compacted => KeydirOf(log) = KeydirOf(compactedLog)

\* A key whose final source record is a tombstone does not appear in
\* the compacted log (no resurrection).
NoDeletedKeyResurrection ==
  compacted =>
    \A k \in Keys :
      ( ~ IsLiveIn(log, k) ) =>
      \A i \in 1..Len(compactedLog) : KeyOf(compactedLog[i]) # k

\* The compacted log has no tombstones, and contains at most one
\* record per key.
ExportCleanCorrectness ==
  compacted =>
    /\ \A i \in 1..Len(compactedLog) : compactedLog[i][1] = "put"
    /\ \A i, j \in 1..Len(compactedLog) :
         ( i # j ) => KeyOf(compactedLog[i]) # KeyOf(compactedLog[j])

=============================================================================
