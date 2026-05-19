//! Cooperative directory lock for a `RecordLog`.
//!
//! v0.1-pre uses an OS-level advisory lock (POSIX `flock(2)` on Unix,
//! `LockFileEx` on Windows) on the sentinel file `path/.lock`, obtained
//! through the [`fs2`] crate.
//!
//! Properties:
//!
//! - **Held by a file descriptor**, not by the existence of the file. The
//!   lock is bound to the open `File` handle inside [`DirLock`], so the lock
//!   is automatically released by the kernel when the handle is dropped
//!   (including when the holding process crashes or is killed). There is no
//!   stale-lock problem.
//! - **Cooperative / advisory.** Another process that opens the file with a
//!   different flock policy, or that bypasses the lock entirely, can still
//!   corrupt the log. Datawal does not promise multi-writer correctness.
//! - **Single-writer per directory.** Two concurrent [`DirLock::acquire`]
//!   calls on the same directory will see the second one fail fast (it does
//!   not block). This is what `RecordLog::open` relies on.
//! - **Local POSIX filesystems only.** Network filesystems (NFS, SMB) may
//!   not honour `flock` correctly. v0.1-pre assumes a local filesystem.
//!
//! The sentinel file itself is created on first acquire and is left in place
//! across runs. This is deliberate: the file existence is not the lock.

use std::fs::{self, File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use fs2::FileExt;

/// Name of the sentinel lock file inside a log directory.
pub const LOCK_FILENAME: &str = ".lock";

/// RAII handle that holds an exclusive OS-level lock on a directory's
/// sentinel file. The lock is released when this value is dropped.
#[derive(Debug)]
pub struct DirLock {
    path: PathBuf,
    /// Holding this `File` open keeps the kernel-level flock active.
    /// Dropping the `File` releases the lock.
    file: File,
}

impl DirLock {
    /// Attempt to acquire the lock on `dir`.
    ///
    /// Fails fast (does not block) if another process or thread already holds
    /// the lock. The sentinel file `<dir>/.lock` is created on demand.
    ///
    /// Writes the current pid into the lock file as a diagnostic. The lock is
    /// **not** trusted to the pid; it is held by the file descriptor.
    pub fn acquire(dir: &Path) -> Result<Self> {
        fs::create_dir_all(dir)
            .with_context(|| format!("datawal: create_dir_all {}", dir.display()))?;
        let path = dir.join(LOCK_FILENAME);
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("datawal: open lock file {}", path.display()))?;

        // Try to grab an exclusive advisory lock without blocking. If the lock
        // is already held by another open file descriptor (this process or
        // another), this returns an error.
        file.try_lock_exclusive().map_err(|e| {
            anyhow!(
                "datawal: log directory is already locked: {} ({}). Another process is using \
                 this datawal directory.",
                path.display(),
                e
            )
        })?;

        // Best-effort diagnostic: overwrite the file with our pid. Failure to
        // write the pid does not affect the lock itself.
        let pid = std::process::id();
        let _ = file.set_len(0);
        let _ = file.seek(SeekFrom::Start(0));
        let _ = writeln!(file, "{pid}");
        let _ = file.sync_all();

        Ok(Self {
            path,
            file: _take(file),
        })
    }

    /// Path of the sentinel file. For diagnostics.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Identity helper used purely to silence an "unused mut" lint on `file` by
/// moving it explicitly. Keeps `acquire` readable without `#[allow]`.
fn _take(f: File) -> File {
    f
}

impl Drop for DirLock {
    fn drop(&mut self) {
        // The lock is automatically released when `self.file` is dropped. We
        // also unlock explicitly as a belt-and-suspenders measure; failure is
        // not actionable here.
        let _ = FileExt::unlock(&self.file);
        // Do NOT remove the sentinel file. Its existence is not the lock; the
        // lock is the fd. Leaving the file in place avoids races on next open.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn acquire_then_release_allows_reacquire() {
        let td = TempDir::new().unwrap();
        {
            let _l = DirLock::acquire(td.path()).unwrap();
            assert!(td.path().join(LOCK_FILENAME).exists());
        }
        // Sentinel file persists, but the lock is gone.
        assert!(td.path().join(LOCK_FILENAME).exists());
        let _l2 = DirLock::acquire(td.path()).unwrap();
    }

    #[test]
    fn second_acquire_fails_while_first_held() {
        let td = TempDir::new().unwrap();
        let _l1 = DirLock::acquire(td.path()).unwrap();
        let err = DirLock::acquire(td.path()).unwrap_err();
        assert!(format!("{err}").contains("already locked"));
    }

    #[test]
    fn drop_releases_lock() {
        let td = TempDir::new().unwrap();
        {
            let _l1 = DirLock::acquire(td.path()).unwrap();
        }
        // First lock has been dropped, so we can acquire again immediately.
        let _l2 = DirLock::acquire(td.path()).unwrap();
    }

    #[test]
    fn stale_lock_file_is_not_a_problem() {
        // Simulate a crashed previous run: lock file exists on disk, no fd
        // holds the kernel lock. We must still be able to acquire.
        let td = TempDir::new().unwrap();
        let path = td.path().join(LOCK_FILENAME);
        std::fs::write(path, b"12345\n").unwrap();
        let _l = DirLock::acquire(td.path()).unwrap();
    }
}
