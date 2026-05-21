# datawal — related work

A non-exhaustive map of nearby Rust crates and external systems, plus where
datawal is meant to land relative to them.

Entries are sketches, not reviews. The goal is not to rank projects, but to
make datawal's scope clear:

> datawal is recoverable JSONL, not a database.

It is a local record store built from:

- a framed append-only `RecordLog`;
- a bytes-first last-write-wins `DataWal` projection;
- tombstone deletes;
- snapshot-style compaction;
- clean export;
- explicit recovery and corruption-detection behavior.

## External Rust crates in nearby design space

### `okaywal`

`okaywal` is the closest WAL-layer relative.

It is more advanced than datawal as a WAL engine: it supports multi-producer
writing, fsync batching, directory fsync batching, preallocated segments,
checkpointing, segment reuse, random access by log position, and recovery hooks.

datawal deliberately starts smaller at the WAL-engine layer. Its differentiation
is not "a more advanced WAL". Its differentiation is the layer above the log:

- fixed bytes-first `DataWal` projection;
- Put/Delete records and tombstones;
- `compact_to`;
- clean JSONL export;
- wire-format corpus;
- TLA+ models for RecordLog, KeydirProjection, Compaction, and ReadWhileWrite;
- fuzz, crash-injection, ENOSPC, property, and soak tests.

Short version:

```text
okaywal = stronger WAL engine
datawal = recoverable JSONL / local record store with explicit evidence
```

datawal should learn from okaywal on fsync batching, preallocated segments,
and multi-producer single-writer APIs, but should not become a generic WAL
engine.

### `bitcasky`

`bitcasky` is closest to `DataWal`'s projection model: an append-only log with
an in-memory keydir and merge/compaction.

The overlap is conceptual:

- append records;
- rebuild an in-memory index;
- last-write-wins key/value state;
- delete through tombstones;
- compact/merge later.

datawal differs by staying bytes-first, documenting the wire format, adding
clean export, and keeping the recovery/compaction invariants explicit.

### `redb`

`redb` is a single-file MVCC embedded database.

It is a better fit when the user wants a mature embedded database abstraction.
It is not trying to expose a canonical append-only log as the main artifact.

datawal differs by keeping the log transparent and recoverable as the primary
object.

### `fjall`

`fjall` is an LSM-style embedded key/value store in Rust.

It is closer to a real storage engine than datawal. It is the right comparison
when the workload needs a mature key/value engine, not a small recoverable
record log.

datawal is intentionally smaller and more inspectable.

### `sled`

`sled` is an embedded KV with log-structured storage. It has historically been
an important Rust storage project, but its on-disk format is not the user-facing
artifact.

datawal's point of differentiation is not being a stronger embedded database.
It is the transparent log, clean export, and explicit recovery evidence.

### `rusqlite` / SQLite WAL mode

SQLite is the right answer for many workloads.

Use SQLite when you need:

- SQL;
- queries;
- indexes;
- joins;
- multi-record transactions;
- mature operational behavior.

datawal is for cases where the artifact still wants to be a local record log,
not a relational database.

### RocksDB bindings / `rocksdict`

RocksDB-backed crates are the right fit when key/value performance and mature
storage-engine behavior matter more than transparency.

datawal does not compete with RocksDB as a KV engine. It is for simpler
JSONL-shaped local persistence where recovery, auditability, and clean export
are more important than database-engine features.

## External non-Rust systems for context

### JSONL

JSONL is the mental model datawal preserves:

```text
append records
replay later
export clean data
```

But JSONL by itself is not a persistence protocol. It has no frame checksum,
no explicit recovery boundary, no tombstone semantics, no formalized compaction,
and no way to distinguish a valid tail from a torn write except ad hoc parsing.

datawal can be described as:

```text
recoverable JSONL with a documented wire format and local-state projection
```

### TFRecord

TFRecord is close to the `RecordLog` layer: framed records with length and CRC.

It is a useful comparison for framing, but it does not provide the `DataWal`
projection, tombstone semantics, `compact_to`, or clean live-state export.

### Kafka log segments

Kafka provides the large-system mental model of segmented logs.

datawal borrows the idea that logs are segmented and recoverable, but it is
local-only, single-writer, and not a distributed streaming system.

### LevelDB / RocksDB WAL

LevelDB/RocksDB WALs are useful references for framed records with checksums.

datawal is not a database WAL hidden behind an engine. Its log is the user-facing
artifact.

### Bitcask

Bitcask is the closest conceptual ancestor for `DataWal`.

The mental model is:

```text
append-only log
+ in-memory keydir
+ tombstones
+ merge/compaction
```

datawal is not a full Bitcask clone, but `DataWal` uses the same basic idea:
derive live state from an append-only log.

### Git

Git is the mental model for a future content-addressed blob layer.

That layer is intentionally not in `datawal-core`. If added, it should live as
a separate crate, e.g. `datablob-rs`, and datawal should only store references.

## Where datawal aims to sit

```text
                     transparent on-disk format
                                ▲
                                │
                                │
           TFRecord ─────────── │ ───── RecordLog
                                │
           bitcasky ─────────── │ ───── DataWal
                                │
                                │
        redb / sled / fjall ─── │
                  \             │
                   \            │
                    RocksDB ────┴── opaque storage engine
```

Another axis:

```text
WAL engine sophistication
        ▲
        │       okaywal
        │
        │
        │
        │              datawal
        │              (smaller WAL layer,
        │               stronger projection/export/evidence story)
        │
        └──────────────────────────────▶ local-state semantics
```

## What datawal optimizes for

datawal optimizes for:

1. Recoverable append-only records.
2. Transparent on-disk format.
3. CRC-checked frames.
4. Valid-prefix recovery.
5. Last-write-wins bytes KV projection.
6. Tombstone deletes.
7. Snapshot-style `compact_to`.
8. Clean JSONL export.
9. Explicit non-goals.
10. Evidence: tests, corpus, fuzz, crash injection, soak, and TLA+ models.

## What datawal does not try to be

datawal is not:

- a database;
- a SQL engine;
- a query planner;
- a queue;
- a cache;
- a distributed log;
- a multi-process writer;
- a generic WAL engine;
- a replacement for SQLite, RocksDB, redb, or fjall.

## Differentiators

datawal's differentiators are:

1. **Recoverable JSONL shape.** Familiar append/replay workflow, but framed,
   CRC-checked, and recoverable.
2. **Bytes-first core.** Codecs and semantic value types live above the core.
3. **Clean export.** Live state can be exported as JSONL.
4. **Explicit failure model.** Durability, recovery, locking, and compaction
   semantics are documented.
5. **Evidence stack.** Wire-format corpus, TLA+, fuzz targets, crash injection,
   ENOSPC tests, property tests, soak driver, and benchmarks.
6. **Small surface area.** The project should stay a local record store, not
   grow into a database.
