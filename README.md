# datawal

[![Crates.io](https://img.shields.io/crates/v/datawal.svg)](https://crates.io/crates/datawal)
[![Docs.rs](https://docs.rs/datawal/badge.svg)](https://docs.rs/datawal)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

datawal is a local record store: a framed append-only `RecordLog` plus an
optional last-write-wins `DataWal` KV projection.

> `0.1.4` is the first non-alpha datawal release. It is suitable for
> local recoverable logs where JSONL would otherwise be used, with the
> documented limits in [`docs/canon.md`](docs/canon.md). `0.1.x` may
> still introduce small breaking changes before `0.2`; the on-disk
> wire format (`WIRE_VERSION = 1`) is frozen and locked by corpus
> fixtures.

**MSRV:** Rust 1.75.0

## What datawal is

- **`RecordLog`** — the canonical append-only list. Every write becomes a
  framed, CRC-checked record on disk. Recovery is defined as the longest
  valid prefix: a truncated tail is reported but not fatal; a mid-stream
  CRC error in a closed segment is a hard error.
- **`DataWal`** — a KV projection derived from the log. Keys are
  bytes; values are bytes. Last-write-wins. Delete leaves a tombstone.
  Reopen rebuilds the keydir from scratch by replaying the log.
- **Bytes-first.** The Rust core does not parse JSON, MessagePack, or
  any semantic encoding. It stores and returns opaque byte slices.
- **Clean export.** `export_jsonl` writes the live key/value state to a
  JSONL file (base64-encoded keys and values) via an atomic write.
- **FS plumbing in a sibling crate.** Atomic POSIX primitives
  (`write_atomic`, `write_once`, `write_append_fsync`, `rename_atomic`,
  `fsync_dir`) live in
  [`safeatomic-rs`](https://github.com/deepcausa/safeatomic-rs)
  ([crates.io](https://crates.io/crates/safeatomic-rs)).

## When to use

- You are manually appending JSONL and a crash truncating the file mid-record
  would be a problem.
- You need a tiny local key/value store with last-write-wins semantics and
  no external process or network.
- You need audit logs, checkpoint logs, or event logs for experiments,
  agents, crawlers, CLIs, or local daemons.
- You want a file-based log format that is documented down to the byte level,
  with frozen wire-format fixtures and TLA+ invariants for the recovery protocol.
- You want to be able to open the log, scan it, and understand exactly what
  is on disk — no opaque internal formats.

## When not to use

- SQL, joins, secondary indexes, or range queries.
- A cache with TTL or eviction.
- A FIFO queue.
- Multi-writer or concurrent writers.
- Distributed or network-attached storage.
- Large object / blob / content-addressed storage.
- DataFrame analytics (use Polars, DuckDB, etc.).
- A production database (use SQLite, LMDB, RocksDB, etc.).

## Current status

**datawal is currently an alpha crate:** functional and model-checked at
the protocol level, but not production-ready. See
[`docs/roadmap.md`](docs/roadmap.md) for the exact release scope.

It is published on crates.io as an alpha release; the on-disk wire format
is frozen by a corpus fixture set, but workload coverage is still only at
the unit and integration level.

What is in:

- 58 tests green (`cargo test --workspace`).
- 3 TLA+ models model-checked with TLC 2.19.
- Wire-format corpus: 6 binary fixture directories, 11 corpus tests.
- 4 runnable examples.
- Real **CRC-32C** (Castagnoli, `0x1EDC6F41`) per record, pinned by a
  known-vector test.
- **fs2 fd-based advisory lock**: held by a file descriptor, not by the
  existence of the sentinel file. Released on `Drop` / process exit. A stale
  `.lock` from a crashed previous process is not a problem.
- **Durability boundary** is explicit: `append` produces a framed,
  recoverable record but does *not* guarantee durability across a crash.
  Call `RecordLog::fsync()` to durabilise (`sync_all` on the active segment
  plus `fsync_dir` on the containing directory).
- `compact_to(out_dir)` only — no in-place `compact()`.

What is not in:

- Python / PyO3 bindings.
- Content-addressed storage / blob / dedup / CAS.
- Compression.
- Server or multi-user access.
- Multi-writer.
- Query / secondary indexes.
- In-place compaction.
- Reader API / concurrent reads.

## Limits

`datawal` is bytes-first, but not unbounded. Neither the `RecordLog` nor
the `DataWal` projection interprets the bytes — no JSON, no UTF-8, no
MessagePack parsing in the core. Current alpha limits:

| Limit              | Value / status         | Notes                                                                                                |
| ------------------ | ---------------------: | ---------------------------------------------------------------------------------------------------- |
| Max key size       | 64 KiB                 | Per record. Larger keys are rejected.                                                                |
| Max payload size   | 64 MiB                 | Per record. For larger values, use an external blob store and store references.                      |
| Writers            | Single writer          | Enforced with an advisory fd lock. No multi-writer semantics.                                        |
| Readers            | No reader API yet      | `scan()` is the only read path; tracked in [#5](https://github.com/deepcausa/datawal/issues/5).      |
| `scan()` memory    | `Vec<Record>`          | Not streaming yet; large logs need [#3](https://github.com/deepcausa/datawal/issues/3).              |
| `DataWal` keydir   | values in memory       | Live values are held in the in-memory keydir; offset-based variant is [#4](https://github.com/deepcausa/datawal/issues/4). |
| Durability         | explicit `fsync()`     | `append()` is recoverable; `append() + fsync()` is durable under documented assumptions.             |
| Compaction         | `compact_to` only      | Snapshot-style rebuild into a target directory. No in-place `compact()`.                             |
| CAS / blob         | not included           | Planned as a separate crate / layer; tracked in [#7](https://github.com/deepcausa/datawal/issues/7). |
| Compression        | not included           | `flags` must be zero in v0.1.                                                                        |
| Query              | not included           | No SQL, indexes, joins, range scans, or planner. See [#13](https://github.com/deepcausa/datawal/issues/13). |
| Production status  | alpha                  | Functional, tested, model-checked at the protocol level; not production-ready.                       |

What is **not** limited inside those bounds: the byte composition of
keys and payloads. Any sequence is legal, including all-zero, all-`0xFF`,
embedded null bytes, and arbitrary binary blobs. The
[`roundtrip` fuzz target](fuzz/README.md) exercises this empirically.

## Quick start

```rust
use datawal::{RecordLog, DataWal};
use std::path::Path;

// --- RecordLog ---
let path = Path::new("/tmp/my-log");
let mut log = RecordLog::open(path)?;
log.append(b"one")?;
log.append(b"two")?;
log.fsync()?;                          // durability boundary

let records = log.scan()?;
assert_eq!(records[0].payload, b"one");
assert_eq!(records[1].payload, b"two");

// --- DataWal ---
let path = Path::new("/tmp/my-kv");
let mut db = DataWal::open(path)?;
db.put(b"a", b"1")?;
db.put(b"a", b"2")?;                  // last-write-wins
assert_eq!(db.get(b"a")?, Some(b"2".to_vec()));

db.delete(b"b")?;
assert_eq!(db.get(b"b")?, None);

db.compact_to(Path::new("/tmp/my-kv-compacted"))?;
db.export_jsonl(Path::new("/tmp/my-kv.jsonl"))?;
# Ok::<(), anyhow::Error>(())
```

## Evidence stack

The protocol has been validated at multiple levels:

| Layer               | Evidence                                              |
| ------------------- | ----------------------------------------------------- |
| Specification       | `docs/canon.md` — 14 binding clauses; byte layout     |
| Code                | `crates/datawal-core/src/` — ~1900 LOC Rust           |
| Unit + integration  | 58 tests across `tests/*.rs` and embedded `#[test]`s  |
| Wire-format corpus  | 6 binary fixture dirs, 11 corpus tests                |
| Formal models       | 3 TLA+ models, model-checked with TLC 2.19            |
| Runnable examples   | 4 examples under `crates/datawal-core/examples/`      |

**Formal models wording:** model-checked under documented assumptions.
Not "formally verified". Models do not check the Rust implementation.
See `formal/README.md` for invariants and how to run TLC.

## Layout

```
datawal/
├── Cargo.toml             # workspace
├── crates/
│   └── datawal-core/
│       ├── src/
│       │   ├── lib.rs
│       │   ├── format.rs           # wire format, encode/decode, CRC, limits
│       │   ├── segment.rs          # segment naming and listing
│       │   ├── lock.rs             # fs2 fd-based advisory lock
│       │   ├── record_log.rs       # RecordLog
│       │   └── datawal.rs          # DataWal KV
│       ├── examples/
│       │   ├── record_log_demo.rs
│       │   ├── datawal_kv_demo.rs
│       │   ├── tail_recovery_demo.rs
│       │   └── gen_corpus.rs       # regenerate tests/corpus/* (run-on-demand)
│       └── tests/
│           ├── record_log.rs       # 14 cases
│           ├── datawal.rs          # 9 cases
│           ├── integration.rs      # 3 cases
│           ├── corpus_fixtures.rs  # 11 cases over the frozen corpus
│           └── corpus/             # binary fixtures, one subdir per fixture
├── formal/                         # TLA+ models (checked with TLC)
│   ├── RecordLog.tla
│   ├── KeydirProjection.tla
│   ├── Compaction.tla
│   ├── *.cfg
│   └── reports/                    # most recent TLC output per model
├── docs/                           # canon, technical decisions, roadmap, related work
└── dev/                            # gitignored; internal notes only
```

`safeatomic-rs` is published separately on crates.io and consumed via
`Cargo.toml`; it is not part of this repository's source tree. See
[`github.com/deepcausa/safeatomic-rs`](https://github.com/deepcausa/safeatomic-rs).

## Running

```sh
cargo fmt --all
cargo check --workspace
cargo test --workspace
cargo run -p datawal --example record_log_demo
cargo run -p datawal --example datawal_kv_demo
cargo run -p datawal --example tail_recovery_demo
cargo doc --workspace --no-deps
```

## Benchmarks

datawal ships [Criterion](https://github.com/bheisler/criterion.rs) benches
under `crates/datawal-core/benches/`:

- `record_log` — `RecordLog::append` (no fsync and fsync-per-append) across
  payload sizes, plus `RecordLog::scan` throughput.
- `datawal_kv` — `DataWal::put / get / delete` as a function of keydir size,
  plus `DataWal::open` (keydir rebuild) cost.
- `compaction` — `DataWal::compact_to` and `DataWal::export_jsonl` against
  delete-heavy and overwrite-heavy logs at varying live-key ratios.
- `recovery` — `RecordLog::open` + `recovery_report` cost vs. log size,
  segment count, and partially-truncated tail length.

Run them all:

```sh
cargo bench --workspace
```

Or one bench at a time:

```sh
cargo bench -p datawal --bench record_log
cargo bench -p datawal --bench datawal_kv
cargo bench -p datawal --bench compaction
cargo bench -p datawal --bench recovery
```

Numbers from any single run are not committed as truth: results depend on
machine, kernel, filesystem, and storage, and small numbers compared across
machines mislead more than they help. CI only verifies that the benches
*compile* (`cargo bench --workspace --no-run`); it does not run them.

For methodology, how to read Criterion output, gotchas (especially around
fsync), and what is *not* measured, see [`docs/benchmarks.md`](docs/benchmarks.md).

For an order-of-magnitude reference run with generic stack description, see
[`docs/benchmarks/v0.1.0-alpha-reference.md`](docs/benchmarks/v0.1.0-alpha-reference.md).

**fsync benches need a real local disk.** On Linux, `/tmp` is often tmpfs and
overlayfs / NFS likewise lie about durability — fsync numbers from those
filesystems are not meaningful. Point the benches at a real SSD/NVMe local
filesystem via:

```sh
DATAWAL_BENCH_DIR=/mnt/nvme/datawal-bench cargo bench -p datawal --bench record_log
```

When `DATAWAL_BENCH_DIR` is unset, benches fall back to the system tempdir.

## Fuzzing

A small [`cargo-fuzz`](https://github.com/rust-fuzz/cargo-fuzz) crate
lives at [`fuzz/`](fuzz/README.md) (outside the workspace, nightly-only).
Three targets cover the wire-format decoder, segment-level recovery,
and the `DataWal` put/get roundtrip:

```sh
cargo install cargo-fuzz
just fuzz-build              # compile every target on nightly
just fuzz-run-decode         # primary decoder target, 30s
just fuzz-run-scan           # RecordLog::open smoke, 30s
just fuzz-run-roundtrip      # DataWal put/get bytes-in == bytes-out, 30s
```

CI verifies the targets *compile* on nightly; it does not run them.

## Formal models

Three small TLA+ models live under `formal/` and are checked with
[TLC](https://github.com/tlaplus/tlaplus/) 2.19+:

- `RecordLog.tla` — append / fsync / crash; durable is a monotonic prefix.
- `KeydirProjection.tla` — last-write-wins keydir from a put/del log.
- `Compaction.tla` — `compact_to` preserves the live projection.

**model-checked under documented assumptions** — not "formally verified",
does not check the Rust implementation. See `formal/README.md`.

## Wire-format corpus

`crates/datawal-core/tests/corpus/` contains binary fixtures that freeze the
v0.1 on-disk format. Regenerate only when the format changes intentionally:

```sh
cargo run -p datawal --example gen_corpus
```

See `crates/datawal-core/tests/corpus/README.md`.

## Related projects

- [`safeatomic-rs`](https://github.com/deepcausa/safeatomic-rs) — Rust
  filesystem primitives used by datawal for atomic writes and directory
  fsyncs.
- [`safeatomic`](https://github.com/deepcausa/safeatomic) — Python package
  for whole-file persistence with explicit guarantees and runtime
  diagnostics.

`safeatomic` is for replacing whole files safely.
`datawal` is for appending recoverable records and deriving local state
from them.

## See also

- `docs/canon.md` — binding decisions and the byte-layout of a record.
- `docs/technical-decisions.md` — TD-NNN entries documenting choices.
- `docs/roadmap.md` — current alpha scope, what is frozen, and the tracked roadmap issues.
- `formal/README.md` — the TLA+ models and how to run TLC.

## License

Dual-licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT License ([LICENSE-MIT](LICENSE-MIT) or
  <http://opensource.org/licenses/MIT>)

at your option.

SPDX-License-Identifier: `MIT OR Apache-2.0`

### Contribution

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.
