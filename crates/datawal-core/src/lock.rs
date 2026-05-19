//! Best-effort cooperative lock on a log directory.
//!
//! v0.1-pre uses a sentinel file `path/.lock` created with `O_CREAT | O_EXCL`
//! (`OpenOptions::create_new`). This is **advisory** and intra-machine:
//!
//! - It prevents two concurrent `RecordLog::open` calls on the same directory
//!   when both processes / threads run on the same filesystem and respect the
//!   lock file.
//! - It does **not** protect against stale lock files left behind by a crash;
//!   `open()` will refuse to proceed and surface a clear error. Manual removal
//!   of `.lock` is required to recover. v0.1-pre intentionally trades
//!   automatic stale-lock detection for simplicity.
//! - It does **not** provide flock/fcntl-style OS-level enforcement. A process
//!   that bypasses the lock file (e.g. by deleting it) can still corrupt the
//!   log.
//! - It does **not** work across NFS / network filesystems where create_new is
//!   not guaranteed atomic. v0.1-pre assumes a local POSIX filesystem.
//!
//! TODO(v0.2): integrate `fs2`/`fd-lock` for OS-level advisory locks and
//! pid-based stale detection. Tracked in `docs/technical-decisions.md`.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

/// Name of the sentinel lock file inside a log directory.
pub const LOCK_FILENAME: &str = ".lock";

/// RAII handle that removes the sentinel lock file on drop.
#[derive(Debug)]
pub struct DirLock {
    path: PathBuf,
}

impl DirLock {
    /// Attempt to acquire the lock on `dir`. Fails if the sentinel file
    /// already exists.
    ///
    /// Writes the current pid into the lock file as a diagnostic. Does not
    /// trust the pid (no liveness check).
    pub fn acquire(dir: &Path) -> Result<Self> {
        fs::create_dir_all(dir)
            .with_context(|| format!("datawal: create_dir_all {}", dir.display()))?;
        let path = dir.join(LOCK_FILENAME);
        let mut f: File = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::AlreadyExists => anyhow!(
                    "datawal: log directory is already locked: {} (delete manually if you are \
                     sure no other process is using it)",
                    path.display()
                ),
                _ => anyhow!("datawal: failed to acquire lock {}: {}", path.display(), e),
            })?;
        let pid = std::process::id();
        // Best-effort write; failure to write the pid is not fatal.
        let _ = writeln!(f, "{pid}");
        let _ = f.sync_all();
        Ok(Self { path })
    }

    /// Path of the sentinel file. For diagnostics.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for DirLock {
    fn drop(&mut self) {
        // Best-effort. If removal fails (filesystem error, manual deletion,
        // etc.) the next `acquire` will see a stale lock and surface a clear
        // error rather than silently overwriting.
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn acquire_then_release() {
        let td = TempDir::new().unwrap();
        {
            let _l = DirLock::acquire(td.path()).unwrap();
            assert!(td.path().join(LOCK_FILENAME).exists());
        }
        assert!(!td.path().join(LOCK_FILENAME).exists());
    }

    #[test]
    fn second_acquire_fails() {
        let td = TempDir::new().unwrap();
        let _l1 = DirLock::acquire(td.path()).unwrap();
        let err = DirLock::acquire(td.path()).unwrap_err();
        assert!(format!("{err}").contains("already locked"));
    }
}
