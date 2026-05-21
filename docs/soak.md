# Soak

This page documents the **soak example**, which exercises a long
`DataWal` lifetime under put / delete / fsync / compact load and
checks the live-set invariant after a drop + reopen cycle.

The soak is shipped as a runnable example (`examples/soak.rs`), not
as a benchmark and not as a CI job. It is intentionally **not** part
of the default test suite. It is a tool you run by hand against a
machine you trust to have wall-clock budget, real local disk, and a
predictable filesystem.

It is **not** a load test, **not** a stress test against a target
QPS, and **not** evidence that the crate is "production-ready". It is
a fixed-shape workload that runs as long as you ask it to, samples
process metrics (Linux only), and writes a CSV of what happened.

## What it does

The driver opens a fresh `DataWal` in a working directory and runs a
deterministic loop:

- ~95% `put`, ~5% `delete`, with the key/value bytes drawn from one
  of three streams (small / medium / large, weights 70 / 25 / 5).
- An explicit `fsync` every 1 000 ops.
- A `compact_to` swap every `DATAWAL_SOAK_ROTATE_EVERY *
  DATAWAL_SOAK_COMPACT_EVERY_ROTATIONS` ops (defaults: 5 000 × 4 =
  20 000 ops between compactions). After each compaction the source
  directory is renamed aside and the target is reopened in place; a
  full reopen exercises the longest-valid-prefix recovery path.

An in-memory `HashMap<Vec<u8>, Vec<u8>>` oracle mirrors the expected
live set. When the wall-clock budget expires the process drops the
`DataWal`, reopens the working directory, and compares the reopened
keydir against the oracle. Exit code `0` means the set matched
exactly. Exit code `1` means it did not.

There is no claim about durability under power loss, OS crash,
filesystem corruption, or hardware failure. The soak only exercises
the paths the crate already exposes from a single live process.

## Two modes

### Synthetic

```
cargo run --release -p datawal --example soak -- --mode synthetic
```

Synthetic mode generates key and payload bytes from a deterministic
in-process PRNG. No external inputs are required. This is the mode
used for ad-hoc smoke runs and the only mode reachable from clean
checkout.

The companion example `gen_soak_fixtures.rs` writes three JSONL
fixtures under `crates/datawal-core/tests/fixtures/soak/`
(`small_records.jsonl`, `medium_records.jsonl`,
`large_payloads.jsonl`). These are committed and reproducible byte
for byte via the same PRNG seeds.

### Real

```
DATAWAL_SOAK_INPUT_SMALL=/path/small.jsonl \
DATAWAL_SOAK_INPUT_MEDIUM=/path/medium.jsonl \
DATAWAL_SOAK_INPUT_LARGE=/path/large.jsonl \
cargo run --release -p datawal --example soak -- --mode real
```

Real mode reads three JSONL files chosen by the caller. The schema
is one record per line: `{"key": "<base64>", "payload": "<base64>"}`.
Real mode requires all three env vars to be set explicitly; there is
no default path. The committed fixtures are a valid starting point
but the caller is expected to point this at their own data when
running for evidence.

## Environment variables

| Variable | Default | Meaning |
| --- | --- | --- |
| `DATAWAL_SOAK_DURATION` | `1800` | Wall-clock budget in seconds. |
| `DATAWAL_SOAK_ROTATE_EVERY` | `5000` | Ops between segment rotations. |
| `DATAWAL_SOAK_COMPACT_EVERY_ROTATIONS` | `4` | Rotations between compactions. |
| `DATAWAL_SOAK_PROGRESS_SECS` | `60` | Seconds between CSV samples. |
| `DATAWAL_SOAK_LOG_DIR` | `/tmp` | Where `soak.csv` is written. |
| `DATAWAL_SOAK_WORK_DIR` | `${TMPDIR}/datawal-soak` | Working directory for the store. |
| `DATAWAL_SOAK_INPUT_SMALL` | — | Real-mode small-stream JSONL path. |
| `DATAWAL_SOAK_INPUT_MEDIUM` | — | Real-mode medium-stream JSONL path. |
| `DATAWAL_SOAK_INPUT_LARGE` | — | Real-mode large-stream JSONL path. |

## CSV columns

The CSV at `${DATAWAL_SOAK_LOG_DIR}/soak.csv` has the header

```
elapsed_s,rss_kb,fds,segments,live_keys,puts,deletes,rotates,compacts,bytes_written
```

`rss_kb` and `fds` are read from `/proc/self/status` and
`/proc/self/fd/` and are populated only on Linux. On other platforms
both columns are blank.

`segments` counts files matching `[0-9]{8}.dwal` in the working
directory.

`live_keys` is the in-memory oracle size, not the on-disk keydir.

## Exit codes

| Code | Meaning |
| --- | --- |
| `0` | Final consistency check passed: reopened keydir matches the oracle. |
| `1` | Invariant violated: reopened keydir differs from the oracle. |
| `2` | Setup error (missing env var, fixture not found, work-dir not creatable). |

## Interpreting a run

- A clean run prints a final `OK` line on stdout and exits `0`.
- The CSV columns are intentionally simple. Look for drift over
  time: a healthy run keeps `rss_kb` bounded, `fds` bounded, and
  `segments` bounded under steady-state compaction.
- Spikes in `bytes_written` correspond to compactions writing the
  fresh snapshot.
- A failed run prints a diagnostic with the sizes of the symmetric
  difference between the reopened keydir and the oracle. The work
  directory is left in place so the caller can inspect it.

## What this is not

- Not a benchmark. Numbers from a soak are not comparable across
  machines.
- Not a load test. There is no target QPS, no SLA, no concurrency.
  The driver is single-threaded by design because the crate is
  single-writer.
- Not part of CI. The example only has to **compile**; it is not
  executed automatically. CI signals are unchanged.
- Not a substitute for the property tests in
  `tests/proptest_recovery.rs` or the crash-injection tests in
  `tests/crash_injection.rs`. Those cover correctness; the soak
  covers behaviour over time.
