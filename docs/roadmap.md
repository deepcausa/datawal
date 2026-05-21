# datawal roadmap

This document records what the **v0.1.0-alpha** release contained as
the original scope statement, what was deliberately excluded from it,
and what has shipped since.

The current published version is **`0.1.5`** on crates.io
(`0.1.0-alpha.1`, `0.1.4`, `0.1.5`). The on-disk wire format
(`WIRE_VERSION = 1`) is frozen and locked by corpus fixtures. The public
Rust surface listed in `lib.rs` is stable for `0.1.x`.

The sections below preserve the v0.1.0-alpha scope tables as the
*original* baseline. What landed on `main` after the alpha is tracked
under "Delivered since the alpha cut" and in the "Public roadmap issues"
table at the bottom.

The goal of `datawal` is to be small, correct, recoverable, and useful
for plain append-only logs and tiny local key/value state. It is *not*
a replacement for a real database, a real WAL system, or a real object
store.

`v0.1.0-alpha` succeeds the earlier `v0.1-pre` walking-skeleton cut. The
deltas from `v0.1-pre` are:

- CRC switched from CRC-32 IEEE (`crc32fast`) to **real CRC-32C
  Castagnoli** via the `crc32c` crate, pinned by a known-vector test.
  No `WIRE_VERSION` bump was needed because no external consumer
  existed.
- Single-writer lock switched from a sentinel-file create to an
  **OS-level fd-based advisory lock** via `fs2::FileExt::try_lock_exclusive`.
- **Durability boundary** documented explicitly: `append` is framed and
  recoverable; durable across crashes only after `fsync` returns.
- Three **TLA+ models** added under `formal/` and model-checked with
  TLC: `RecordLog`, `KeydirProjection`, `Compaction`. (A fourth model,
  `ReadWhileWrite`, landed post-alpha alongside `RecordLogReader`.)
- A **wire-format corpus** committed under
  `crates/datawal-core/tests/corpus/`, plus tests that scan, reopen,
  and validate the fixtures.
- A `tail_recovery_demo` example illustrating tail-truncation recovery.

---

## v0.1.0-alpha: in scope

### 1. `RecordLog` (functional)

| Feature                                        | Status |
| ---------------------------------------------- | :----: |
| `RecordLog::open(dir)`                         |   OK   |
| `RecordLog::append(payload)`                   |   OK   |
| `RecordLog::append_record(type, key, payload)` |   OK   |
| `RecordLog::scan()`                            |   OK   |
| `RecordLog::recovery_report()`                 |   OK   |
| `RecordLog::fsync()`                           |   OK   |
| `RecordLog::rotate()`                          |   OK   |
| `RecordLog::close()`                           |   OK   |
| `RecordRef { segment, offset, len }`           |   OK   |
| `Record { type, txid, key, payload, ... }`     |   OK   |
| `RecoveryReport`                               |   OK   |

### 2. Framed binary record format

| Field             | Status                                  |
| ----------------- | :-------------------------------------: |
| `magic = b"DWAL"` |                  OK                     |
| `version u16 LE`  |              OK (= 1)                   |
| `record_type`     |       OK (Raw / Put / Delete)           |
| `flags u8 = 0`    |     OK (reserved, must be zero)         |
| `txid u64 LE`     |           OK (monotonic)                |
| `key_len u32 LE`  |                  OK                     |
| `payload_len u32` |                  OK                     |
| CRC per record    |   OK (real CRC-32C Castagnoli)          |
| `MAX_KEY_LEN`     |         OK (default 64 KiB)             |
| `MAX_PAYLOAD_LEN` |         OK (default 64 MiB)             |

CRC: `crc32c` crate, polynomial `0x1EDC6F41`, pinned by a known-vector
test in `format.rs` against RFC 3720 reference values. A sentinel
`assert_ne!` against the CRC-32 IEEE result detects any silent
regression.

### 3. Segments

| Feature                            | Status |
| ---------------------------------- | :----: |
| Files named `00000001.dwal` (8 dg) |   OK   |
| Active segment = highest id        |   OK   |
| `scan()` reads ascending order     |   OK   |
| Manual `rotate()` creates next id  |   OK   |

No explicit MANIFEST. The directory listing is the source of truth.

### 4. Recovery

| Case                       | Status |
| -------------------------- | :----: |
| Apply longest valid prefix |   OK   |
| Truncated tail tolerated   |   OK   |
| CRC error in tail reported |   OK   |
| CRC error mid-stream       |  err   |
| Unknown magic              |  err   |
| Unknown version            |  err   |
| Unknown record type        |  err   |
| Reserved flags non-zero    |  err   |
| `RecoveryReport` populated |   OK   |

Recovery never physically truncates files. Damaged bytes at the end of
the active segment are *logically* ignored and reported.

### 5. `DataWal` (bytes-based key/value)

| Feature                | Status |
| ---------------------- | :----: |
| `DataWal::open(dir)`   |   OK   |
| `put(key, value)`      |   OK   |
| `get(key)`             |   OK   |
| `delete(key)`          |   OK   |
| `contains_key(key)`    |   OK   |
| `len()` / `is_empty()` |   OK   |
| `keys()` / `items()`   |   OK   |
| Reopen rebuilds keydir |   OK   |
| Last-write-wins        |   OK   |
| Put-after-delete       |   OK   |

Keys and values are bytes. The core does not parse JSON or any other
encoding.

### 6. Compaction

| Feature                            | Status                       |
| ---------------------------------- | :--------------------------: |
| `compact_to(out_dir)`              |             OK               |
| `CompactionStats`                  |             OK               |
| In-place `compact()`               | no (deferred, not safe yet)  |
| No resurrection of deleted keys    |   OK (covered by tests)      |
| Live state preserved               |   OK (covered by tests)      |

### 7. Export

| Feature                                          | Status |
| ------------------------------------------------ | :----: |
| `export_jsonl(out)` writes live state            |   OK   |
| `{"key_b64": "...", "value_b64": "..."}` format  |   OK   |
| Uses `safeatomic-rs::write_atomic` for the file  |   OK   |
| Raw/audit dump of every physical record          |   no   |

### 8. Single-writer lock

| Feature                                                              | Status |
| -------------------------------------------------------------------- | :----: |
| Exclusive advisory lock via `fs2::FileExt::try_lock_exclusive`       |   OK   |
| Held by file descriptor, released on `Drop` / process exit           |   OK   |
| Second `RecordLog::open` on same dir fails fast                      |   OK   |
| Sentinel `.lock` file persists between runs (not itself the lock)    |   OK   |
| Stale `.lock` from a crashed previous process is not a problem       |   OK   |
| Mandatory locking / multi-writer                                     |   no   |

### 9. Durability

| Property                                                              | Status |
| --------------------------------------------------------------------- | :----: |
| `append` produces a framed, recoverable record                        |   OK   |
| Durability across host crash requires `RecordLog::fsync` to return    |   OK   |
| `fsync` syncs active segment file and fsyncs the containing directory |   OK   |
| `fsync_policy`, `fdatasync`, group commit, per-batch atomic commit    |   no   |

Callers requiring per-record durability must pair every `append` with an
`fsync`.

### 10. `safeatomic-rs` integration

| Feature                                              | Status |
| ---------------------------------------------------- | :----: |
| `safeatomic-rs` is a real dependency                 |   OK   |
| Used for `write_atomic` (JSONL export)               |   OK   |
| Used for `fsync_dir` (segment create / rotate / fsync) |  OK  |
| No upstream changes to `safeatomic-rs` from this cut |   OK   |

### 11. Formal models (model-checked, not verified Rust)

| Model                            | Status (in alpha) |
| -------------------------------- | :---------------: |
| `formal/RecordLog.tla`           |        OK         |
| `formal/KeydirProjection.tla`    |        OK         |
| `formal/Compaction.tla`          |        OK         |
| `formal/reports/*.txt`           |        OK         |
| Reader / `ReadWhileWrite`        |        no         |

(`formal/ReadWhileWrite.tla` landed after the alpha. See "Delivered
since the alpha cut".)

Wording: "model-checked under documented assumptions". Not "formally
verified". Models do not check the Rust implementation.

### 12. Wire-format corpus

| Fixture                                              | Status |
| ---------------------------------------------------- | :----: |
| `tests/corpus/valid_log/`                            |   OK   |
| `tests/corpus/truncated_tail/`                       |   OK   |
| `tests/corpus/bad_crc/`                              |   OK   |
| `tests/corpus/unknown_version/`                      |   OK   |
| `tests/corpus/delete_tombstone/`                     |   OK   |
| `tests/corpus/compact_to_output/`                    |   OK   |
| `examples/gen_corpus.rs` regenerator (run-on-demand) |   OK   |
| `tests/corpus_fixtures.rs` (11 tests)                |   OK   |

Fixtures freeze the v0.1 on-disk format. They are read-only by tests.

### 13. Tests

The v0.1.0-alpha cut shipped roughly 58 tests covering unit
encode/decode + CRC vector, segment primitives, the `fs2` lock,
`RecordLog`, `DataWal`, cross-cutting integration and corpus
fixtures. The count is not stable across versions and is not part of
the contract; `cargo test --workspace` is the only authoritative
signal. Post-alpha test additions are listed under "Delivered since
the alpha cut".

### 14. Examples (v0.1.0-alpha cut)

| Example                                | Status |
| -------------------------------------- | :----: |
| `examples/record_log_demo.rs`          |   OK   |
| `examples/datawal_kv_demo.rs`          |   OK   |
| `examples/tail_recovery_demo.rs`       |   OK   |
| `examples/gen_corpus.rs` (regenerator) |   OK   |

Post-alpha examples (`gen_soak_fixtures`, `soak`, `power_loss_workload`,
`power_loss_validate`) are described under "Delivered since the alpha
cut".

---

## v0.1.0-alpha: out of scope

These are *explicitly* not in v0.1.0-alpha. They might land in later
versions but should not be assumed to exist today.

### 1. Python / PyO3 bindings

- PyO3 module
- `maturin` build
- wheel publication
- A `datawal` Python package
- `pandas` / `arrow` / `duckdb` integration

### 2. Content-addressed storage / blob / dedup

- A `datawal-cas` crate
- SHA-256 blob references
- Chunking (FastCDC and friends)
- Blob garbage collection
- Hybrid inline-vs-blob payload routing

### 3. Compression

- LZ4, Zstd, gzip
- Real compression flags
- Decompression failure handling

The `flags` byte exists, but in v0.1.0-alpha it must be `0`. Any
non-zero value is rejected by the decoder.

### 4. Semantic codecs in the core

- `JsonCodec<T>` baked into the core
- MessagePack / CBOR codecs
- Any form of "trusted pickle" loader
- The Rust core interpreting JSON

In v0.1.0-alpha, the Rust core deals only in bytes.

### 5. Reader API / `ReadWhileWrite` model

- Concurrent readers seeing a consistent view
- A separate read-path TLA+ model
- Snapshot / point-in-time read handles

*(This item was delivered after the alpha. See "Delivered since the
alpha cut".)*

### 6. Server / multi-user / multi-db

- A network server
- RPC layer
- AuthN/AuthZ
- Multi-user
- Tenant isolation
- Multi-database manager
- `RequestContext` types

### 7. Multi-writer and advanced locking

- Multi-writer support
- Distributed locks
- etcd / Postgres advisory lock backends
- Fairness or deadlock detection
- Shared / reader locks

The lock is single-writer, cooperative, OS-level advisory. Mandatory
locking is out of scope.

### 8. Advanced WAL features

- Group commit
- A full `fsync_policy`
- Per-interval flush
- Background fsync threads
- Multi-record transactions
- `BEGIN` / `COMMIT` / `ABORT`
- A separate audit log

### 9. Hint files / explicit manifest

- Hint files (Bitcask-style)
- An explicit `MANIFEST`
- A `CURRENT` pointer
- Stable snapshot handles
- Background compaction

### 10. Query / indexes

- Secondary indexes
- Filter / groupby / aggregate
- Promised range scans
- SQL
- DataFrame semantics
- Cache eviction / TTL

---

## One-line cut

**In v0.1.0-alpha:**

```text
RecordLog binary with real CRC-32C + segments + scan/recovery
+ DataWal bytes KV + tombstone
+ compact_to + export_jsonl
+ fs2 fd-based advisory lock
+ TLA+ models (RecordLog / KeydirProjection / Compaction)
+ wire-format corpus
+ tests + examples.
```

**Out of v0.1.0-alpha:**

```text
Python, CAS, compression, semantic codecs, reader API,
server, multi-writer, transactions, hint files, explicit
manifest, query, in-place compact.
```

---

## Protocol freeze

**Do not edit `format.rs` or `record_log.rs` without expecting corpus,
TLA+ model, and wire-format consequences.**

Specifically:

- Any change to the byte layout requires regenerating `tests/corpus/` and
  bumping `WIRE_VERSION`.
- Any change to recovery semantics requires updating `formal/RecordLog.tla`
  and re-running TLC.
- Any change to keydir semantics requires updating
  `formal/KeydirProjection.tla` and `formal/ReadWhileWrite.tla`.
- Any change to compaction requires updating `formal/Compaction.tla`.

`WIRE_VERSION = 1` is frozen since v0.1.0-alpha and is locked by the
binary corpus fixtures under `crates/datawal-core/tests/corpus/` (CI
job `corpus`). Later versions that must change the wire format will
bump `WIRE_VERSION` and document the migration path.

---

## Delivered since the alpha cut

The items below shipped on `main` after v0.1.0-alpha without changing
`WIRE_VERSION`, the corpus fixtures, or the public surface in
`lib.rs`. They are reachable from the current crate version on
crates.io (`0.1.5`).

### Track A (Rust production track) — delivered

| Item                                              | Where it lives                                                              | Issue / PR     |
| ------------------------------------------------- | --------------------------------------------------------------------------- | -------------- |
| Criterion benchmarks                              | `crates/datawal-core/benches/`                                              | #1, PR #15     |
| `cargo-fuzz` harness on the wire-format decoder   | `fuzz/`                                                                     | #2             |
| Streaming `scan_iter`                             | `RecordLog::scan_iter` in `record_log.rs`                                   | #3             |
| Keydir by offset (`RecordRef` in keydir)          | `datawal.rs` (CRC verify still mandatory on `get`)                          | #4             |
| Snapshot reader (`RecordLogReader`)               | `record_log_reader.rs`; TLA+ model `formal/ReadWhileWrite.tla`              | #5             |
| Crash-injection tests                             | `tests/crash_injection.rs` (kill during append, rotate, compact, export)    | #8             |
| Soak harness                                      | `examples/soak.rs`, `scripts/run_soak_real.sh`, `docs/soak.md`              | #8             |
| ENOSPC + poison-writer tests                      | `tests/` (post-alpha hardening tier)                                        | issue #22 umbrella |
| `datawal` CLI (read + mutate, JSONL export, scan) | `crates/datawal-cli/`                                                       | issue #22 umbrella |
| First non-alpha release: `0.1.4`                  | `cargo add datawal --version 0.1.4`                                         | PR #30         |
| Power-loss simulation harness via `dm-flakey`     | `examples/power_loss_{workload,validate}.rs`, `scripts/power_loss_dm_flakey.sh`, `scripts/power_loss_cleanup.sh`, `docs/power-loss-testing.md`, `docs/power-loss-results.md` | #31, PR #35 (v0.1.5) |
| README rewritten for post-alpha framing           | `README.md` "Durability evidence" section                                   | v0.1.5         |

### Track A — still open

- `fdatasync` / configurable `fsync_policy` and group commit (issue #9).
- `append_batch` / multi-record entries with a shared fsync boundary
  (issue #11, exploratory; **not** a SQL transaction).
- Preallocated segments to reduce append latency variance (issue #10).
- Power-loss simulation v2 via `dm-log-writes` replay (issue #31 v2,
  follow-up to the v1 `dm-flakey` harness landed in 0.1.5). Captures
  the I/O log and asserts every intermediate state, not just the
  final one. Linux-only, root-only, never CI.
- CI matrix on macOS / Windows in addition to Linux. Some of the
  hardening harnesses are Linux-only by construction (dm-flakey, soak
  with `fsync_dir` accounting); the unit + integration matrix can
  still be widened.

Future compaction work must remain snapshot/replace-style unless a new
protocol, corpus update and formal model explicitly accept otherwise.
In-place mutation of existing segment files is not part of the
roadmap.

### Track B — Python visibility track

Status: **not started**. Tracked as issue #6.

- PyO3 bindings for `RecordLog` and `DataWal`.
- `maturin` build and wheel publication.
- Minimal Python API exposing `RecordLog.append / scan / fsync` and
  `DataWal.put / get / delete`.
- Python examples, `pytest` tests, docs for Python users.
- No CAS in this track.

### Direction

```text
Track A is largely delivered and continues with fsync policy, group
commit, append_batch, preallocated segments and dm-log-writes v2.
Track B (PyO3) remains unstarted; first real Python consumer would
trigger it.
```

The two tracks are independent. Track B can start without further
Track A work. Track A does not require Python. The order depends on
where the first real consumer is.

---

## Public roadmap issues

The list below mirrors what is actually tracked on GitHub. This document
is the consolidated picture; each issue is the executable unit. If the
two disagree, the issues win — please open a PR against this section.

| Issue                                                                                          | Track       | Status          | Notes                                                                            |
| ---------------------------------------------------------------------------------------------- | ----------- | --------------- | -------------------------------------------------------------------------------- |
| [#1](https://github.com/deepcausa/datawal/issues/1) Benchmark `RecordLog` and `DataWal`        | Track A     | done (PR #15)   | Criterion benches under `crates/datawal-core/benches/`; CI runs `--no-run` only. |
| [#2](https://github.com/deepcausa/datawal/issues/2) Fuzz the wire-format decoder               | Track A     | done            | `cargo-fuzz` harness under `fuzz/`; targets `format::decode_frame` + roundtrip. CI job `fuzz build (nightly)`. |
| [#3](https://github.com/deepcausa/datawal/issues/3) Streaming `scan` iterator                  | Track A     | done            | `scan_iter` shipped; `scan()` retained for now.                                  |
| [#4](https://github.com/deepcausa/datawal/issues/4) Keydir by offset, not full values          | Track A     | done            | Keydir uses `RecordRef`; CRC verify on `get` is mandatory and remains so.        |
| [#5](https://github.com/deepcausa/datawal/issues/5) Reader API + `ReadWhileWrite` model        | Track A     | done            | `RecordLogReader` shipped; `formal/ReadWhileWrite.tla` model-checked in CI.      |
| [#6](https://github.com/deepcausa/datawal/issues/6) Python bindings via PyO3                   | Track B     | open            | PyO3 + maturin, bytes-first surface; not started.                                |
| [#7](https://github.com/deepcausa/datawal/issues/7) CAS / blob storage is a separate crate     | scope       | open            | Out of scope for `datawal-core`. Separate crate concern.                         |
| [#8](https://github.com/deepcausa/datawal/issues/8) Crash injection + soak testing             | Track A     | done            | `tests/crash_injection.rs` + `examples/soak.rs` + `scripts/run_soak_real.sh`.    |
| [#9](https://github.com/deepcausa/datawal/issues/9) Durability policy + group commit           | Track A     | open            | `FsyncPolicy` enum. Bench before adding a background thread.                     |
| [#10](https://github.com/deepcausa/datawal/issues/10) Preallocated segments                    | Track A     | open            | Reduce append latency variance; scanner must distinguish unused vs torn.         |
| [#11](https://github.com/deepcausa/datawal/issues/11) `append_batch` / multi-record entries    | Track A     | exploratory     | Shared fsync boundary for related records. **Not** a SQL transaction.            |
| [#12](https://github.com/deepcausa/datawal/issues/12) WAL-engine features and related work     | scope       | open (notes)    | Reference list (incl. `okaywal`). Features adopted only when justified.          |
| [#13](https://github.com/deepcausa/datawal/issues/13) Guardrails: datawal is JSONL, not a DB   | scope       | open (charter)  | Canonical scope statement; SQL / query / multi-writer / server remain non-goals. |
| [#14](https://github.com/deepcausa/datawal/issues/14) Derived tag index for simple filtering   | exploratory | open (research) | Must remain a deterministic projection. No SQL drift, no general indexes.        |
| [#22](https://github.com/deepcausa/datawal/issues/22) Roadmap: hardening + CLI + 0.1.4 release | Track A     | done (closed)   | Umbrella for the 8 PRs that took the crate from alpha to 0.1.4. Closed 2026-05-21. |
| [#31](https://github.com/deepcausa/datawal/issues/31) Power-loss simulation via Linux device-mapper | Track A | v1 done (0.1.5) | v1: `dm-flakey` harness shipped in 0.1.5 (PR #35). v2: `dm-log-writes` replay still open. Linux-only, root-only, not CI. See `docs/power-loss-testing.md` + `docs/power-loss-results.md`. |

Bodies on GitHub stay authoritative for acceptance criteria and open
questions. This table only records track, status and the one-line cut.

---

## Acceptance criteria (must keep working)

These snippets must keep passing on `cargo test --workspace`.

### RecordLog

```rust
let mut log = RecordLog::open(path)?;
log.append(b"one")?;
log.append(b"two")?;
log.fsync()?;

let records = log.scan()?;
assert_eq!(records[0].payload, b"one");
assert_eq!(records[1].payload, b"two");
```

### DataWal

```rust
let mut db = DataWal::open(path)?;
db.put(b"a", b"1")?;
db.put(b"b", b"2")?;
db.put(b"a", b"3")?;
db.delete(b"b")?;

assert_eq!(db.get(b"a")?, Some(b"3".to_vec()));
assert_eq!(db.get(b"b")?, None);

db.compact_to(clean_path)?;
db.export_jsonl(out_path)?;
```

If either of those breaks, v0.1.0-alpha is regressed.

---

## What v0.2 is *likely* to add

Not a commitment. Depends on which track (A or B) is prioritised next
and what real workloads reveal. Plausible items:

- A `JsonCodec<T>` helper crate on top of the bytes core.
- PyO3 bindings (Track B).
- Group commit / `fsync_policy` if a real workload demands it
  (issue #9).
- `append_batch` for shared-fsync record groups (issue #11).
- Preallocated segments to flatten append latency (issue #10).
- Power-loss harness v2 via `dm-log-writes` (issue #31 v2).
- Optional `zstd` per-record compression (uses one bit in `flags`).

CAS, server, multi-writer, transactions, query — those stay out of the
near-term roadmap. CAS/blob is a separate crate concern (`datablob-rs`),
not a `datawal-core` feature. Chunking (FastCDC) is above the CAS layer
and is not part of `datawal-core` either.
