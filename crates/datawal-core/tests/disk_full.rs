//! `ENOSPC` simulation against the writer-poisoning contract.
//!
//! This test exercises the *real* I/O-driven poisoning path of
//! [`RecordLog::append_record`]: when the active segment cannot be
//! extended (because the underlying filesystem refuses the write),
//! the live handle must transition to the poisoned state and the
//! caller-visible error must follow the documented format.
//!
//! Strategy (Linux, non-root):
//!
//! 1. Pick a small `tmpfs` we can fill: `/dev/shm` is mounted as
//!    `tmpfs` on every standard Linux container and CI runner, so
//!    its free space is bounded and controllable.
//! 2. Open a `RecordLog` inside a fresh subdirectory of `/dev/shm`.
//! 3. Append small `Put` frames in a loop until the underlying
//!    `write_all` fails. `tmpfs` returns `ENOSPC` when full; that is
//!    exactly the production failure we care about.
//! 4. Assert the resulting error is the documented poison error.
//! 5. Assert the live handle reports `is_poisoned() == true` and
//!    that subsequent mutating calls keep returning the same error.
//! 6. Drop the handle, free space (delete the segment files), and
//!    reopen: the longest-valid-prefix recovery must succeed and
//!    the new handle must be writable again.
//!
//! Why not `setrlimit(RLIMIT_FSIZE)` instead? It would let us hit
//! the same partial-write code path without a tmpfs, but it requires
//! a `libc` dependency we don't otherwise need, and the failure
//! signal arrives via a `SIGXFSZ` signal that complicates the test
//! harness. The tmpfs approach is portable across Linux distros and
//! container runners, and uses only stdlib + tempfile.
//!
//! Skipped automatically when `/dev/shm` is missing or unwritable
//! (e.g. on macOS or restricted CI sandboxes). The test logs the
//! reason so an absent run is distinguishable from a silent skip.
//!
//! **Manual disk-full reproduction.** For a stronger signal on
//! pre-release rehearsals, an operator can mount a dedicated 1 MiB
//! `tmpfs` (`mount -t tmpfs -o size=1M tmpfs /mnt/dw-full`),
//! `DATAWAL_DISK_FULL_DIR=/mnt/dw-full cargo test --test disk_full --
//! --ignored`. The `manual_disk_full` test below runs only when that
//! env var is set, and uses the same assertions against the user-
//! supplied path.

#![cfg(target_os = "linux")]

use std::fs;
use std::path::{Path, PathBuf};

use datawal::format::RecordType;
use datawal::RecordLog;

/// Probe whether `/dev/shm` is usable for this test on the current
/// host. Returns `Some(path)` to a fresh subdirectory we own, or
/// `None` if the test should be skipped.
fn shm_test_dir() -> Option<PathBuf> {
    let shm = Path::new("/dev/shm");
    if !shm.is_dir() {
        eprintln!("skip: /dev/shm not present");
        return None;
    }
    // Probe writability without using `tempfile::TempDir::into_path`
    // (deprecated in newer tempfile versions). Build a unique
    // subdirectory under `/dev/shm` ourselves; on failure we treat
    // the test as skipped rather than failed.
    let unique = format!(
        "datawal-disk-full-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let probe = shm.join(unique);
    if let Err(e) = fs::create_dir_all(&probe) {
        eprintln!("skip: /dev/shm not writable: {e}");
        return None;
    }
    Some(probe)
}

/// Drive the writer until something fails. Returns `(records_written,
/// final_error_message)`.
fn append_until_failure(log: &mut RecordLog) -> (u64, String) {
    // 64 KiB payload keeps each frame compact but still meaningful
    // against a typical 64 MiB tmpfs.
    let payload = vec![0u8; 64 * 1024];
    let mut count: u64 = 0;
    loop {
        let key = format!("k{count:010}");
        match log.append_record(RecordType::Put, key.as_bytes(), &payload) {
            Ok(_) => {
                count += 1;
                // Safety net so a misconfigured runner with a huge
                // tmpfs doesn't run forever.
                if count > 1_000_000 {
                    panic!(
                        "wrote 1M records ({} GiB) without ENOSPC; \
                         is the tmpfs too large for this test?",
                        count * 64 / (1024 * 1024)
                    );
                }
            }
            Err(e) => return (count, format!("{e:#}")),
        }
    }
}

#[test]
fn enospc_on_tmpfs_poisons_writer() {
    let Some(dir) = shm_test_dir() else { return };

    let mut log = RecordLog::open(&dir).expect("open log on tmpfs");
    assert!(!log.is_poisoned(), "fresh log must not be poisoned");

    let (count, msg) = append_until_failure(&mut log);
    // Even on a tight tmpfs we expect at least one successful
    // append; otherwise the test isn't really exercising recovery
    // from a partial-write boundary.
    assert!(
        count >= 1,
        "no record was written before failure; tmpfs too small? msg={msg}"
    );

    // The handle must now be poisoned.
    assert!(
        log.is_poisoned(),
        "writer must be poisoned after I/O failure"
    );

    // Subsequent mutating calls must keep returning the documented
    // poison error.
    let err = log
        .append_record(RecordType::Put, b"after", b"after")
        .expect_err("post-poison append must fail");
    let post_msg = format!("{err:#}");
    assert!(
        post_msg.starts_with("datawal: writer poisoned: "),
        "post-poison message must use the documented prefix; got: {post_msg}"
    );
    assert!(
        post_msg.ends_with("; drop handle and reopen"),
        "post-poison message must use the documented suffix; got: {post_msg}"
    );

    // Read-only methods stay available.
    let report = log.recovery_report().expect("recovery_report after poison");
    assert_eq!(report.files_scanned, 1);

    // Drop the handle, free space by removing every segment file in
    // place, and reopen. Reopen must use longest-valid-prefix
    // recovery on the (now-empty) directory and start fresh.
    drop(log);
    for entry in fs::read_dir(&dir).expect("read tmpfs dir") {
        let entry = entry.unwrap();
        if entry.path().extension().and_then(|s| s.to_str()) == Some("dwal") {
            let _ = fs::remove_file(entry.path());
        }
    }

    let mut log2 = RecordLog::open(&dir).expect("reopen after cleanup");
    assert!(!log2.is_poisoned(), "reopen must clear poison");
    log2.append_record(RecordType::Put, b"fresh", b"v")
        .expect("write must work on the reopened handle");

    // Best-effort cleanup; the tempdir was extracted via into_path.
    let _ = fs::remove_dir_all(&dir);
}

/// Manual / operator-driven disk-full test.
///
/// Runs only when `DATAWAL_DISK_FULL_DIR` points to a writable
/// directory backed by a deliberately-small filesystem (e.g. a
/// dedicated 1 MiB tmpfs mount). Same assertions as the automatic
/// variant. `#[ignore]` so it never runs in CI by default.
#[test]
#[ignore]
fn manual_disk_full() {
    let Ok(dir) = std::env::var("DATAWAL_DISK_FULL_DIR") else {
        eprintln!("DATAWAL_DISK_FULL_DIR not set; skipping");
        return;
    };
    let dir = PathBuf::from(dir);
    assert!(dir.is_dir(), "DATAWAL_DISK_FULL_DIR must be a directory");

    let mut log = RecordLog::open(&dir).expect("open log on small fs");
    let (count, msg) = append_until_failure(&mut log);
    assert!(count >= 1, "no records written; msg={msg}");
    assert!(log.is_poisoned(), "writer must be poisoned");
    let err = log
        .append_record(RecordType::Put, b"x", b"y")
        .expect_err("post-poison must fail");
    let post = format!("{err:#}");
    assert!(post.starts_with("datawal: writer poisoned: "));
    assert!(post.ends_with("; drop handle and reopen"));
}
