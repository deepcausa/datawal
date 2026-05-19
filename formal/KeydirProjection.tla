--------------------------- MODULE KeydirProjection ---------------------------
(******************************************************************************)
(* DataWal keydir projection from an append-only Put/Delete log.              *)
(*                                                                            *)
(* The log is a finite sequence of records, each tagged as either             *)
(*   <<"put", k, v>>   or   <<"del", k>>                                      *)
(*                                                                            *)
(* The keydir is the rebuilt projection (last-write-wins) over that log:      *)
(*                                                                            *)
(*   for k in keys:                                                           *)
(*     find the last record mentioning k                                      *)
(*     if it is <<"put", k, v>> => keydir[k] = v                              *)
(*     if it is <<"del", k>>    => k is absent                                *)
(*                                                                            *)
(* Invariants checked:                                                        *)
(*   - KeydirIsProjection            : keydir agrees with the LWW             *)
(*                                     projection function                    *)
(*   - LastWriteWins                 : a later put overrides an earlier put   *)
(*   - TombstoneDeletion             : a del after a put removes the key     *)
(*   - PutAfterDeleteResurrectsNewValue                                       *)
(*       a put after a del re-creates the key                                 *)
(******************************************************************************)

EXTENDS Naturals, Sequences, FiniteSets

CONSTANTS Keys, Values, MaxOps

ASSUME /\ IsFiniteSet(Keys)
       /\ IsFiniteSet(Values)
       /\ MaxOps \in Nat

VARIABLES log

vars == <<log>>

----------------------------------------------------------------------------
(* Records *)
----------------------------------------------------------------------------

PutRec(k, v) == <<"put", k, v>>
DelRec(k)    == <<"del", k>>

Record == { PutRec(k, v) : k \in Keys, v \in Values }
       \cup { DelRec(k) : k \in Keys }

KeyOf(rec) == rec[2]

----------------------------------------------------------------------------
(* Init *)
----------------------------------------------------------------------------

TypeInvariant ==
  /\ log \in Seq(Record)
  /\ Len(log) <= MaxOps

Init == log = << >>

----------------------------------------------------------------------------
(* Actions *)
----------------------------------------------------------------------------

DoPut(k, v) ==
  /\ Len(log) < MaxOps
  /\ k \in Keys
  /\ v \in Values
  /\ log' = Append(log, PutRec(k, v))

DoDel(k) ==
  /\ Len(log) < MaxOps
  /\ k \in Keys
  /\ log' = Append(log, DelRec(k))

Next ==
  \/ \E k \in Keys, v \in Values : DoPut(k, v)
  \/ \E k \in Keys : DoDel(k)

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------
(* LWW projection function *)
----------------------------------------------------------------------------

LastIndexOfKey(k) ==
  LET idxs == { i \in 1..Len(log) : KeyOf(log[i]) = k } IN
    IF idxs = {} THEN 0
    ELSE CHOOSE i \in idxs : \A j \in idxs : i >= j

\* For each key, whether the last touching record is a put.
IsLive(k) ==
  LET i == LastIndexOfKey(k) IN
    /\ i > 0
    /\ log[i][1] = "put"

\* Value of a live key under LWW.
LiveValue(k) ==
  LET i == LastIndexOfKey(k) IN
    log[i][3]

\* The rebuilt keydir, modelled as a function Keys -> {NULL} \cup Values.
NULL == "<<none>>"

Keydir ==
  [ k \in Keys |-> IF IsLive(k) THEN LiveValue(k) ELSE NULL ]

----------------------------------------------------------------------------
(* Invariants *)
----------------------------------------------------------------------------

\* Keydir agrees with the projection (definitionally identical).
KeydirIsProjection ==
  \A k \in Keys :
    /\ ~IsLive(k) => Keydir[k] = NULL
    /\  IsLive(k) => Keydir[k] = LiveValue(k)

\* If `put(k, v_j)` is the last record mentioning k, the keydir holds v_j.
LastWriteWins ==
  \A k \in Keys :
    \A j \in 1..Len(log) :
      ( /\ log[j] \in { PutRec(k, v) : v \in Values }
        /\ \A m \in (j+1)..Len(log) : KeyOf(log[m]) # k ) =>
        ( IsLive(k) /\ LiveValue(k) = log[j][3] )

\* A del after a put removes the key.
TombstoneDeletion ==
  \A k \in Keys :
    \A i, j \in 1..Len(log) :
      ( /\ i < j
        /\ log[i] \in { PutRec(k, v) : v \in Values }
        /\ log[j] = DelRec(k)
        /\ \A m \in (j+1)..Len(log) : KeyOf(log[m]) # k ) =>
        ~ IsLive(k)

\* A put after a del re-creates the key (resurrection).
PutAfterDeleteResurrectsNewValue ==
  \A k \in Keys :
    \A i, j \in 1..Len(log) :
      ( /\ i < j
        /\ log[i] = DelRec(k)
        /\ log[j] \in { PutRec(k, v) : v \in Values }
        /\ \A m \in (j+1)..Len(log) : KeyOf(log[m]) # k ) =>
        ( IsLive(k) /\ LiveValue(k) = log[j][3] )

=============================================================================
