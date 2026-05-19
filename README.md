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

## v0.1-pre status

`v0.1-pre` is the first release with **real** I/O. It implements:

- `RecordLog::{open, append, append_record, scan, recovery_report, fsync,
  rotate, close, dir}`.
- `DataWal::{open, put, get, delete, contains_key, len, is_empty, keys,
  items, fsync, compact_to, export_jsonl}`.
- Wire format: `b"DWAL"` magic, version `u16 LE = 1`, record type, txid,
  key, payload, CRC32 of the framed record (see `docs/canon.md` for the
  byte layout).
- Tail-truncation recovery on the last (active) segment.
- Hard errors on: CRC mismatch in a *closed* (non-tail) segment, unknown
  magic, unknown wire version, unknown record type, reserved flags set.
- Advisory single-writer lock via a best-effort `.lock` file.
- Local POSIX / Linux filesystems.
- No compression. No CAS. No PyO3. No server. No multi-writer. No query.

The CRC is implemented with `crc32fast` (CRC-32 IEEE / Ethernet), not the
true CRC-32C / Castagnoli polynomial. The on-disk field is still named
`crc32c` so a future wire-version bump can switch the implementation
without renaming the format. This is v0.1-pre baggage and is documented
in `docs/technical-decisions.md`.

## Layout

```
datawal/
├── Cargo.toml             # workspace
├── crates/
│   └── datawal-core/
│       ├── src/
│       │   ├── lib.rs
│       │   ├── format.rs       # wire format, encode/decode, CRC, limits
│       │   ├── segment.rs      # segment naming and listing
│       │   ├── lock.rs         # advisory .lock file
│       │   ├── record_log.rs   # RecordLog
│       │   └── datawal.rs      # DataWal KV
│       ├── examples/
│       │   ├── record_log_demo.rs
│       │   └── datawal_kv_demo.rs
│       └── tests/
│           ├── record_log.rs   # 12 cases
│           ├── datawal.rs      # 9 cases
│           └── integration.rs  # 3 cases
├── formal/                # TLA+ specs (planned)
├── docs/                  # canon, technical decisions, related work
└── tests/                 # (reserved for workspace-level tests)
```

`safeatomic-rs` lives in the sibling crate at `../safeatomic-rs/` and is
not part of this workspace.

## Running

```
cargo fmt --all
cargo check --workspace
cargo test --workspace
cargo run -p datawal-core --example record_log_demo
cargo run -p datawal-core --example datawal_kv_demo
```

See `docs/canon.md` for the binding decisions, `docs/roadmap.md` for the
in-scope/out-of-scope breakdown of v0.1-pre and what is plausible for
v0.2, and `formal/README.md` for the planned TLA+ models.

## License

Apache-2.0 (planned).
