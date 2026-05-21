//! Read-only, lock-free view of a [`RecordLog`](crate::RecordLog) directory.
//!
//! [`RecordLogReader`] is the lightweight counterpart of [`RecordLog`]:
//!
//! - It opens a log directory **without** taking the cooperative
//!   single-writer lock (`<dir>/.lock`). Multiple readers may coexist with
//!   each other and with a live writer in the same or a different
//!   process.
//! - It snapshots the set of segment ids at construction time. Segments
//!   created or rotated in after [`RecordLogReader::open`] returns are
//!   **not** observed by this handle. Reopen the reader to pick them up.
//! - It exposes one method, [`RecordLogReader::scan_iter`], which yields
//!   records lazily one at a time. The returned [`RecordIter`] decodes
//!   one segment file at a time; peak memory is bounded by the size of
//!   the **largest** segment in the snapshot.
//!
//! # What this is for
//!
//! Observing the contents of a log that is being written to by another
//! handle (in the same or a different process), without disturbing the
//! writer. Typical uses:
//!
//! - An out-of-band inspector / dashboard.
//! - A tailing process that periodically re-opens and scans the tail.
//! - A test harness that wants to compare on-disk state against an
//!   in-memory oracle while the writer keeps appending.
//!
//! # What this is not
//!
//! - **Not** a live tail. The snapshot is taken at `open()` time and is
//!   not refreshed by the iterator. Reopen to widen the snapshot.
//! - **Not** a concurrent reader for the *same process's* writer
//!   instance. The writer's [`RecordLog`] already exposes
//!   [`RecordLog::scan_iter`] for that purpose; that variant snapshots
//!   the segment list the same way and borrows immutably.
//! - **Not** a substitute for the writer's recovery. The reader does
//!   *not* run [`RecordLog`]'s longest-valid-prefix recovery into
//!   `next_txid` or update any persistent state; it only decodes the
//!   bytes it finds.
//!
//! # Concurrency semantics with a live writer
//!
//! The wire format is a series of self-delimiting frames with a 24-byte
//! header, key + payload bytes, and a 4-byte CRC trailer. A reader that
//! opens after the writer has appended some records sees exactly those
//! frames; a reader that opens **before** the writer's next append
//! still sees only the frames that were present at open time *plus*
//! any bytes that happened to be on disk when each segment file is
//! loaded.
//!
//! In practice: the iterator loads one full segment file into memory
//! when it advances to that segment. A frame whose **header** is on
//! disk but whose **body or CRC** is not yet on disk shows up to the
//! reader as a tail-truncated or CRC-mismatched record on the **last**
//! segment of the snapshot. The iterator treats this as a clean end of
//! the active segment (consistent with [`RecordLog::scan_iter`]) and
//! reports the discarded byte count via
//! [`RecordIter::recovery_report`]. Sealed (non-last) segments are
//! treated strictly: any truncation or CRC mismatch there is a hard
//! error, because the writer should never leave a sealed segment in a
//! bad state.
//!
//! The reader does not coordinate with the writer's `fsync`. A record
//! that the writer has appended but not yet fsynced may already be
//! visible to a reader because the OS page cache makes it readable
//! through a fresh `open()`, but the same record is *not yet durable*
//! across a crash. The durability contract belongs to the writer.
//!
//! # Examples
//!
//! ```no_run
//! use datawal::RecordLogReader;
//!
//! let reader = RecordLogReader::open(std::path::Path::new("/var/lib/my-app/log"))?;
//! let mut count = 0u64;
//! for rec in reader.scan_iter()? {
//!     let rec = rec?;
//!     count += 1;
//!     let _ = rec;
//! }
//! println!("snapshot contained {count} records");
//! # Ok::<(), anyhow::Error>(())
//! ```

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::record_log::RecordIter;
use crate::segment::list_segment_ids;

/// Read-only, lock-free handle on a [`RecordLog`](crate::RecordLog) directory.
///
/// See the documentation on each method for the full contract: snapshot
/// at open, no fs2 lock, one-segment-at-a-time decode, tail truncation on
/// the last segment is tolerated, mid-stream errors on sealed segments
/// are hard errors.
#[derive(Debug)]
pub struct RecordLogReader {
    dir: PathBuf,
    /// Snapshot of segment ids taken at `open()`. Sorted ascending.
    snapshot_ids: Vec<u32>,
}

impl RecordLogReader {
    /// Open a log directory in read-only mode.
    ///
    /// Does **not** acquire the cooperative single-writer lock. Does
    /// **not** create the directory: returns an error if `dir` does not
    /// exist or is not a directory. Snapshots the set of segment ids
    /// matching `[0-9]{8}\.dwal` exactly; ignores anything else
    /// (including a writer's `.lock` sentinel).
    ///
    /// Returns successfully even if the directory is empty
    /// (no segments). In that case [`RecordLogReader::scan_iter`]
    /// yields zero records.
    pub fn open(dir: &Path) -> Result<Self> {
        let meta = std::fs::metadata(dir)
            .with_context(|| format!("datawal: stat reader dir {}", dir.display()))?;
        if !meta.is_dir() {
            anyhow::bail!("datawal: reader dir {} is not a directory", dir.display());
        }
        let snapshot_ids = list_segment_ids(dir)?;
        Ok(Self {
            dir: dir.to_path_buf(),
            snapshot_ids,
        })
    }

    /// Directory backing this reader.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Segment ids snapshotted at `open()`, in ascending order.
    ///
    /// Empty if the directory contained no segments at open time.
    /// Segments created after `open()` returned are not present here
    /// and will not be observed by [`RecordLogReader::scan_iter`].
    pub fn segments(&self) -> &[u32] {
        &self.snapshot_ids
    }

    /// Iterate over the records in the open-time snapshot.
    ///
    /// Yields one record at a time. Decodes one segment file at a time
    /// (peak memory is bounded by the size of the largest segment in
    /// the snapshot, not by the total log size).
    ///
    /// Recovery semantics mirror [`RecordLog::scan_iter`]: a truncated
    /// or CRC-bad tail on the **last** snapshotted segment is tolerated
    /// and ends iteration cleanly (the discarded byte count is reflected
    /// in [`RecordIter::recovery_report`]). Any structural decode error
    /// or any truncation / CRC problem on a non-last segment yields an
    /// `Err` item, after which iteration ends.
    ///
    /// Calling `scan_iter` multiple times on the same reader is
    /// allowed; each call returns a fresh iterator over the same
    /// snapshot.
    ///
    /// [`RecordLog::scan_iter`]: crate::RecordLog::scan_iter
    pub fn scan_iter(&self) -> Result<RecordIter<'_>> {
        Ok(RecordIter::new(&self.dir, self.snapshot_ids.clone()))
    }
}
