# datawal — technical decisions

A log of choices, not yet a design document. Each entry has a status:
`accepted`, `proposed`, or `rejected`.

## TD-001 — Workspace layout: `crates/datawal-*`
- **Status:** accepted (revised in TD-011)
- One crate at v0.0.1: `datawal-core`.
- `datawal-cas` and `datawal-py` are reserved names; not created yet.
- Filesystem primitives moved out into the sibling crate `safeatomic-rs`
  (see TD-011). Original v0.0.1 had a second crate `datawal-io`; it has
  been removed.

## TD-002 — Filesystem primitives extracted to `safeatomic-rs`
- **Status:** historical (superseded by TD-011)
- The six atomic FS primitives originated from prior Rust work and were
  copied verbatim into this workspace. They first lived in
  `crates/datawal-io/src/lib.rs` and then moved to the sibling crate
  `apps/safeatomic-rs/src/lib.rs`.
- Function names and bodies are preserved 1:1 in `safeatomic-rs`. Exported
  symbols:
  - `write_atomic`
  - `write_once`
  - `write_append_fsync`
  - `rename_atomic`
  - `fsync_dir`
  - `write_once_with_parents`

## TD-003 — `anyhow` is allowed
- **Status:** accepted
- The extracted module uses `anyhow::Result`. Removing it would require
  diverging from the canonical source. `anyhow` is not a domain dependency.
- A later cleanup may switch the public surface to a typed error
  (`std::io::Error` + a thin `IoError`). Tracked as proposed. This applies
  to `safeatomic-rs` (which now hosts the primitives) as well as to
  `datawal-core`.

## TD-004 — No `path =` dependencies pointing outside `datawal/` or its allowed siblings
- **Status:** accepted (revised in TD-011)
- The datawal workspace must remain self-contained. Reuse of external code
  happens via `/bin/cp`, not via Cargo path dependencies — with the single
  exception called out in TD-011: when `datawal-core` starts performing real
  I/O it will take a `path = "../../safeatomic-rs"` dependency on the
  `safeatomic-rs` sibling crate. No other path dependencies are permitted.

## TD-005 — `datawal-core` v0.0.1 was intentionally inert
- **Status:** historical (superseded by TD-012)
- Public types reserved at v0.0.1: `RecordLog`, `RecordRef`, `Record`,
  `DataWal`. All non-trivial methods returned `unimplemented!`.
- Rationale: lock the API names early; defer protocol decisions until
  the format is understood. v0.1-pre (TD-012) implements them for real.

## TD-006 — Length-prefixed framing with CRC-32C
- **Status:** accepted
- Layout (28-byte header + body + 4-byte CRC), see `docs/canon.md` §5.
- CRC is CRC-32C (Castagnoli), polynomial `0x1EDC6F41`, via the `crc32c`
  crate. Pinned by a known-vector test in `format.rs` against RFC 3720
  reference values, including a sentinel `assert_ne!` against the
  CRC-32 IEEE result for `"123456789"` to detect any silent regression.
- Earlier history: v0.1-pre originally shipped with `crc32fast` (CRC-32
  IEEE / Ethernet) under the name `crc32c`, as a deliberate misnomer to
  defer the dependency choice. That drift was corrected before any
  external consumer existed, so the on-disk format never had to bump
  `WIRE_VERSION` from `1`.
- Alternatives reconsidered: xxhash64 (8 bytes; no widespread HW accel
  on the relevant platforms), blake3 (overkill for frame integrity).

## TD-007 — Single-writer advisory lock via `fs2`
- **Status:** accepted
- `RecordLog::open` acquires an exclusive OS-level advisory lock on
  `{dir}/.lock` using `fs2::FileExt::try_lock_exclusive` (POSIX
  `flock(2)` on Unix, `LockFileEx` on Windows). The lock is held by
  the file descriptor stored inside the `RecordLog`; `Drop` and process
  exit release it. The sentinel `.lock` file persists between runs and
  is **not** the lock — only the kernel-held flock is.
- Acquisition is non-blocking: `try_lock_exclusive` fails fast if another
  holder is alive. A stale `.lock` file from a previous crashed process
  is not a problem; the kernel releases the flock automatically when
  the holding process exits.
- This is *advisory*: only cooperating callers that all go through
  `RecordLog::open` are protected. Mandatory locking and multi-writer
  semantics remain out of scope.
- Tests: `acquire_then_release_allows_reacquire`,
  `second_acquire_fails_while_first_held`, `drop_releases_lock`,
  `stale_lock_file_is_not_a_problem` in `src/lock.rs`; plus
  `second_open_fails_while_first_open` and the reopen flow in
  `tests/record_log.rs`.

## TD-007a — Durability boundary
- **Status:** accepted
- `RecordLog::append` / `append_record` write a framed, CRC-protected
  record via `write_all` to the active segment's underlying file. The
  record is *recoverable* immediately (a subsequent `scan()` will
  return it modulo OS buffering), but is **not durable** across a host
  crash or power loss until `RecordLog::fsync` returns successfully.
- `RecordLog::fsync` calls `File::sync_all` on the active segment and
  `safeatomic_rs::fsync_dir` on the containing directory, so segment
  creations / rotations are also durable.
- v0.1-pre does not offer fdatasync, group commit, configurable fsync
  policy, or per-batch atomic commit. Callers that need per-record
  durability must pair every `append` with an `fsync`.
- Tests: `append_fsync_reopen` in `tests/record_log.rs`; documented
  contract on `append`, `append_record` and `fsync` doc comments and
  in `docs/canon.md` §9a.

## TD-008 — No CAS in v0.1
- **Status:** accepted
- Content-addressed blob storage stays in its upstream home for now.
- A future `datawal-cas` may be added but will not be a dependency of core.

## TD-009 — TLA+ precedes frozen wire format
- **Status:** accepted
- Models listed in `formal/README.md` must check before v0.1.0 ships.

## TD-010 — Existing projects are not modified
- **Status:** accepted
- This phase is "create the new repo from extractions". Pilots into the
  upstream consumers of these primitives are scheduled for later phases
  and are tracked separately, outside this repository.

## TD-011 — Atomic FS primitives extracted to `safeatomic-rs`
- **Status:** accepted
- The crate `datawal-io` introduced at bootstrap has been removed and its
  contents extracted to a separate, single-crate repository
  `apps/safeatomic-rs/` (crate name `safeatomic-rs`, edition 2021).
- Rationale: the six primitives are generic POSIX filesystem operations and
  are useful outside datawal (a future `datawal-cas`, other consumers,
  external users). Keeping them inside the datawal workspace would
  recreate, in Rust, the same coupling that the Python `safeatomic` package
  was carved out to avoid.
- `safeatomic-rs` is the Rust sibling of the Python `safeatomic` package.
  It is not a binding, not an FFI wrapper, and not an API mirror — the
  two crates share intent and surface only.
- `datawal-core` does **not** depend on `safeatomic-rs` at v0.0.1, because
  no I/O is yet performed. The dependency will be added when `RecordLog`
  starts writing manifests/segments. Path: `safeatomic-rs = { path =
  "../../../safeatomic-rs" }` (relative from `crates/datawal-core/`).
- `write_append_fsync` in `safeatomic-rs` is documented as a **primitive**,
  not a framed log: record framing, CRC, segmentation, and recovery remain
  datawal's responsibility.

## TD-012 — `datawal-core` v0.1-pre implements the protocol
- **Status:** accepted
- Modules added: `format` (wire format, encode/decode, CRC, limits),
  `segment` (segment naming and listing), `lock` (best-effort `.lock`),
  `record_log` (RecordLog), `datawal` (DataWal KV).
- Dependencies added: `crc32fast`, `serde`, `serde_json`, `base64`,
  `safeatomic-rs` (path), and `tempfile` (dev only).
- `RecordLog` is `&mut self` for writes and rescans on `scan()`; payloads
  are returned in full as `Vec<u8>` (no zero-copy in v0.1-pre).
- `DataWal` keydir is `HashMap<Vec<u8>, Vec<u8>>` — full values are kept
  in memory. This is acceptable for the v0.1-pre target workloads
  (append-only JSONL-shaped audit/checkpoint logs) but is the obvious
  thing to optimise next.
- `compact_to(out_dir)` is the only supported compaction. In-place
  `compact()` is deliberately not implemented: there is no safe atomic
  swap in v0.1-pre without more lock machinery.
- `export_jsonl` writes one JSON line per live key, sorted by key, with
  base64 of both key and value. It uses `safeatomic_rs::write_atomic`
  for the final write.
- Tests: 18 unit tests in `src/*` + 12 RecordLog integration tests + 9
  DataWal integration tests + 3 cross-cutting integration tests
  (`cargo test --workspace` → 42 passed).
- Out of scope (unchanged): CAS, PyO3, compression, async, server,
  multi-writer, query.
