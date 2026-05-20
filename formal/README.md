# datawal — formal specifications

This directory holds TLA+ specifications for datawal's core protocols.

These models are **small, finite, and deliberately minimal**. They are
"model-checked under documented assumptions" — they are not a proof of
correctness of the Rust implementation, and the project does not claim
"formal verification". Their purpose is to:

1. Pin the protocol intent in a precise, machine-checkable form.
2. Catch obvious mistakes before they reach the wire format.
3. Survive future refactors: any wire-format-breaking change must keep
   the corresponding model checked.

## Models in this release (v0.1-pre)

### `RecordLog.tla`

Models the append-only record log, fsync, crash, and prefix recovery.

- **Variables:** `appended` (set), `buffered`/`durable` (sequences),
  `appendCount`, `crashed`.
- **Actions:** `DoAppend(r)`, `DoFsync`, `DoCrash`.
- **Invariants:**
  - `TypeInvariant`
  - `NoPartialRecordApplied` — `durable` is a prefix of the recoverable view.
  - `PrefixRecovery` — fsync only extends `durable` (monotonic).
  - `NoSpuriousRecord` — every record in `durable` was previously appended.

### `KeydirProjection.tla`

Models the keydir as a deterministic last-write-wins projection over a
sequence of put/del records.

- **Variables:** `log` (sequence of `<<"put",k,v>>` or `<<"del",k>>`).
- **Actions:** `DoPut(k,v)`, `DoDel(k)`.
- **Invariants:**
  - `KeydirIsProjection`
  - `LastWriteWins`
  - `TombstoneDeletion`
  - `PutAfterDeleteResurrectsNewValue`

### `Compaction.tla`

Models `compact_to` as a function from a source log to a new log
containing exactly one put per live key and no tombstones.

- **Variables:** `log`, `compactedLog`, `compacted`.
- **Actions:** `DoPut(k,v)`, `DoDel(k)`, `DoCompact`.
- **Invariants:**
  - `TypeInvariant`
  - `CompactionPreservesLiveState` — projections agree.
  - `NoDeletedKeyResurrection`
  - `ExportCleanCorrectness` — no tombstones, no duplicate keys.

## What is NOT modelled in this release

- **`ReadWhileWrite.tla`** — concurrent scan and append. Deferred until a
  reader API beyond `scan(&mut self)` exists.
- Multi-writer coordination (datawal is single-writer per directory).
- Filesystem-level details (real `fsync`, page cache).
- CAS / blob store integration (deferred to v0.2).

## How to run

The models target [TLC](https://github.com/tlaplus/tlaplus/) (TLA+ tools
2.19 or later). With `tla2tools.jar` available somewhere on disk:

```sh
cd apps/datawal/formal

java -XX:+UseParallelGC -cp /path/to/tla2tools.jar tlc2.TLC \
  -workers 2 -config RecordLog.cfg RecordLog.tla

java -XX:+UseParallelGC -cp /path/to/tla2tools.jar tlc2.TLC \
  -workers 2 -config KeydirProjection.cfg KeydirProjection.tla

java -XX:+UseParallelGC -cp /path/to/tla2tools.jar tlc2.TLC \
  -workers 2 -config Compaction.cfg Compaction.tla
```

The default configs use very small constants (2 keys, 2 values, 4 ops)
so each model finishes in well under a second. Increase
`MaxAppends` / `MaxOps` to widen coverage at the cost of time.

## Reports

TLC output is not committed to the repo. To inspect a run, either
re-run locally (commands above) or download the `tlc-logs` artifact
that CI uploads on every run of `.github/workflows/ci.yml`.

## Caveats

- Model checking the abstract specs does not check the Rust
  implementation. The Rust tests under `crates/datawal-core/tests/` and
  the corpus fixtures are the implementation-level verification.
- The models are deliberately small; they are aimed at catching
  protocol-level mistakes, not exhaustive enumeration of large states.
- `CHECK_DEADLOCK FALSE` is set in every config because the models
  legitimately reach quiescent states once their op counter saturates.
