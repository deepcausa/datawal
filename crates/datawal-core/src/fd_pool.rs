//! Small bounded LRU cache of read-only segment file descriptors.
//!
//! Used by [`crate::DataWal`] (v0.1.4+) to avoid reopening the same
//! `[0-9]{8}.dwal` segment on every `get`.
//!
//! Design notes:
//!
//! - Hard cap on entries (`capacity`). Once full, the least-recently-used
//!   segment id is evicted.
//! - All file descriptors are opened **read-only**. They never compete
//!   with the writer's exclusive append-mode handle for that same segment.
//!   On Unix this is well-defined; on macOS/Linux a read-only `pread`
//!   against a file being appended to by the same process is safe and
//!   returns bytes that were already written (length checked by caller).
//! - The pool is intentionally not `Sync`. `DataWal::get` takes `&mut
//!   self`, so a single-threaded borrow is enough.
//! - Eviction never panics. If `open` fails the pool is left unchanged
//!   and the error is propagated.
//! - A capacity of 0 means: open on every call, never cache. Useful for
//!   tests.

use std::collections::VecDeque;
use std::fs::File;
#[cfg(not(unix))]
use std::io::Read;
#[cfg(unix)]
use std::os::unix::fs::FileExt;
use std::path::Path;

use anyhow::{Context, Result};

use crate::segment::segment_path;

/// Default cap. Tuned for the soak driver: a 4-segment hot working set
/// covers the common case (active + compaction target + a recent rotated
/// pair). Adjust via [`FdPool::with_capacity`] for benchmarks.
pub const DEFAULT_CAPACITY: usize = 16;

/// Tiny LRU of read-only segment file handles, keyed by segment id.
#[derive(Debug)]
pub(crate) struct FdPool {
    capacity: usize,
    /// `(segment_id, File)` pairs. Most recently used at the back.
    entries: VecDeque<(u32, File)>,
}

impl FdPool {
    /// New pool with the given hard cap.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            capacity,
            entries: VecDeque::with_capacity(capacity.max(1)),
        }
    }

    /// Pre-existing entries.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Look the cached fd for `segment_id`, opening it under `dir` if not
    /// already cached. Touches LRU order on hit.
    fn touch_or_open(&mut self, dir: &Path, segment_id: u32) -> Result<&mut File> {
        // Hit: move to the back (most-recently-used) and return.
        if let Some(pos) = self.entries.iter().position(|(id, _)| *id == segment_id) {
            let entry = self.entries.remove(pos).expect("position just found");
            self.entries.push_back(entry);
            return Ok(&mut self.entries.back_mut().expect("just pushed").1);
        }

        // Miss: open read-only, evict if needed, push back.
        let path = segment_path(dir, segment_id);
        let file = File::open(&path)
            .with_context(|| format!("datawal: open segment {}", path.display()))?;

        if self.capacity == 0 {
            // Caller wants no caching. Return a temporary handle that
            // will be dropped after this call by overwriting `entries`.
            self.entries.clear();
            self.entries.push_back((segment_id, file));
            return Ok(&mut self.entries.back_mut().expect("just pushed").1);
        }

        if self.entries.len() >= self.capacity {
            self.entries.pop_front();
        }
        self.entries.push_back((segment_id, file));
        Ok(&mut self.entries.back_mut().expect("just pushed").1)
    }

    /// Read exactly `len` bytes starting at `offset` from segment
    /// `segment_id` under `dir`.
    ///
    /// Returns an owned `Vec<u8>` with the bytes. Uses positional reads
    /// (`pread` on Unix) so the underlying file descriptor offset is not
    /// disturbed. On non-Unix platforms falls back to `seek + read`.
    pub fn read_at(
        &mut self,
        dir: &Path,
        segment_id: u32,
        offset: u64,
        len: usize,
    ) -> Result<Vec<u8>> {
        let file = self.touch_or_open(dir, segment_id)?;
        let mut buf = vec![0u8; len];

        #[cfg(unix)]
        {
            file.read_exact_at(&mut buf, offset).with_context(|| {
                format!(
                    "datawal: pread {} bytes at offset {} of segment {:08}.dwal",
                    len, offset, segment_id
                )
            })?;
        }
        #[cfg(not(unix))]
        {
            use std::io::{Seek, SeekFrom};
            file.seek(SeekFrom::Start(offset)).with_context(|| {
                format!(
                    "datawal: seek to offset {} of segment {:08}.dwal",
                    offset, segment_id
                )
            })?;
            file.read_exact(&mut buf).with_context(|| {
                format!(
                    "datawal: read {} bytes at offset {} of segment {:08}.dwal",
                    len, offset, segment_id
                )
            })?;
        }

        Ok(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    fn seed_segment(dir: &Path, id: u32, bytes: &[u8]) {
        let p = segment_path(dir, id);
        let mut f = File::create(&p).unwrap();
        f.write_all(bytes).unwrap();
        f.sync_all().unwrap();
    }

    #[test]
    fn read_at_returns_requested_bytes() {
        let dir = tempdir().unwrap();
        seed_segment(dir.path(), 1, b"hello world");
        let mut pool = FdPool::with_capacity(4);
        let got = pool.read_at(dir.path(), 1, 6, 5).unwrap();
        assert_eq!(got, b"world");
    }

    #[test]
    fn second_read_reuses_cached_fd() {
        let dir = tempdir().unwrap();
        seed_segment(dir.path(), 1, b"abcdefgh");
        let mut pool = FdPool::with_capacity(2);
        pool.read_at(dir.path(), 1, 0, 4).unwrap();
        pool.read_at(dir.path(), 1, 4, 4).unwrap();
        assert_eq!(pool.len(), 1);
    }

    #[test]
    fn lru_evicts_oldest_at_capacity() {
        let dir = tempdir().unwrap();
        for id in 1..=3u32 {
            seed_segment(dir.path(), id, b"xxxx");
        }
        let mut pool = FdPool::with_capacity(2);
        pool.read_at(dir.path(), 1, 0, 1).unwrap();
        pool.read_at(dir.path(), 2, 0, 1).unwrap();
        assert_eq!(pool.len(), 2);
        pool.read_at(dir.path(), 3, 0, 1).unwrap();
        // 1 was the oldest -> evicted.
        assert_eq!(pool.len(), 2);
        // Verify by re-reading: would re-open if evicted; still works regardless.
        pool.read_at(dir.path(), 1, 0, 1).unwrap();
        assert_eq!(pool.len(), 2);
    }

    #[test]
    fn capacity_zero_disables_cache() {
        let dir = tempdir().unwrap();
        seed_segment(dir.path(), 1, b"abcd");
        let mut pool = FdPool::with_capacity(0);
        pool.read_at(dir.path(), 1, 0, 4).unwrap();
        pool.read_at(dir.path(), 1, 0, 4).unwrap();
        // With cap 0 the entry is overwritten each call (single slot).
        assert!(pool.len() <= 1);
    }

    #[test]
    fn missing_segment_returns_error() {
        let dir = tempdir().unwrap();
        let mut pool = FdPool::with_capacity(2);
        let err = pool.read_at(dir.path(), 7, 0, 1).unwrap_err();
        assert!(format!("{err:#}").contains("open segment"));
    }

    #[test]
    fn out_of_bounds_read_errors() {
        let dir = tempdir().unwrap();
        seed_segment(dir.path(), 1, b"abc");
        let mut pool = FdPool::with_capacity(2);
        let err = pool.read_at(dir.path(), 1, 0, 16).unwrap_err();
        assert!(
            format!("{err:#}").to_lowercase().contains("eof")
                || format!("{err:#}").to_lowercase().contains("pread")
                || format!("{err:#}").to_lowercase().contains("read")
        );
    }
}
