# datawal canon

Binding decisions. These hold until explicitly retracted in this file.

## Scope

1. **RecordLog is the canonical append-only list.**
   All persisted state of a datawal directory is reconstructible from its
   segment files plus the MANIFEST. The MANIFEST is a hint; segments are the
   source of truth.

2. **DataWal is a projection.**
   The KV view is a *deterministic* fold over the RecordLog with
   last-write-wins semantics and tombstone deletion. Compaction rewrites the
   log into a smaller log that yields the same projection.

3. **The Rust core operates on bytes.**
   `RecordLog::append` takes `&[u8]`. `Record::bytes` is `Vec<u8>`.
   Serialization formats (JSON, MessagePack, ...) and language-specific value
   types live in *codec* layers above the core.

4. **Python codecs are out of scope of the core.**
   The future `datawal-py` PyO3 binding will expose the core to Python; any
   pickling, dataclass conversion, or schema work is a separate concern.

## Wire format

5. **Framing: 24-byte header + key + payload + 4-byte CRC.**
   Layout (little-endian):
   ```
   magic        4   b"DWAL"
   version      u16  = 1
   record_type  u8   Raw=0 | Put=1 | Delete=2
   flags        u8   = 0  (reserved; non-zero on disk is a hard error)
   txid         u64  monotonic from 1
   key_len      u32  ≤ MAX_KEY_LEN     (64 KiB)
   payload_len  u32  ≤ MAX_PAYLOAD_LEN (64 MiB)
   key          key_len bytes
   payload      payload_len bytes
   crc          u32  CRC of header_without_crc || key || payload
   ```
   Total bytes on disk: `28 + key_len + payload_len`.

6. **CRC is CRC-32C (Castagnoli).**
   Polynomial `0x1EDC6F41`, computed over `header_without_crc || key ||
   payload` via the `crc32c` crate. Pinned by a known-vector test in
   `format.rs` against the RFC 3720 reference values, so a silent
   regression to CRC-32 IEEE (or any other polynomial) fails the suite.
   The wire field name `crc32c` matches the algorithm.

7. **No compression in v0.1.**
   Records are stored raw. `zstd` is planned as an opt-in cargo feature
   (`zstd = ["dep:zstd"]`) for v0.2+. Compression, when added, applies
   per-record and is signaled in a per-record header bit (currently
   reserved in `flags`, which must be zero in v0.1-pre).

8. **Recovery = longest valid prefix.**
   On `open`, segments are scanned in ascending id order. The active
   (last) segment tolerates a truncated or CRC-bad **tail** without error
   and reports the damage in `RecoveryReport`. Any of the following is
   a **hard error**, even on the tail segment, and causes `open()` to
   fail: bad magic, unknown wire version, unknown record_type, reserved
   flag set, oversize key/payload. CRC mismatch in a **closed**
   (non-tail) segment is also a hard error.

## Concurrency

9. **Single writer per directory, OS-level advisory lock.**
   `RecordLog::open` acquires an exclusive advisory lock on `{dir}/.lock`
   via `fs2::FileExt::try_lock_exclusive` (POSIX `flock(2)` on Unix,
   `LockFileEx` on Windows). The lock is held by the file descriptor
   stored inside the `RecordLog`, not by the existence of the file.
   Drop / process exit releases the lock automatically; the sentinel
   `.lock` file is allowed to persist between runs. Acquisition is
   non-blocking: a second `open` on the same directory fails fast while
   the first holder is alive. This is *advisory*: cooperating callers
   must all go through `RecordLog::open` for the guarantee to hold.
   Concurrent readers are not guaranteed to see a consistent view in
   v0.1-pre.

9a. **Durability boundary.**
    `RecordLog::append` / `append_record` write a framed, CRC-protected
    record to the active segment file. The record is *recoverable*
    immediately (a subsequent `scan()` returns it), but it is **not
    durable** across a host crash or power loss until `RecordLog::fsync`
    returns successfully. `fsync` `sync_all`s the active segment file
    **and** fsyncs the containing directory. datawal never silently
    fsyncs on every append. Callers requiring per-record durability must
    pair every `append` with an `fsync`.

10. **No network, no RPC.**
    datawal is a library, not a service.

## CAS

11. **CAS / blob store is out of v0.1.**
    The known-good blob implementation in its upstream home is not copied
    into this repo yet. When/if `datawal-cas` is created, it will live in its own
    crate and will *not* be a dependency of `datawal-core`.

## Formal methods

12. **TLA+ comes before the wire format is frozen.**
    The models listed in `formal/README.md` must exist and check before any
    breaking change to the on-disk format is accepted post-v0.1.

## Process

13. **Upstream consumers are not modified in this phase.**
    Existing Rust code that motivated this extraction is treated as a
    **read-only source**. Code that lands in `datawal/` arrives via
    `/bin/cp` followed by in-`datawal/` refactor. No `path = "../..."`
    dependencies on those upstream repos.

14. **No piloting in upstream consumers yet.**
    Migrating any upstream call site onto datawal is *planned*, not
    started. Pilots land only after `RecordLog` works. `v0.1-pre` clears
    that gate.

## Dependencies

15. **Filesystem primitives live in `safeatomic-rs`.**
    Atomic POSIX operations are owned by the sibling crate
    `safeatomic-rs` (at `apps/safeatomic-rs/`). `datawal-core` depends on
    it via `path = "../../../safeatomic-rs"` and uses `write_atomic` for
    JSONL export and `fsync_dir` after creating segments and during
    `fsync()`. This split keeps generic FS plumbing out of the datawal
    public surface and makes those primitives reusable by other
    consumers (e.g. a future `datawal-cas`). `write_append_fsync` in
    `safeatomic-rs` is a **primitive**, not a WAL: datawal owns record
    framing, CRC, segmentation, and recovery.

## v0.1-pre KV semantics

16. **DataWal is last-write-wins, bytes-only.**
    `put(k, v)` and `delete(k)` map directly to `Put` and `Delete`
    records in the underlying log. The in-memory keydir is rebuilt on
    `open` by replaying all records in physical order. `delete` writes
    a tombstone record with an empty payload. `compact_to(out_dir)`
    writes one `Put` per live key into a fresh log and is the only
    supported compaction in v0.1-pre. In-place `compact()` is not
    implemented because it cannot be made safe without further work.

17. **`export_jsonl` is base64.**
    Both key and value are arbitrary bytes. Export writes one JSON
    object per line, `{"key_b64": "...", "value_b64": "..."}`, sorted
    by key for determinism. This is not analytics; it is a transport
    format for downstream tooling.
