# Changelog

All notable changes to the `datawal` crate are documented in this
file. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
and the crate uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

`datawal` is in the `0.y.z` line. While the wire format is frozen at
`WIRE_VERSION = 1`, public Rust API additions and small breaking
changes may still occur within `0.1.x` until `0.2.0`.

## [0.1.4] — 2026-05-21

`0.1.4` is the first non-alpha release of `datawal`. It is suitable
for local recoverable logs where JSONL would otherwise be used, with
the documented limits in [`docs/canon.md`](docs/canon.md) and the
release-quality benchmarks in
[`docs/benchmarks/v0.1.4-reference.md`](docs/benchmarks/v0.1.4-reference.md).

This release consolidates an 8-PR quality kit accumulated since
`0.1.0-alpha.1`. The version jump from `0.1.0-alpha.1` to `0.1.4`
(skipping `0.1.1`, `0.1.2`, `0.1.3`) is intentional: each PR in the
kit was a quality increment, not a hotfix, and the `alpha` qualifier
was dropped now that the crate has been validated in active use.

### Added

- **Record-level lazy iterator on `RecordLog`** (`RecordLog::scan_iter`,
  PR #23): walks every segment as a record iterator backed by a
  segment-level buffered reader. Not zero-copy. Does not materialise
  the whole log in memory.
- **Lockless concurrent reader** (`RecordLogReader`, PR #27): opens a
  store without acquiring the cooperative single-writer lock for
  read-only inspection. Reads a snapshot at segment-id level. Formal
  model `formal/ReadWhileWrite.tla` (6 invariants) covers
  reader-while-writer correctness.
- **Keydir stores offsets, not values** (PR #28): `DataWal`'s
  in-memory keydir is now `HashMap<Vec<u8>, RecordRef>`. Values are
  loaded on demand via a private LRU fd-pool (default 16 fds, Unix
  `pread`). CRC32C is re-verified on every `get`. Memory footprint
  drops from `O(sum of value sizes)` to `O(sum of key sizes + 32
  bytes/key)`. New helper `DataWal::ref_of`.
- **`RecordLog::is_poisoned()`** (PR #25): exposes whether a prior
  write / fsync / rotate failure has poisoned the writer. A poisoned
  writer fails subsequent writes with a stable, scriptable message:
  `"datawal: writer poisoned: <reason>; drop handle and reopen"`.
- **`datawal` CLI binary** (PRs #24 and #29, this kit's closing PR):
  inspector + source-untouched maintenance commands shipped in a
  new sibling crate `datawal-cli` (binary name: `datawal`). Eight
  subcommands in two groups:
  - Inspection (read-only): `scan`, `get`, `report`, `verify`, `dump`,
    `check`.
  - Source-untouched mutations: `export`, `compact`. Both write only
    to a caller-supplied output path; the source store on disk is
    never modified.

  JSON output schema is `datawal.cli.v1` (stable). Binary keys and
  values are base64-encoded in JSON form; printable-ASCII bytes are
  rendered literally in human form, with explicit `b64:` / `hex:`
  prefixes for binary bytes. CLI does **not** offer `put` / `delete` /
  `rotate` in 0.1.x. Reserved CLI names that will never be used:
  `query`, `select`, `where`, `index`, `server`.

### Changed

- **Breaking (`DataWal`):** `get`, `items`, `compact_to`, and
  `export_jsonl` now take `&mut self` (PR #28). `items` now returns
  `Result<Vec<(Vec<u8>, Vec<u8>)>>` (was `Vec<(Vec<u8>, Vec<u8>)>`)
  because lazy value loading can surface I/O / CRC errors.
- **Public surface added (additive, non-breaking):**
  `RecordLog::scan_iter`, `RecordIter`, `RecordLog::is_poisoned`,
  `RecordLogReader`, `DataWal::ref_of`, and the four format constants
  `RecordType`, `MAX_KEY_LEN`, `MAX_PAYLOAD_LEN`, `WIRE_VERSION` were
  promoted to canonical re-exports.

### Tests

- Property-based tests for `RecordLog` recovery + multi-process lock
  contention + tmpfs disk-full + poison-writer property (PR #25;
  `proptest` pinned to 1.8.0 in `Cargo.lock` for MSRV 1.75).
- Long-running soak driver (`examples/soak.rs`, PR #26): fd-leak
  check, RSS stability over hours, final-state oracle equality
  against an in-memory replica.
- Fuzz targets for the decoder and the encode/decode roundtrip
  (under `fuzz/`).
- Crash-injection tests for `append`, `rotate`, `compact_to`,
  `export_jsonl`.
- Criterion benches under `crates/datawal-core/benches/` covering
  append, recovery, compaction, and KV `put`/`get`.

### Formal models

Four TLA+ models pinned at TLA+ tools 1.8.0 and gated in CI on every
push:
- `formal/RecordLog.tla` — valid-prefix recovery.
- `formal/KeydirProjection.tla` — KV projection correctness.
- `formal/Compaction.tla` — snapshot-style rebuild.
- `formal/ReadWhileWrite.tla` — lockless reader correctness (PR #27).

The CI job step asserts on `Model checking completed. No error has
been found.` per model and uploads the TLC logs as artifacts.

### Wire format

`WIRE_VERSION = 1`, frozen. Six on-disk corpus fixtures under
`crates/datawal-core/tests/corpus/` lock the byte layout against
drift; the `corpus` CI job regenerates them on every run and compares
SHA-256s.

### MSRV

`rust-version = 1.75.0`. CI matrix exercises `stable` and `1.75.0`.
`getrandom` is pinned to 0.3.4, `proptest` to 1.8.0, `assert_cmd` to
2.1.2, and `clap` to 4.5.20 in `Cargo.lock` to stay compatible with
Cargo 1.75 (later releases of those crates declare `edition = "2024"`
or `rust-version >= 1.85`, both unparseable on the MSRV row).

## [0.1.0-alpha.1] — 2026-05-19

- Include `README.md` in the published crate (corrects the missing
  README in `0.1.0-alpha`).

## [0.1.0-alpha] — 2026-05-19

- Initial public alpha. Append-only framed record log with valid-prefix
  recovery, CRC32C, cooperative single-writer lock, bytes-based
  last-write-wins KV projection (`DataWal`), snapshot-style
  `compact_to`, JSONL export, no in-place segment mutation.
- Wire format frozen at `WIRE_VERSION = 1`.
- Public re-exports: `RecordLog`, `Record`, `RecordRef`,
  `RecoveryReport`, `DataWal`, `CompactionStats`.

[0.1.4]: https://github.com/deepcausa/datawal/releases/tag/v0.1.4
[0.1.0-alpha.1]: https://github.com/deepcausa/datawal/releases/tag/v0.1.0-alpha.1
[0.1.0-alpha]: https://github.com/deepcausa/datawal/releases/tag/v0.1.0-alpha
