//! Crash injection tests for `RecordLog` append recovery.
//!
//! Each test spawns the same test binary as a child with
//! `DATAWAL_CRASH_CHILD=<scenario>` set in the environment. The child
//! detects this env var at the top of the corresponding `#[test]`,
//! runs an append loop in the shared temp directory, prints progress
//! lines to `stdout` after every durability boundary, and waits to be
//! killed.
//!
//! The parent:
//!
//! 1. Creates a fresh tempdir per attempt;
//! 2. Spawns the child with `--exact <test_name>` so only that test
//!    runs in the child process (other tests must short-circuit if
//!    they see `DATAWAL_CRASH_CHILD`);
//! 3. Reads `stdout` line-by-line on a background thread, keeping the
//!    highest `fsynced <i>` it observed in memory;
//! 4. Sleeps a small deterministic interval (different per attempt)
//!    to let the child make progress;
//! 5. Sends SIGKILL via `Child::kill()` (Unix POSIX semantics);
//! 6. Waits for the child to exit, joins the reader thread;
//! 7. Reopens the `RecordLog` in the same directory and asserts the
//!    scenario's invariants.
//!
//! Scenarios implemented in this PR (per issue #8, initial scope):
//!
//! - `append_no_fsync`: child appends in a tight loop without
//!   `fsync`. Parent invariant: reopen does not panic, scan returns
//!   a valid prefix, recovered records form a contiguous sequence
//!   `0..n` for some `n`. The tail may be lost; that is acceptable.
//!
//! - `append_fsync`: child appends and `fsync`s every record, then
//!   prints `fsynced <i>`. Parent invariant: reopen does not panic,
//!   scan returns a valid prefix, every `i` the parent observed via
//!   stdout *before* the kill is present in the recovered log. The
//!   recovered log may extend past the last observed `i` (the child
//!   may have fsynced and queued the line but not yet flushed
//!   stdout); it must never fall short of it.
//!
//! Out of scope here, deferred to follow-up PRs:
//!
//! - `rotate` kill/reopen
//! - `compact_to` kill/reopen
//! - `export_jsonl` kill/reopen
//!
//! Unix-only. `Child::kill` on Windows terminates the process but
//! does not correspond to SIGKILL semantics; revisit when there is a
//! Windows CI lane.

#![cfg(unix)]

use std::env;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use datawal::RecordLog;
use tempfile::TempDir;

/// Number of kill/reopen attempts per scenario. Each attempt uses a
/// different sleep, giving coverage across various crash points.
const ATTEMPTS: usize = 8;

/// Sleep schedule before sending SIGKILL, in milliseconds. The values
/// are chosen to span "before any write completes", "in the middle of
/// the loop", and "after many writes have been fsynced". Adjust if
/// the test grows flaky on slow CI machines.
const SLEEPS_MS: [u64; ATTEMPTS] = [1, 3, 8, 15, 25, 40, 60, 100];

/// Payload size in bytes. Small enough to keep append+fsync rates
/// high so the schedule above touches many records.
const PAYLOAD_SIZE: usize = 64;

/// Maximum number of records the child will append. Acts as a safety
/// cap so the child does not run unbounded if the parent fails to
/// kill it for any reason.
const MAX_RECORDS: u64 = 100_000;

// ---------------------------------------------------------------
// Child entry point detection
// ---------------------------------------------------------------

/// If `DATAWAL_CRASH_CHILD` is set, dispatch to the matching child
/// handler. Returns `true` if the current process was a child (which
/// means it has either exited or is looping forever; callers should
/// abort or short-circuit on `true`). Returns `false` if the process
/// is the parent and the test should proceed normally.
fn is_child(expected: &str) -> bool {
    let Ok(mode) = env::var("DATAWAL_CRASH_CHILD") else {
        return false;
    };
    if mode != expected {
        // Another scenario's child landed in this test by mistake
        // (should not happen with `--exact` but be defensive).
        return true;
    }
    let dir = env::var("DATAWAL_CRASH_DIR")
        .expect("child: DATAWAL_CRASH_DIR must be set alongside DATAWAL_CRASH_CHILD");
    // Each child handler diverges (`-> !`); this `match` therefore
    // does not return. Reaching the end of `is_child` after a
    // dispatch is impossible.
    match mode.as_str() {
        "append_no_fsync" => child_append_no_fsync(Path::new(&dir)),
        "append_fsync" => child_append_fsync(Path::new(&dir)),
        other => panic!("child: unknown DATAWAL_CRASH_CHILD mode {other:?}"),
    }
}

// ---------------------------------------------------------------
// Child handlers
// ---------------------------------------------------------------

/// Child: append a deterministic sequence of payloads as fast as
/// possible, without `fsync`. Loop until SIGKILL.
fn child_append_no_fsync(dir: &Path) -> ! {
    let mut log = RecordLog::open(dir).expect("child: RecordLog::open");
    let mut stdout = std::io::stdout().lock();
    for i in 0..MAX_RECORDS {
        let payload = make_payload(i);
        log.append(&payload).expect("child: append");
        // Announce progress *before* fsync so the parent has a notion
        // of how far the child claims to have written, even if those
        // records are still in the page cache. The parent must not
        // treat this as a durability oracle for the no_fsync
        // scenario; that is checked separately in the fsync test.
        let _ = writeln!(stdout, "appended {i}");
        let _ = stdout.flush();
    }
    panic!("child: append_no_fsync exceeded MAX_RECORDS without being killed");
}

/// Child: append, then `fsync`, then print `fsynced <i>`. The print
/// happens strictly after `fsync` returns, so any line the parent
/// reads on stdout corresponds to a record that is durable.
fn child_append_fsync(dir: &Path) -> ! {
    let mut log = RecordLog::open(dir).expect("child: RecordLog::open");
    let mut stdout = std::io::stdout().lock();
    for i in 0..MAX_RECORDS {
        let payload = make_payload(i);
        log.append(&payload).expect("child: append");
        log.fsync().expect("child: fsync");
        // Ordering matters: fsync has already returned before this
        // line is emitted.
        let _ = writeln!(stdout, "fsynced {i}");
        let _ = stdout.flush();
    }
    panic!("child: append_fsync exceeded MAX_RECORDS without being killed");
}

/// Deterministic payload generator. The first 8 bytes encode the
/// record index `i`, so the parent can verify ordering on reopen.
fn make_payload(i: u64) -> Vec<u8> {
    let mut buf = vec![0u8; PAYLOAD_SIZE];
    buf[..8].copy_from_slice(&i.to_le_bytes());
    buf
}

/// Decode a payload's record index. Returns `None` if the payload is
/// too short.
fn payload_index(payload: &[u8]) -> Option<u64> {
    if payload.len() < 8 {
        return None;
    }
    let mut idx = [0u8; 8];
    idx.copy_from_slice(&payload[..8]);
    Some(u64::from_le_bytes(idx))
}

// ---------------------------------------------------------------
// Parent helpers
// ---------------------------------------------------------------

/// Outcome of a single kill/reopen attempt.
struct Outcome {
    /// Highest `i` the parent observed on stdout before issuing
    /// `kill()`. For `append_no_fsync` this is "appended <i>"; for
    /// `append_fsync` this is "fsynced <i>". `None` if the child was
    /// killed before producing any output (legitimate at very short
    /// sleeps).
    last_observed: Option<u64>,
    /// Records actually recovered from the log on reopen, ordered as
    /// they appear in the segment.
    recovered_indices: Vec<u64>,
}

/// Spawn the child for `test_name` with `scenario` in the env, sleep
/// `sleep`, kill it, reopen the log and return the outcome.
fn run_attempt(test_name: &str, scenario: &str, dir: &Path, sleep: Duration) -> Outcome {
    let exe = env::current_exe().expect("parent: current_exe");
    let mut child = Command::new(exe)
        // `--exact` makes the child run only this single test, so it
        // hits our `is_child` short-circuit immediately. `--nocapture`
        // keeps our progress lines visible on stdout. `--quiet`
        // suppresses libtest's own summary noise.
        .args(["--exact", test_name, "--nocapture", "--quiet"])
        .env("DATAWAL_CRASH_CHILD", scenario)
        .env("DATAWAL_CRASH_DIR", dir)
        // Prevent libtest from spawning multiple test threads in the
        // child; the child only runs one test and we want
        // deterministic single-threaded behaviour.
        .env("RUST_TEST_THREADS", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("parent: spawn child");

    let stdout = child.stdout.take().expect("parent: child stdout pipe");
    let (tx, rx) = mpsc::channel::<u64>();
    let reader = thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines().map_while(Result::ok) {
            if let Some(i) = parse_progress_line(&line) {
                // Best-effort send; parent may have dropped rx by the
                // time we get here, which is fine.
                let _ = tx.send(i);
            }
        }
    });

    thread::sleep(sleep);
    let _ = child.kill();
    let _ = child.wait();

    // Drain whatever the reader thread queued. Drop the sender's
    // counterpart implicitly when `reader` finishes (the pipe closes
    // when the child exits, so `lines()` ends).
    reader.join().expect("parent: reader thread");
    let mut last_observed: Option<u64> = None;
    while let Ok(i) = rx.try_recv() {
        last_observed = Some(last_observed.map(|prev| prev.max(i)).unwrap_or(i));
    }

    // Reopen and scan.
    let mut log = RecordLog::open(dir).expect("parent: reopen RecordLog");
    let records = log.scan().expect("parent: scan");
    let recovered_indices = records
        .iter()
        .map(|r| payload_index(&r.payload).expect("parent: payload too short"))
        .collect::<Vec<_>>();

    Outcome {
        last_observed,
        recovered_indices,
    }
}

/// Parse a `appended <i>` or `fsynced <i>` line into `i`. Returns
/// `None` if the line does not match.
fn parse_progress_line(line: &str) -> Option<u64> {
    let rest = line
        .strip_prefix("appended ")
        .or_else(|| line.strip_prefix("fsynced "))?;
    rest.trim().parse::<u64>().ok()
}

/// Assert that `indices` is a contiguous prefix `0..n` for some `n`,
/// then return `n` (or 0 if the slice is empty).
fn assert_contiguous_prefix(indices: &[u64], context: &str) -> u64 {
    for (pos, &i) in indices.iter().enumerate() {
        assert_eq!(
            i, pos as u64,
            "{context}: recovered indices are not a contiguous prefix at position {pos}: \
             got {i}, expected {pos}. full indices: {indices:?}"
        );
    }
    indices.len() as u64
}

// ---------------------------------------------------------------
// Tests
// ---------------------------------------------------------------

#[test]
fn crash_append_no_fsync() {
    if is_child("append_no_fsync") {
        return;
    }
    for (attempt, &ms) in SLEEPS_MS.iter().enumerate() {
        let tmp = TempDir::new().expect("parent: tempdir");
        let outcome = run_attempt(
            "crash_append_no_fsync",
            "append_no_fsync",
            tmp.path(),
            Duration::from_millis(ms),
        );

        // Invariant: recovered records form a contiguous prefix.
        let recovered_n = assert_contiguous_prefix(
            &outcome.recovered_indices,
            &format!("append_no_fsync attempt {attempt} (sleep {ms}ms)"),
        );

        // No durability claim: the recovered count may legitimately
        // be less than what the child printed because `appended <i>`
        // does not imply fsync. We only require that recovery did
        // not panic and that what survived is a valid prefix.
        let observed = outcome.last_observed.unwrap_or(0);
        eprintln!(
            "append_no_fsync attempt={attempt} sleep={ms}ms observed_appended={observed} \
             recovered_n={recovered_n}"
        );
    }
}

#[test]
fn crash_append_fsync() {
    if is_child("append_fsync") {
        return;
    }
    for (attempt, &ms) in SLEEPS_MS.iter().enumerate() {
        let tmp = TempDir::new().expect("parent: tempdir");
        let outcome = run_attempt(
            "crash_append_fsync",
            "append_fsync",
            tmp.path(),
            Duration::from_millis(ms),
        );

        // Invariant 1: recovered records form a contiguous prefix.
        let recovered_n = assert_contiguous_prefix(
            &outcome.recovered_indices,
            &format!("append_fsync attempt {attempt} (sleep {ms}ms)"),
        );

        // Invariant 2: every record the parent observed as `fsynced`
        // *before* the kill must be present after reopen. The child
        // prints strictly after `fsync()` returns, so the parent
        // never sees an `i` whose fsync did not actually happen.
        if let Some(observed) = outcome.last_observed {
            assert!(
                recovered_n > observed,
                "append_fsync attempt {attempt} (sleep {ms}ms): parent observed fsynced {observed} \
                 but only recovered {recovered_n} records (need at least {})",
                observed + 1
            );
        }

        let observed = outcome.last_observed.unwrap_or(0);
        eprintln!(
            "append_fsync attempt={attempt} sleep={ms}ms observed_fsynced={observed} \
             recovered_n={recovered_n}"
        );
    }
}
