//! Multi-process contention test for the `RecordLog` cooperative lock.
//!
//! `RecordLog::open` takes an advisory `fs2` exclusive lock on the
//! `.lock` file in the data directory. Within a single process this
//! is checked by the existing `record_log.rs` integration tests; this
//! file checks that the lock survives a **process** boundary.
//!
//! Strategy:
//!
//! - The parent test creates a tempdir, then spawns the same test
//!   binary as a child with `DATAWAL_TEST_LOCK_HOLDER=<dir>` set.
//! - The child detects the env var, opens the log in that directory
//!   (taking the lock), prints `LOCK_HELD` to stdout, and sleeps.
//! - The parent waits for the `LOCK_HELD` line, then attempts its
//!   own `RecordLog::open` on the same directory. This call must
//!   fail with an error message that mentions locking.
//! - The parent SIGKILLs the child (`Child::kill()` on Unix). The
//!   OS drops the advisory lock when the file handle closes.
//! - The parent retries `RecordLog::open` in a short loop. Within a
//!   bounded number of attempts the lock must be acquirable again.
//!
//! Unix-only (`fs2` advisory locking semantics are POSIX). Skipping
//! on Windows mirrors the precedent set by `crash_injection.rs`.

#![cfg(unix)]

use std::env;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use datawal::RecordLog;

const HOLD_ENV: &str = "DATAWAL_TEST_LOCK_HOLDER";

/// Child role: take the lock, announce, sleep until killed.
///
/// Called from `main`-like context (a `#[test]` that early-returns
/// when the env var is present). Stays alive via `thread::sleep` so
/// the parent has a deterministic window to attempt a contended
/// open. The parent always SIGKILLs; this function only returns if
/// the kill fails (which would itself be a test failure on the
/// parent side).
fn child_run_lock_holder(dir: PathBuf) -> ! {
    let _log = RecordLog::open(&dir).expect("child: open + take lock");
    // Single line marker the parent grep'd for.
    println!("LOCK_HELD");
    // Best-effort to flush; on Unix stdout to a piped child is
    // line-buffered, but be explicit.
    use std::io::Write;
    std::io::stdout().flush().ok();
    // Sleep generously — parent will SIGKILL well before this.
    thread::sleep(Duration::from_secs(60));
    // Unreachable in normal runs.
    std::process::exit(0);
}

/// Parent role: spawn self with the env var set.
fn spawn_lock_holder(dir: &std::path::Path) -> std::process::Child {
    let exe = env::current_exe().expect("locate test binary");
    Command::new(exe)
        // `--exact` so we only run *this* test in the child and don't
        // recursively spawn more children for other tests.
        .args([
            "--exact",
            "lock_is_contended_across_processes",
            "--nocapture",
        ])
        .env(HOLD_ENV, dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn child")
}

#[test]
fn lock_is_contended_across_processes() {
    // Child branch: detect env var and never return.
    if let Ok(dir) = env::var(HOLD_ENV) {
        child_run_lock_holder(PathBuf::from(dir));
    }

    // Parent branch.
    let tmp = tempfile::tempdir().expect("tempdir");

    // Make sure the directory exists before the child tries to open;
    // `RecordLog::open` creates it if missing, but we want to be sure
    // we are racing against the same canonical path.
    std::fs::create_dir_all(tmp.path()).unwrap();

    let mut child = spawn_lock_holder(tmp.path());
    let child_stdout = child.stdout.take().expect("child stdout");

    // Wait for the `LOCK_HELD` line on a background thread (or
    // timeout). Using a channel keeps the wait bounded.
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let reader_handle = thread::spawn(move || {
        let reader = BufReader::new(child_stdout);
        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(_) => return,
            };
            if line.contains("LOCK_HELD") {
                let _ = tx.send(());
                return;
            }
        }
    });

    rx.recv_timeout(Duration::from_secs(10))
        .expect("child must report LOCK_HELD within 10s");

    // Now the child holds the lock. Our `open` must fail.
    let err = RecordLog::open(tmp.path())
        .expect_err("contended open must fail while child holds the lock");
    let msg = format!("{err:#}");
    // Don't pin the exact wording (fs2 / OS messages vary), but the
    // top-level context comes from `lock.rs` and mentions locking.
    let lower = msg.to_lowercase();
    assert!(
        lower.contains("lock") || lower.contains("locked"),
        "expected a lock-related error, got: {msg}"
    );

    // Kill the child. SIGKILL on Unix; the OS drops the advisory
    // lock when the file handle closes during process teardown.
    child.kill().expect("kill child");
    let _ = child.wait();
    let _ = reader_handle.join();

    // The lock should become acquirable again within a bounded
    // number of attempts. Filesystems and the kernel both need a
    // moment to release the advisory lock.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match RecordLog::open(tmp.path()) {
            Ok(_log) => break,
            Err(e) => {
                if Instant::now() > deadline {
                    panic!("lock never became acquirable after child kill; last error: {e:#}");
                }
                thread::sleep(Duration::from_millis(50));
            }
        }
    }
}
