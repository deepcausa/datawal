//! Append-only framed record log: the durable substrate of datawal.
//!
//! See [`crate::format`] for the wire format and [`crate::segment`] for the
//! on-disk segment naming convention.
//!
//! v0.1-pre semantics:
//! - Single writer per directory, enforced by an OS-level advisory lock
//!   on `.lock` (see [`crate::lock`]).
//! - One active segment file at a time. `rotate()` closes the current one
//!   and opens the next id.
//! - **Durability boundary.** `append` / `append_record` write a framed,
//!   CRC-protected record to the active segment's file. The record is
//!   immediately *recoverable* (a subsequent `scan()` will return it) but
//!   is **not yet durable** across a host crash or power loss. Durability
//!   is established by a successful call to `fsync()`, which `sync_all`s
//!   the active segment file and fsyncs the containing directory. This
//!   crate never silently fsyncs on every append.
//! - `scan` reads every segment in order and returns every CRC-valid record.
//!   Tail truncation on the **last** segment is treated as recoverable; any
//!   structural error (bad magic, unknown version/type, oversize) and any
//!   mid-stream corruption are hard errors.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};

use crate::format::{
    decode_next, encode_record, DecodeError, DecodeOutcome, RecordType, HEADER_LEN, MAX_KEY_LEN,
    MAX_PAYLOAD_LEN,
};
use crate::lock::DirLock;
use crate::segment::{
    active_segment_id, list_segment_ids, next_segment_id, segment_path, segment_size,
};

/// Reference to a record's location on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RecordRef {
    /// Segment id (matches the on-disk filename).
    pub segment: u32,
    /// Byte offset of the record header within that segment.
    pub offset: u64,
    /// Total wire size of the record (header + key + payload + crc).
    pub len: u32,
}

/// A decoded record returned by [`RecordLog::scan`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub record_type: RecordType,
    pub txid: u64,
    pub key: Vec<u8>,
    pub payload: Vec<u8>,
    pub segment: u32,
    pub offset: u64,
    pub len: u32,
}

/// Summary of the last `scan()` over a log: how many records were valid,
/// how many bytes (if any) of trailing garbage were ignored at the tail of
/// the last segment, and so on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecoveryReport {
    /// Total segment files inspected.
    pub files_scanned: u32,
    /// Total CRC-valid records returned.
    pub records_replayed: u64,
    /// Whether the last segment had a non-fatal truncated/CRC-bad tail.
    /// Counted in segments, not records.
    pub tail_truncated: u32,
    /// Bytes of trailing garbage in the last segment that were skipped.
    pub tail_bytes_discarded: u64,
    /// Number of mid-stream errors detected. v0.1-pre aborts on the first
    /// one, so this is always 0 on success and >0 only if a future variant
    /// switches to lenient mode.
    pub mid_stream_errors: u32,
    /// Always 0 in v0.1-pre because unknown versions are a hard error.
    pub unsupported_versions: u32,
    /// Highest txid observed across all replayed records, or 0 if none.
    pub last_txid_seen: u64,
}

/// Append-only framed record log.
///
/// # Failure model
///
/// Mutating operations (`append`, `append_record`, `fsync`, `rotate`) may
/// fail in the middle of an I/O operation, after the kernel has accepted
/// **part** of a frame but before the whole frame is on disk (`ENOSPC`,
/// a broken disk, a torn write). When that happens the log handle enters
/// a **poisoned** state:
///
/// - Every subsequent mutating call returns a deterministic error whose
///   message starts with `datawal: writer poisoned:` and ends with
///   `; drop handle and reopen`. The error is intentionally a plain
///   `anyhow::Error` in 0.1.x; promotion to a typed error variant is
///   tracked for a future minor release.
/// - Read-only operations (`scan`, `scan_iter`, `recovery_report`,
///   `active_segment`, `dir`) remain available so the caller can inspect
///   state before dropping the handle.
/// - The caller **must** drop the handle and re-open the directory with
///   [`RecordLog::open`]. Reopen uses the standard longest-valid-prefix
///   recovery (see invariant 2 in `AGENTS.md`) and will discard any
///   partial tail bytes left behind by the failed write.
///
/// The crate intentionally does not try to truncate the partial tail or
/// resync `active_size` on the live handle. Both are forms of mutating
/// state after a write failure, which expands rather than contains the
/// blast radius.
#[derive(Debug)]
pub struct RecordLog {
    dir: PathBuf,
    _lock: DirLock,
    active_id: u32,
    /// Open file handle on the active segment, opened in append mode.
    active_file: File,
    /// Cached size of the active segment in bytes, used to compute offsets
    /// for `RecordRef` without an extra `metadata()` call per append.
    active_size: u64,
    /// Next txid to assign on append.
    next_txid: u64,
    /// Last scan report, lazily refreshed by `recovery_report()`.
    last_report: Option<RecoveryReport>,
    /// Set on any mutating I/O failure (`append_record`, `fsync`,
    /// `rotate`). Once set, subsequent mutating calls return a
    /// deterministic error; read-only calls remain available. See the
    /// type-level "Failure model" docs for the full contract.
    poisoned: Option<&'static str>,
}

impl RecordLog {
    /// Open (or create) a record log rooted at `dir`.
    ///
    /// Steps:
    /// 1. `mkdir -p dir`.
    /// 2. Acquire an exclusive OS-level advisory lock on `<dir>/.lock`
    ///    (held by a file descriptor; released automatically when this
    ///    `RecordLog` is dropped or when the holding process exits).
    /// 3. Discover segments; if none, create segment id 1.
    /// 4. Pick the highest id as the active segment.
    /// 5. Scan all segments to discover `next_txid` and store the recovery
    ///    report.
    /// 6. Open the active segment for append.
    ///
    /// Fails fast if another `RecordLog` is already open on the same
    /// directory (the kernel-level lock acquisition does not block).
    pub fn open(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("datawal: create_dir_all {}", dir.display()))?;

        let lock = DirLock::acquire(dir)?;

        let mut ids = list_segment_ids(dir)?;
        if ids.is_empty() {
            // Create segment 1.
            let p = segment_path(dir, 1);
            File::create(&p)
                .with_context(|| format!("datawal: create initial segment {}", p.display()))?;
            // fsync parent so the new file is durable before we proceed.
            safeatomic_rs::fsync_dir(dir)
                .with_context(|| format!("datawal: fsync_dir {}", dir.display()))?;
            ids.push(1);
        }

        let active_id = active_segment_id(dir)?.expect("just ensured at least one segment");

        // Scan once for recovery + next txid.
        let report = scan_all(dir, &ids)?;
        let next_txid = report.last_txid_seen.checked_add(1).unwrap_or(1);

        // Open the active file for append. The recovery report tells us if
        // the active segment has a truncated tail; we ignore that here
        // because we never re-write into the bad region — appends always go
        // to the very end of the file as it currently exists.
        let active_size_logical = report.last_segment_logical_size_for(active_id).unwrap_or(0);
        let active_size_on_disk = segment_size(dir, active_id)?;
        // If there is trailing garbage at the end of the active segment, do
        // **not** physically truncate it in v0.1-pre — that would destroy
        // bytes without an explicit user request. Document the
        // discrepancy by leaving `active_size` set to the logical end.
        let _ = active_size_on_disk;
        let active_file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(false)
            .open(segment_path(dir, active_id))
            .with_context(|| {
                format!(
                    "datawal: open active segment {}",
                    segment_path(dir, active_id).display()
                )
            })?;

        Ok(Self {
            dir: dir.to_path_buf(),
            _lock: lock,
            active_id,
            active_file,
            active_size: active_size_logical,
            next_txid,
            last_report: Some(report.into_public()),
            poisoned: None,
        })
    }

    /// Returns `true` if the writer is poisoned by a prior I/O failure.
    ///
    /// A poisoned log refuses all further mutating operations. Read-only
    /// operations remain available so the caller can inspect state before
    /// dropping the handle. See the type-level "Failure model" docs.
    pub fn is_poisoned(&self) -> bool {
        self.poisoned.is_some()
    }

    /// Internal: produce the stable poison error.
    ///
    /// The message format is part of the public contract documented on
    /// `RecordLog` and is covered by tests; do not change it without
    /// updating both the docs and `tests/poison_writer.rs`.
    fn poison_error(reason: &'static str) -> anyhow::Error {
        anyhow!(
            "datawal: writer poisoned: {}; drop handle and reopen",
            reason
        )
    }

    /// Internal: if poisoned, return the stable poison error.
    fn check_poisoned(&self) -> Result<()> {
        if let Some(reason) = self.poisoned {
            Err(Self::poison_error(reason))
        } else {
            Ok(())
        }
    }

    /// Test-only: synthetically poison the writer. Reached from
    /// integration tests via the `crate::testing` module. Not part
    /// of the public API.
    #[doc(hidden)]
    pub fn __set_poisoned_for_test(&mut self, reason: &'static str) {
        self.poisoned = Some(reason);
    }

    /// Directory backing this log.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Active segment id.
    pub fn active_segment(&self) -> u32 {
        self.active_id
    }

    /// Last recovery report computed by `open()` or `scan()`.
    pub fn recovery_report(&self) -> Result<RecoveryReport> {
        Ok(self.last_report.clone().unwrap_or_default())
    }

    /// Append an opaque payload as a `Raw` record.
    ///
    /// **Durability boundary.** This call writes a framed, CRC-protected
    /// record to the active segment's file via `write_all`. It does **not**
    /// fsync the file or the directory. The record is *recoverable* (a
    /// subsequent `scan()` will return it) as long as the OS does not lose
    /// the buffered write, but it is **not yet durable** across a power
    /// failure or hard crash of the host until `fsync()` returns
    /// successfully.
    ///
    /// Pattern for "this must survive a crash":
    /// ```ignore
    /// log.append(payload)?;
    /// log.fsync()?;
    /// ```
    pub fn append(&mut self, payload: &[u8]) -> Result<RecordRef> {
        self.append_record(RecordType::Raw, b"", payload)
    }

    /// Append a typed record with a key and a payload.
    ///
    /// Used by [`crate::DataWal`] for `Put` / `Delete`. Length limits are
    /// validated by the encoder before allocation.
    ///
    /// Same durability semantics as [`RecordLog::append`]: framed and
    /// recoverable on return, but only durable after a successful
    /// [`RecordLog::fsync`].
    pub fn append_record(
        &mut self,
        record_type: RecordType,
        key: &[u8],
        payload: &[u8],
    ) -> Result<RecordRef> {
        self.check_poisoned()?;

        let txid = self.next_txid;
        // Encoding errors (over-limit key/payload, txid overflow inside the
        // encoder) happen before any I/O and therefore cannot leave a
        // partial frame on disk. Do not poison the writer in that case.
        let bytes = encode_record(record_type, txid, key, payload)?;
        let len = bytes.len() as u32;
        let offset = self.active_size;

        // OpenOptions::append guarantees writes go to the end on POSIX.
        // A failure here may have written a prefix of `bytes` to the
        // segment file. The longest-valid-prefix recovery on reopen will
        // discard the partial tail, but the live handle's `active_size`
        // and `next_txid` would now be out of sync with the file. Poison
        // the writer so subsequent mutating calls fail loudly.
        if let Err(e) = self.active_file.write_all(&bytes) {
            self.poisoned = Some("append_record write_all failed");
            return Err(anyhow::Error::new(e).context(format!(
                "datawal: write_all to segment {}",
                segment_path(&self.dir, self.active_id).display()
            )));
        }
        // The two checked_add calls below cannot leave a partial frame on
        // disk (the write already succeeded). They are integer-overflow
        // guards. Treat overflow as poisoning anyway, because the live
        // handle's offset bookkeeping is now meaningless.
        self.active_size = match self.active_size.checked_add(len as u64) {
            Some(v) => v,
            None => {
                self.poisoned = Some("active segment size overflow");
                return Err(anyhow!("datawal: active segment size overflow"));
            }
        };
        self.next_txid = match txid.checked_add(1) {
            Some(v) => v,
            None => {
                self.poisoned = Some("txid overflow");
                return Err(anyhow!("datawal: txid overflow at {}", txid));
            }
        };

        Ok(RecordRef {
            segment: self.active_id,
            offset,
            len,
        })
    }

    /// Scan every segment in order and return every valid record.
    ///
    /// Materialises every record into a `Vec<Record>`. For logs with many
    /// records or large payloads, prefer [`RecordLog::scan_iter`] which
    /// yields one record at a time without materialising the whole log.
    ///
    /// Also refreshes `recovery_report()` and the internal `next_txid`.
    pub fn scan(&mut self) -> Result<Vec<Record>> {
        let ids = list_segment_ids(&self.dir)?;
        let internal = scan_all(&self.dir, &ids)?;
        self.next_txid = internal.last_txid_seen.checked_add(1).unwrap_or(1);
        self.last_report = Some(internal.clone().into_public());
        Ok(internal.records)
    }

    /// Returns an iterator over records.
    ///
    /// This is lazy at the record level: callers can pull one record at a
    /// time without materialising the whole log into a `Vec<Record>`. It
    /// is **not** a chunked or zero-copy scanner — v0.1 loads one segment
    /// at a time into memory before yielding records from it. Peak memory
    /// is therefore bounded by the size of the **largest segment**, not by
    /// the total log size.
    ///
    /// Recovery semantics match [`RecordLog::scan`]:
    ///
    /// - A truncated or CRC-bad tail on the **last** segment is tolerated
    ///   and ends iteration cleanly. The amount of trailing garbage
    ///   discarded is reflected in
    ///   [`RecordIter::recovery_report`].
    /// - Any structural decode error, or any CRC/truncation problem in a
    ///   sealed (non-last) segment, is yielded as an `Err` item; iteration
    ///   ends after that error, and the underlying error is the same
    ///   `anyhow` error that [`RecordLog::scan`] would have returned.
    ///
    /// This method takes `&self`. It does not refresh the log's own
    /// `recovery_report()` or `next_txid` — only [`RecordLog::scan`] does
    /// that.
    ///
    /// Aborting iteration early (by dropping the iterator before
    /// exhaustion) is supported and has no on-disk side effects.
    pub fn scan_iter(&self) -> Result<RecordIter<'_>> {
        let ids = list_segment_ids(&self.dir)?;
        Ok(RecordIter::new(&self.dir, ids))
    }

    /// Force durability of all records appended so far.
    ///
    /// On successful return, every record passed to `append` /
    /// `append_record` since this `RecordLog` was opened (or since the last
    /// `fsync` returned) is durable: it will survive a process crash,
    /// kernel panic or power loss on the underlying disk, modulo the
    /// usual filesystem caveats (working `fsync` syscall, no lying disk
    /// cache).
    ///
    /// Internally this calls `File::sync_all` on the active segment **and**
    /// `fsync` on the containing directory, so that segment creations and
    /// rotations are also durable.
    ///
    /// `fsync` may be called as often as desired; on a log with no new
    /// appends since the last fsync it is effectively a no-op at the
    /// kernel level, but it is always safe.
    pub fn fsync(&mut self) -> Result<()> {
        self.check_poisoned()?;

        // `sync_all` can fail (`EIO`, journal flush refused, etc.). On
        // Linux, fsync errors are not always re-reported on subsequent
        // calls, so we treat any fsync failure as a fatal event for this
        // handle. The on-disk state is whatever the kernel made of the
        // dirty pages; the caller must drop + reopen and let
        // longest-valid-prefix recovery settle it.
        if let Err(e) = self.active_file.sync_all() {
            self.poisoned = Some("fsync sync_all failed");
            return Err(anyhow::Error::new(e).context(format!(
                "datawal: sync_all on segment {}",
                segment_path(&self.dir, self.active_id).display()
            )));
        }
        // Also fsync the directory so the directory entry of the active
        // segment (and any prior `rotate()` rename) is durable.
        if let Err(e) = safeatomic_rs::fsync_dir(&self.dir) {
            self.poisoned = Some("fsync fsync_dir failed");
            return Err(e.context(format!("datawal: fsync_dir {}", self.dir.display())));
        }
        Ok(())
    }

    /// Rotate to the next segment. The current segment is closed and
    /// fsynced; the new segment is created empty and becomes active.
    pub fn rotate(&mut self) -> Result<()> {
        self.check_poisoned()?;

        // Make the current segment durable before moving on. A failure
        // here means the previous segment's tail durability is in doubt,
        // so we poison just like in `fsync`.
        if let Err(e) = self.active_file.sync_all() {
            self.poisoned = Some("rotate sync_all on previous segment failed");
            return Err(anyhow::Error::new(e).context(format!(
                "datawal: sync_all on rotate, segment {}",
                segment_path(&self.dir, self.active_id).display()
            )));
        }

        let ids = list_segment_ids(&self.dir)?;
        let new_id = next_segment_id(&ids)?;
        if new_id <= self.active_id {
            // Defensive: not reachable from a clean log because
            // `next_segment_id` returns `max(ids) + 1` and `active_id`
            // is in `ids`. Poison anyway because the on-disk segment
            // sequence is now in an unexpected state.
            self.poisoned = Some("rotate computed non-increasing segment id");
            bail!(
                "datawal: rotate computed non-increasing segment id (current={}, computed={})",
                self.active_id,
                new_id
            );
        }
        let new_path = segment_path(&self.dir, new_id);
        // A failure between creating the new segment file and opening it
        // for append would leave a zero-byte segment on disk that the
        // next reopen would treat as the active segment. That is the
        // textbook poison case.
        if let Err(e) = File::create(&new_path) {
            self.poisoned = Some("rotate create new segment failed");
            return Err(anyhow::Error::new(e)
                .context(format!("datawal: create segment {}", new_path.display())));
        }
        if let Err(e) = safeatomic_rs::fsync_dir(&self.dir) {
            self.poisoned = Some("rotate fsync_dir after segment create failed");
            return Err(e.context(format!("datawal: fsync_dir {}", self.dir.display())));
        }

        let new_file = match OpenOptions::new().read(true).append(true).open(&new_path) {
            Ok(f) => f,
            Err(e) => {
                self.poisoned = Some("rotate open new active segment failed");
                return Err(anyhow::Error::new(e).context(format!(
                    "datawal: open new active segment {}",
                    new_path.display()
                )));
            }
        };
        self.active_file = new_file;
        self.active_id = new_id;
        self.active_size = 0;
        Ok(())
    }

    /// Close the log, releasing the directory lock.
    pub fn close(self) -> Result<()> {
        // Dropping `self` runs `DirLock::drop`, which closes the lock file
        // descriptor and releases the kernel-level flock. The sentinel
        // `.lock` file itself remains on disk; it is not the lock.
        Ok(())
    }
}

/// Internal scan state: accumulates records (for the eager `scan_all` path)
/// or just counts them (for the streaming `RecordIter` path). Kept private
/// so future field changes do not break the public API.
#[derive(Debug, Clone)]
struct ScanInternal {
    /// Records collected eagerly. Empty in the streaming path.
    records: Vec<Record>,
    /// Number of records replayed so far. Drives `RecoveryReport.records_replayed`.
    /// In the eager path this equals `records.len()`; in the streaming
    /// path `records` stays empty and only this counter advances.
    records_replayed: u64,
    files_scanned: u32,
    last_txid_seen: u64,
    tail_truncated: u32,
    tail_bytes_discarded: u64,
    last_segment_logical_end: Option<(u32, u64)>,
}

impl ScanInternal {
    fn last_segment_logical_size_for(&self, segment: u32) -> Option<u64> {
        self.last_segment_logical_end
            .filter(|(id, _)| *id == segment)
            .map(|(_, end)| end)
    }

    fn into_public(self) -> RecoveryReport {
        RecoveryReport {
            files_scanned: self.files_scanned,
            records_replayed: self.records_replayed,
            tail_truncated: self.tail_truncated,
            tail_bytes_discarded: self.tail_bytes_discarded,
            mid_stream_errors: 0,
            unsupported_versions: 0,
            last_txid_seen: self.last_txid_seen,
        }
    }
}

/// Read a segment file completely into memory and decode every record.
///
/// `is_last_segment` controls how trailing problems are treated: tolerated
/// for the last segment, hard error otherwise.
fn scan_segment(dir: &Path, id: u32, is_last_segment: bool, out: &mut ScanInternal) -> Result<()> {
    let path = segment_path(dir, id);
    let mut f =
        File::open(&path).with_context(|| format!("datawal: open segment {}", path.display()))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)
        .with_context(|| format!("datawal: read_to_end {}", path.display()))?;
    let mut offset: u64 = 0;
    let file_len = buf.len() as u64;

    loop {
        if offset == file_len {
            out.last_segment_logical_end = Some((id, offset));
            break;
        }
        match decode_next(&buf, offset) {
            Ok(DecodeOutcome::Ok {
                record_type,
                txid,
                key,
                payload,
                bytes_consumed,
            }) => {
                let len = bytes_consumed;
                out.records.push(Record {
                    record_type,
                    txid,
                    key,
                    payload,
                    segment: id,
                    offset,
                    len,
                });
                out.records_replayed += 1;
                if txid > out.last_txid_seen {
                    out.last_txid_seen = txid;
                }
                offset += bytes_consumed as u64;
            }
            Ok(DecodeOutcome::Truncated { .. }) => {
                if is_last_segment {
                    let discarded = file_len - offset;
                    out.tail_truncated += 1;
                    out.tail_bytes_discarded += discarded;
                    out.last_segment_logical_end = Some((id, offset));
                    break;
                } else {
                    bail!(
                        "datawal: truncated record at offset {} of non-tail segment {} ({}); refusing to silently drop data",
                        offset,
                        id,
                        path.display()
                    );
                }
            }
            Ok(DecodeOutcome::CrcMismatch { bytes_consumed }) => {
                if is_last_segment {
                    // Treat as tail damage and stop.
                    let discarded = file_len - offset;
                    out.tail_truncated += 1;
                    out.tail_bytes_discarded += discarded;
                    out.last_segment_logical_end = Some((id, offset));
                    let _ = bytes_consumed;
                    break;
                } else {
                    bail!(
                        "datawal: CRC mismatch at offset {} of non-tail segment {} ({})",
                        offset,
                        id,
                        path.display()
                    );
                }
            }
            Err(err) => {
                // Structural / hard errors are never silently tolerated, not
                // even on the tail segment.
                let _: DecodeError = err;
                bail!(
                    "datawal: structural decode error at offset {} of segment {} ({}): {}",
                    offset,
                    id,
                    path.display(),
                    err
                );
            }
        }
    }
    Ok(())
}

/// Scan every segment in `ids` (must be sorted ascending). Treat the
/// final segment as recoverable for tail problems; all earlier segments
/// must be fully clean.
fn scan_all(dir: &Path, ids: &[u32]) -> Result<ScanInternal> {
    let mut out = ScanInternal {
        records: Vec::new(),
        records_replayed: 0,
        files_scanned: 0,
        last_txid_seen: 0,
        tail_truncated: 0,
        tail_bytes_discarded: 0,
        last_segment_logical_end: None,
    };
    if ids.is_empty() {
        return Ok(out);
    }
    let last_idx = ids.len() - 1;
    for (i, id) in ids.iter().enumerate() {
        out.files_scanned += 1;
        let is_last = i == last_idx;
        scan_segment(dir, *id, is_last, &mut out)?;
    }
    Ok(out)
}

/// Record-level lazy iterator over a [`RecordLog`].
///
/// Yielded by [`RecordLog::scan_iter`]. The iterator is lazy at the
/// record level: each call to `next()` decodes one frame. It is **not**
/// zero-copy and it is **not** chunked I/O — one whole segment file is
/// resident in memory at a time. Peak memory is bounded by the largest
/// segment, not by the total log size.
///
/// The list of segment ids is snapshotted when [`RecordLog::scan_iter`]
/// is called; segments rotated in by the writer after that point are
/// **not** observed by this iterator (this matches [`RecordLog::scan`]).
///
/// The borrow on `'log` only scopes the iterator to the lifetime of the
/// [`RecordLog`]; the iterator does not hold the directory lock itself.
pub struct RecordIter<'log> {
    dir: PathBuf,
    /// Snapshot of segment ids at the time `scan_iter` was called, sorted
    /// ascending.
    ids: Vec<u32>,
    /// Index into `ids` of the segment currently being decoded.
    cur_idx: usize,
    /// Bytes of the current segment, fully loaded into memory.
    cur_buf: Vec<u8>,
    /// Logical decode cursor within `cur_buf`.
    cur_offset: u64,
    /// Id of the current segment (mirrors `ids[cur_idx]`, cached for
    /// `Record::segment` without re-indexing).
    cur_id: u32,
    /// Whether the current segment has been loaded yet (cleared whenever
    /// we advance to a new segment).
    cur_loaded: bool,
    /// Accumulated recovery state for [`RecordIter::report`].
    report: ScanInternal,
    /// Set to `true` once iteration has yielded `None` or a hard error.
    /// Subsequent calls to `next()` always return `None`.
    done: bool,
    /// Borrow tag tying this iterator to its parent log.
    _log: std::marker::PhantomData<&'log RecordLog>,
}

impl<'log> RecordIter<'log> {
    fn new(dir: &Path, ids: Vec<u32>) -> Self {
        Self {
            dir: dir.to_path_buf(),
            ids,
            cur_idx: 0,
            cur_buf: Vec::new(),
            cur_offset: 0,
            cur_id: 0,
            cur_loaded: false,
            report: ScanInternal {
                records: Vec::new(),
                records_replayed: 0,
                files_scanned: 0,
                last_txid_seen: 0,
                tail_truncated: 0,
                tail_bytes_discarded: 0,
                last_segment_logical_end: None,
            },
            done: false,
            _log: std::marker::PhantomData,
        }
    }

    /// Return the accumulated recovery report.
    ///
    /// The report is **complete only after the iterator has been fully
    /// consumed** (i.e. once `next()` has returned `None`). While
    /// iteration is in progress this returns a partial snapshot:
    ///
    /// - `records_replayed` counts only records the iterator has yielded
    ///   successfully so far.
    /// - `tail_truncated` and `tail_bytes_discarded` are populated only
    ///   when the iterator has reached and finished processing the last
    ///   segment.
    /// - `files_scanned` increments as each segment is loaded; if
    ///   iteration is dropped mid-segment, that segment is still counted.
    pub fn recovery_report(&self) -> RecoveryReport {
        self.report.clone().into_public()
    }

    /// Load `ids[cur_idx]` into `cur_buf` if not already loaded.
    fn ensure_loaded(&mut self) -> Result<()> {
        if self.cur_loaded {
            return Ok(());
        }
        let id = self.ids[self.cur_idx];
        let path = segment_path(&self.dir, id);
        let mut f = File::open(&path)
            .with_context(|| format!("datawal: open segment {}", path.display()))?;
        self.cur_buf.clear();
        f.read_to_end(&mut self.cur_buf)
            .with_context(|| format!("datawal: read_to_end {}", path.display()))?;
        self.cur_id = id;
        self.cur_offset = 0;
        self.cur_loaded = true;
        self.report.files_scanned += 1;
        Ok(())
    }

    /// Try to decode one more record from the current state. Returns:
    /// - `Ok(Some(record))` — yielded a record, more may follow.
    /// - `Ok(None)` — current segment is done; caller should advance.
    /// - `Err(_)` — hard error; iterator must terminate.
    fn try_next_in_segment(&mut self) -> Result<Option<Record>> {
        let id = self.cur_id;
        let is_last = self.cur_idx + 1 == self.ids.len();
        let file_len = self.cur_buf.len() as u64;

        if self.cur_offset == file_len {
            self.report.last_segment_logical_end = Some((id, self.cur_offset));
            return Ok(None);
        }

        match decode_next(&self.cur_buf, self.cur_offset) {
            Ok(DecodeOutcome::Ok {
                record_type,
                txid,
                key,
                payload,
                bytes_consumed,
            }) => {
                let len = bytes_consumed;
                let offset = self.cur_offset;
                self.cur_offset += bytes_consumed as u64;
                if txid > self.report.last_txid_seen {
                    self.report.last_txid_seen = txid;
                }
                // Track records_replayed by counting yielded records: we
                // do not store them in `self.report.records` (that vector
                // is only used by the eager `scan_all` path).
                Ok(Some(Record {
                    record_type,
                    txid,
                    key,
                    payload,
                    segment: id,
                    offset,
                    len,
                }))
            }
            Ok(DecodeOutcome::Truncated { .. }) => {
                if is_last {
                    let discarded = file_len - self.cur_offset;
                    self.report.tail_truncated += 1;
                    self.report.tail_bytes_discarded += discarded;
                    self.report.last_segment_logical_end = Some((id, self.cur_offset));
                    Ok(None)
                } else {
                    let path = segment_path(&self.dir, id);
                    Err(anyhow!(
                        "datawal: truncated record at offset {} of non-tail segment {} ({}); refusing to silently drop data",
                        self.cur_offset,
                        id,
                        path.display()
                    ))
                }
            }
            Ok(DecodeOutcome::CrcMismatch { bytes_consumed }) => {
                if is_last {
                    let discarded = file_len - self.cur_offset;
                    self.report.tail_truncated += 1;
                    self.report.tail_bytes_discarded += discarded;
                    self.report.last_segment_logical_end = Some((id, self.cur_offset));
                    let _ = bytes_consumed;
                    Ok(None)
                } else {
                    let path = segment_path(&self.dir, id);
                    Err(anyhow!(
                        "datawal: CRC mismatch at offset {} of non-tail segment {} ({})",
                        self.cur_offset,
                        id,
                        path.display()
                    ))
                }
            }
            Err(err) => {
                let _: DecodeError = err;
                let path = segment_path(&self.dir, id);
                Err(anyhow!(
                    "datawal: structural decode error at offset {} of segment {} ({}): {}",
                    self.cur_offset,
                    id,
                    path.display(),
                    err
                ))
            }
        }
    }
}

impl Iterator for RecordIter<'_> {
    type Item = Result<Record>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        loop {
            if self.cur_idx >= self.ids.len() {
                self.done = true;
                return None;
            }
            if let Err(e) = self.ensure_loaded() {
                self.done = true;
                return Some(Err(e));
            }
            match self.try_next_in_segment() {
                Ok(Some(rec)) => {
                    self.report.records_replayed += 1;
                    return Some(Ok(rec));
                }
                Ok(None) => {
                    // Current segment exhausted (cleanly, or tail-truncated
                    // on the last segment). Advance to the next.
                    self.cur_idx += 1;
                    self.cur_loaded = false;
                    self.cur_buf.clear();
                    continue;
                }
                Err(e) => {
                    self.done = true;
                    return Some(Err(e));
                }
            }
        }
    }
}

#[allow(dead_code)]
const _ASSERT_HEADER: () = {
    // Document the wire constants compile-time so the doc-comment cannot drift.
    let _ = HEADER_LEN;
    let _ = MAX_KEY_LEN;
    let _ = MAX_PAYLOAD_LEN;
};
