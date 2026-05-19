# datawal

datawal is a local record store: append-only framed records, valid-prefix
recovery, optional KV projection, tombstone deletes, manual compaction, and
clean export.

## What datawal is

- A Rust core (`datawal-core`) that operates on **bytes**.
- An append-only **RecordLog** with CRC-framed records, segments,
  monotonic txids, and recovery defined as the longest valid prefix.
- An optional **DataWal** KV projection over the same log: last-write-wins,
  tombstone deletes, in-memory keydir, manual `compact_to`.
- A clean **export** path to JSONL (base64-encoded keys and values).
- A clean separation from filesystem plumbing: atomic POSIX primitives
  (`write_atomic`, `write_once`, `write_append_fsync`, `rename_atomic`,
  `fsync_dir`) live in the sibling crate
  [`safeatomic-rs`](../safeatomic-rs/) and are consumed by `datawal-core`
  only where needed (atomic export, dir fsync).

## What datawal is **not**

- Not a SQL database.
- Not a dataframe / query engine.
- Not a cache.
- Not a multi-writer concurrent database.
- Not a network-attached store.
- Not a distributed log.

## v0.1.0-alpha status

`v0.1.0-alpha` is the first release with **real** I/O and the first one
hardened past the initial `v0.1-pre` walking skeleton. It implements:

- `RecordLog::{open, append, append_record, scan, recovery_report, fsync,
  rotate, close, dir}`.
- `DataWal::{open, put, get, delete, contains_key, len, is_empty, keys,
  items, fsync, compact_to, export_jsonl}`.
- Wire format: `b"DWAL"` magic, `WIRE_VERSION = 1`, record type, txid,
  key, payload, **CRC-32C** (Castagnoli, polynomial `0x1EDC6F41`) of the
  framed record. See `docs/canon.md` for the byte layout. A known-vector
  test pins the algorithm.
- **Durability boundary.** `append` produces a framed, recoverable record
  but does **not** by itself guarantee durability after a crash. Call
  `fsync` to durabilise: `sync_all` on the active segment plus
  `fsync_dir` on the containing directory.
- Tail-truncation recovery on the last (active) segment.
- Hard errors on: CRC mismatch in a *closed* (non-tail) segment, unknown
  magic, unknown wire version, unknown record type, reserved flags set.
- **Single-writer per directory** via an OS-level advisory lock
  (`fs2::FileExt::try_lock_exclusive` on `<dir>/.lock`, POSIX `flock(2)`
  / Windows `LockFileEx`). The lock is held by a file descriptor, not by
  the existence of the sentinel file. A second `RecordLog::open` on the
  same directory fails fast; the lock is released on `Drop` or on
  process exit.
- Local POSIX / Linux filesystems.
- No compression. No CAS. No PyO3. No server. No multi-writer. No query.

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
└── tests/                          # (reserved for workspace-level tests)
```

`safeatomic-rs` lives in the sibling crate at `../safeatomic-rs/` and is
not part of this workspace.

## Running

```sh
cargo fmt --all
cargo check --workspace
cargo test --workspace
cargo run -p datawal-core --example record_log_demo
cargo run -p datawal-core --example datawal_kv_demo
cargo run -p datawal-core --example tail_recovery_demo
cargo doc --workspace --no-deps
```

## Formal models

Three small TLA+ models live under `formal/` and are checked with
[TLC](https://github.com/tlaplus/tlaplus/) 2.19+:

- `RecordLog.tla` — append / fsync / crash; durable is a monotonic prefix.
- `KeydirProjection.tla` — last-write-wins keydir from a put/del log.
- `Compaction.tla` — `compact_to` preserves the live projection.

This is **model-checked under documented assumptions**, not "formally
verified", and does not check the Rust implementation. See
`formal/README.md`.

## Wire-format corpus

`crates/datawal-core/tests/corpus/` contains hand-checked binary
fixtures that freeze the v0.1 on-disk format. Regenerate only when the
format changes intentionally:

```sh
cargo run -p datawal-core --example gen_corpus
```

See `crates/datawal-core/tests/corpus/README.md`.

## See also

- `docs/canon.md` — binding decisions and the byte-layout of a record.
- `docs/technical-decisions.md` — TD-NNN entries documenting choices.
- `docs/roadmap.md` — what is in v0.1-alpha vs out-of-scope vs plausible
  for v0.2.
- `formal/README.md` — the TLA+ models and how to run TLC.

## License

Apache-2.0 (planned).
