# datawal roadmap

This document records what the **v0.1.0-alpha** release actually contains,
what was explicitly excluded from it, and what is on the table for later
versions.

It is the binding scope statement for the current cut. Anything not
listed under "Inside v0.1.0-alpha" should be assumed **not implemented**.

The goal of v0.1.0-alpha is to be small, correct, recoverable, and
useful for plain append-only logs and tiny local key/value state. It is
*not* a replacement for a real database, a real WAL system, or a real
object store.

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
  TLC: `RecordLog`, `KeydirProjection`, `Compaction`.
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

| Model                       | Status |
| --------------------------- | :----: |
| `formal/RecordLog.tla`      |   OK   |
| `formal/KeydirProjection.tla` |  OK  |
| `formal/Compaction.tla`     |   OK   |
| `formal/reports/*.txt`      |   OK   |
| Reader / `ReadWhileWrite`   |   no   |

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

58 tests total in v0.1.0-alpha:

```
src/format.rs::tests                — 12 unit tests (encode/decode + CRC vector)
src/segment.rs::tests               —  4 unit tests
src/lock.rs::tests                  —  4 unit tests (fs2 fd lock)
src/record_log.rs internal          —  1 const-assert block (compile-time)

tests/record_log.rs                 — 14 tests
tests/datawal.rs                    —  9 tests
tests/integration.rs                —  3 tests
tests/corpus_fixtures.rs            — 11 tests
```

All green on `cargo test --workspace`.

### 14. Examples

| Example                                | Status |
| -------------------------------------- | :----: |
| `examples/record_log_demo.rs`          |   OK   |
| `examples/datawal_kv_demo.rs`          |   OK   |
| `examples/tail_recovery_demo.rs`       |   OK   |
| `examples/gen_corpus.rs` (regenerator) |   OK   |

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

Not a commitment. The most plausible next steps:

- A `JsonCodec<T>` helper crate built on top of the bytes core.
- PyO3 bindings for `RecordLog` and `DataWal`.
- A real `compact()` in place (only if it can be made obviously safe).
- A `ReadWhileWrite` TLA+ model alongside a real reader API.
- Optional `zstd` per-record compression (uses one bit in `flags`).
- Group commit / `fsync_policy` if a real workload demands it.

CAS, server, multi-writer, transactions, query — those stay out of the
near-term roadmap.
