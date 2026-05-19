# datawal — formal specifications

This directory will hold TLA+ specifications for datawal's core protocols.
**No models are implemented in v0.1-pre.** The protocol shipped in
v0.1-pre is the *target* of these specifications, not a result of them.
A future release must check at least `RecordLog.tla` before any
wire-format-breaking change to the v0.1-pre protocol is accepted.

## Planned models

### `RecordLog.tla`
- **Invariants:**
  - Every record observable via `scan` was returned by a prior `append`.
  - The set of records observable after a crash is a *prefix* of the
    sequence of completed appends.
  - CRC failure truncates the log to the last valid offset; no record after
    that point is ever observable again.
- **Actions:** `Append`, `Fsync`, `Crash`, `Reopen`, `Scan`.

### `KeydirProjection.tla`
- **Invariants:**
  - For each key `k`, `DataWal::get(k)` returns the value of the *latest*
    write for `k` that precedes a tombstone or — if no tombstone — the
    latest write overall.
  - `contains_key(k)` matches `get(k).is_some()`.
  - The projection of the log equals the projection of the in-memory keydir.
- **Actions:** `Put`, `Delete`, `Get`, `Rebuild`.

### `Compaction.tla`
- **Invariants:**
  - The projection before compaction equals the projection after compaction.
  - Compaction is restartable: a crash at any point leaves a log whose
    projection equals one of the two valid endpoints.
  - No record is lost: every key present pre-compaction is present
    post-compaction (unless tombstoned).
- **Actions:** `BeginCompaction`, `WriteCompactedSegment`, `SwapManifest`,
  `Crash`, `Recover`.

### `ReadWhileWrite.tla`
- **Invariants:**
  - A reader started at time `t0` sees a consistent snapshot: the set of
    records committed at or before `t0`.
  - A writer never invalidates a reader's view mid-scan.
  - `rotate` is observable to readers only after it completes.
- **Actions:** `BeginScan`, `Append`, `Rotate`, `EndScan`.

## Order of work

1. `RecordLog.tla` first — everything else builds on it.
2. `KeydirProjection.tla` once `RecordLog` checks.
3. `Compaction.tla` once the projection is fixed.
4. `ReadWhileWrite.tla` last; it constrains the public API more than the
   on-disk format.

## Tooling

- TLC for model checking small finite instances.
- Apalache as a stretch goal for symbolic checks.
- Models live in `formal/`. Output of model runs is not committed.
