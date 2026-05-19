# datawal roadmap

This document records what the **v0.1-pre** release actually contains, what
was explicitly excluded from it, and what is on the table for later versions.

It is the binding scope statement for the current feature cut
`feat: implement RecordLog and DataWal core v0.1-pre`. Anything not listed
under "Inside v0.1-pre" should be assumed **not implemented**.

The goal of v0.1-pre is to be small, correct, and useful for plain
append-only logs and tiny local key/value state. It is *not* a replacement
for a real database, a real WAL system, or a real object store.

---

## v0.1-pre: in scope

### 1. `RecordLog` (functional)

| Feature                                        | Status |
| ---------------------------------------------- | :----: |
| `RecordLog::open(dir)`                         |   ✅   |
| `RecordLog::append(payload)`                   |   ✅   |
| `RecordLog::append_record(type, key, payload)` |   ✅   |
| `RecordLog::scan()`                            |   ✅   |
| `RecordLog::fsync()`                           |   ✅   |
| `RecordLog::rotate()`                          |   ✅   |
| `RecordLog::close()`                           |   ✅   |
| `RecordRef { segment, offset, len }`           |   ✅   |
| `Record { type, txid, key, payload, ... }`     |   ✅   |
| `RecoveryReport`                               |   ✅   |

### 2. Framed binary record format

| Field             | Status                                  |
| ----------------- | :-------------------------------------: |
| `magic = b"DWAL"` |                    ✅                   |
| `version u16 LE`  |                    ✅                   |
| `record_type`     |          ✅ (Raw / Put / Delete)         |
| `flags u8 = 0`    |        ✅ (reserved, must be zero)       |
| `txid u64 LE`     |             ✅ (monotonic)              |
| `key_len u32 LE`  |                    ✅                   |
| `payload_len u32` |                    ✅                   |
| CRC per record    | ✅ (CRC32-IEEE in v0.1-pre, see caveat) |
| `MAX_KEY_LEN`     |           ✅ (default 64 KiB)           |
| `MAX_PAYLOAD_LEN` |           ✅ (default 64 MiB)           |

CRC caveat: the on-disk field is named `crc32c` for forward compatibility,
but v0.1-pre uses CRC32-IEEE (`crc32fast`). Switching to true CRC32C in a
later version requires a `WIRE_VERSION` bump.

### 3. Segments

| Feature                            | Status |
| ---------------------------------- | :----: |
| Files named `00000001.dwal` (8 dg) |   ✅   |
| Active segment = highest id        |   ✅   |
| `scan()` reads ascending order     |   ✅   |
| Manual `rotate()` creates next id  |   ✅   |
| Soft target segment size constant  |   🟡  |

No explicit MANIFEST. The directory listing is the source of truth.

### 4. Recovery

| Case                       | Status |
| -------------------------- | :----: |
| Apply longest valid prefix |   ✅   |
| Truncated tail tolerated   |   ✅   |
| CRC error in tail reported |   ✅   |
| CRC error mid-stream       | ✅ err  |
| Unknown magic              | ✅ err  |
| Unknown version            | ✅ err  |
| Unknown record type        | ✅ err  |
| `RecoveryReport` populated |   ✅   |

Recovery never physically truncates files. Damaged bytes at the end of the
active segment are *logically* ignored and reported.

### 5. `DataWal` (bytes-based key/value)

| Feature                | Status |
| ---------------------- | :----: |
| `DataWal::open(dir)`   |   ✅   |
| `put(key, value)`      |   ✅   |
| `get(key)`             |   ✅   |
| `delete(key)`          |   ✅   |
| `contains_key(key)`    |   ✅   |
| `len()` / `is_empty()` |   ✅   |
| `keys()` / `items()`   |   ✅   |
| Reopen rebuilds keydir |   ✅   |
| Last-write-wins        |   ✅   |
| Put-after-delete       |   ✅   |

Keys and values are bytes. The core does not parse JSON or any other
encoding.

### 6. Compaction

| Feature                            | Status                      |
| ---------------------------------- | :-------------------------: |
| `compact_to(out_dir)`              |              ✅              |
| `CompactionStats`                  |              ✅              |
| In-place `compact()`               | ❌ (deferred, not safe yet) |
| No resurrection of deleted keys    |   ✅ (covered by tests)     |
| Live state preserved               |   ✅ (covered by tests)     |

### 7. Export

| Feature                                          | Status |
| ------------------------------------------------ | :----: |
| `export_jsonl(out)` writes live state            |   ✅   |
| `{"key_b64": "...", "value_b64": "..."}` format  |   ✅   |
| Uses `safeatomic-rs::write_atomic` for the file  |   ✅   |
| Raw/audit dump of every physical record          |   ❌   |

### 8. `safeatomic-rs` integration

| Feature                                              | Status |
| ---------------------------------------------------- | :----: |
| `safeatomic-rs` is a real dependency now             |   ✅   |
| Used for atomic writes in export and `compact_to`    |   ✅   |
| No upstream changes to `safeatomic-rs` from this cut |   ✅   |

### 9. Tests

42 tests total in v0.1-pre:

```
src/format.rs::tests          — 11 unit tests (encode/decode)
src/segment.rs::tests         —  4 unit tests
src/lock.rs::tests            —  2 unit tests
src/record_log.rs internal    —  1 const-assert block (compile-time)

tests/record_log.rs           — 12 tests
tests/datawal.rs              —  9 tests
tests/integration.rs          —  3 tests
```

All green on `cargo test --workspace`.

### 10. Examples

| Example                          | Status |
| -------------------------------- | :----: |
| `examples/record_log_demo.rs`    |   ✅   |
| `examples/datawal_kv_demo.rs`    |   ✅   |
| Old `examples/skeleton.rs`       |   ❌   |

---

## v0.1-pre: out of scope

These are *explicitly* not in v0.1-pre. They might land in later versions
but should not be assumed to exist today.

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

The `flags` byte exists, but in v0.1-pre it must be `0`. Any non-zero value
is rejected by the decoder.

### 4. Semantic codecs in the core

- `JsonCodec<T>` baked into the core
- MessagePack / CBOR codecs
- Any form of "trusted pickle" loader
- The Rust core interpreting JSON

In v0.1-pre, the Rust core deals only in bytes.

### 5. Formal TLA+ specs

- `RecordLog.tla`
- `KeydirProjection.tla`
- `Compaction.tla`
- A real TLC harness

The `formal/` directory describes a *target* protocol. v0.1-pre ships
without verified specs.

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

v0.1-pre ships only a best-effort single-writer advisory lock based on
`OpenOptions::create_new` against a `.lock` file. A crashed writer leaves a
stale lock that has to be removed manually.

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

**In v0.1-pre:**

```text
RecordLog binary with CRC + segments + scan/recovery
+ DataWal bytes KV + tombstone
+ compact_to + export_jsonl + tests + two examples.
```

**Out of v0.1-pre:**

```text
Python, CAS, compression, semantic codecs, full TLA+,
server, multi-writer, transactions, hint files, explicit
manifest, query.
```

---

## Acceptance criteria (must keep working)

The feat is only considered "done" while both of these snippets keep
working on `cargo test --workspace`.

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

If either of those breaks, v0.1-pre is regressed.

---

## What v0.2 is *likely* to add

Not a commitment. Just the most plausible next step, based on what was
explicitly deferred above:

- True CRC32C (and a `WIRE_VERSION` bump).
- `fs2` / `fd-lock` based file locking, with stale-lock recovery.
- A `JsonCodec<T>` helper crate built on top of the bytes core.
- PyO3 bindings for `RecordLog` and `DataWal`.
- A real `compact()` in place (only if it can be made obviously safe).
- A first cut of the TLA+ specs for `RecordLog` and `KeydirProjection`.

CAS, server, multi-writer, transactions, query — those stay out of the
near-term roadmap.
