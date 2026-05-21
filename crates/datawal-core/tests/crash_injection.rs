//! Crash injection tests for `RecordLog` and `DataWal`.
//!
//! Each test spawns the same test binary as a child with
//! `DATAWAL_CRASH_CHILD=<scenario>` set in the environment. The child
//! detects this env var at the top of the corresponding `#[test]`,
//! runs a loop in the shared temp directory, prints progress lines
//! to `stdout` after every observable boundary, and waits to be
//! killed.
//!
//! The parent:
//!
//! 1. Creates a fresh tempdir per attempt (and optionally seeds it);
//! 2. Spawns the child with `--exact <test_name>` so only that test
//!    runs in the child process (other tests must short-circuit if
//!    they see `DATAWAL_CRASH_CHILD`);
//! 3. Reads `stdout` line-by-line on a background thread, keeping the
//!    highest `<verb> <i>` it observed in memory;
//! 4. Sleeps a small deterministic interval (different per attempt)
//!    to let the child make progress;
//! 5. Sends SIGKILL via `Child::kill()` (Unix POSIX semantics);
//! 6. Waits for the child to exit, joins the reader thread;
//! 7. Reopens the relevant log / directory and asserts the
//!    scenario's invariants.
//!
//! Scenarios:
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
//! - `rotate`: child appends, fsyncs, and rotates the segment every
//!   `ROTATE_EVERY` records. Parent invariants: reopen does not
//!   panic, recovered records are a contiguous prefix `0..n`, and
//!   `n` is at least `last_observed_appended + 1` (because every
//!   `appended <i>` is emitted strictly after `fsync()` returned).
//!   This exercises kills around the create-new-segment + fsync_dir
//!   window of `RecordLog::rotate`.
//!
//! - `compact_to`: parent seeds a source `DataWal` with a deterministic
//!   key set, drops it, then the child repeatedly opens the source,
//!   runs `DataWal::compact_to(out_dir)` into a per-iteration
//!   subdirectory, and removes that subdirectory before the next
//!   iteration. Parent invariants after SIGKILL: the source
//!   directory is byte-for-byte unchanged (compaction is
//!   snapshot-style), and any output directory that survives can
//!   either be opened and scanned without panic, or is empty.
//!
//! - `export_jsonl`: parent seeds a source `DataWal`, drops it, then
//!   the child repeatedly opens the source and calls
//!   `DataWal::export_jsonl(out_path)`. `export_jsonl` uses
//!   `safeatomic_rs::write_atomic`, so the only externally visible
//!   states are "file absent" or "file complete and parseable".
//!   Parent invariant: if the file exists after SIGKILL, every line
//!   is a parseable JSONL row with the original key set; otherwise
//!   the file is absent. The source is unchanged either way.
//!
//! Unix-only. `Child::kill` on Windows terminates the process but
//! does not correspond to SIGKILL semantics; revisit when there is a
//! Windows CI lane.

#![cfg(unix)]

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use datawal::{DataWal, RecordLog};
use serde::Deserialize;
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

/// Maximum number of iterations the child will perform. Acts as a
/// safety cap so the child does not run unbounded if the parent
/// fails to kill it for any reason.
const MAX_RECORDS: u64 = 100_000;

/// Number of records appended between rotations in the `rotate`
/// scenario. Small enough that the schedule above straddles many
/// rotation boundaries.
const ROTATE_EVERY: u64 = 4;

/// Number of keys planted in the seeded `DataWal` source used by
/// `compact_to` and `export_jsonl` scenarios. Large enough to make
/// each compaction/export take measurable time, small enough to keep
/// the test fast.
const SEED_KEYS: u64 = 200;

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
        "rotate" => child_rotate(Path::new(&dir)),
        "compact_to" => child_compact_to(Path::new(&dir)),
        "export_jsonl" => child_export_jsonl(Path::new(&dir)),
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

/// Child: append + fsync + announce, and rotate every `ROTATE_EVERY`
/// records. `appended <i>` is printed strictly after `fsync()`
/// returned, so the parent's durability oracle still applies.
/// `rotated` lines carry no semantic load for the parent invariants
/// but are useful when debugging traces.
fn child_rotate(dir: &Path) -> ! {
    let mut log = RecordLog::open(dir).expect("child: RecordLog::open");
    let mut stdout = std::io::stdout().lock();
    for i in 0..MAX_RECORDS {
        let payload = make_payload(i);
        log.append(&payload).expect("child: append");
        log.fsync().expect("child: fsync");
        // Print durability-after-fsync before rotating so the parent
        // never observes an `appended <i>` for a record that has not
        // been fsynced.
        let _ = writeln!(stdout, "appended {i}");
        let _ = stdout.flush();
        if (i + 1) % ROTATE_EVERY == 0 {
            log.rotate().expect("child: rotate");
            let _ = writeln!(stdout, "rotated");
            let _ = stdout.flush();
        }
    }
    panic!("child: rotate exceeded MAX_RECORDS without being killed");
}

/// Child: in a tight loop, open the seeded source `DataWal`, compact
/// it into a fresh per-iteration subdirectory, then remove that
/// subdirectory. The point is to maximise the chance of being killed
/// somewhere inside `compact_to`. The seeded source is created by
/// the parent before spawning so the child doesn't pay the seed cost
/// on every iteration.
///
/// `dir` is the *parent of the source*, with the source itself
/// living at `dir/source` and per-iteration outputs at
/// `dir/out_<i>`.
fn child_compact_to(dir: &Path) -> ! {
    let source = dir.join("source");
    let mut stdout = std::io::stdout().lock();
    for i in 0..MAX_RECORDS {
        let out_dir = dir.join(format!("out_{i}"));
        // Each iteration: open, compact, drop, remove. Open takes a
        // fresh fs2 lock; SIGKILL during the loop releases the lock
        // automatically.
        let mut kv = DataWal::open(&source).expect("child: open source");
        kv.compact_to(&out_dir).expect("child: compact_to");
        drop(kv);
        let _ = writeln!(stdout, "compacted {i}");
        let _ = stdout.flush();
        // Remove the output so the next iteration's `compact_to`
        // sees an empty target (it refuses non-empty targets).
        // A crash *between* the announce above and this remove is
        // legitimate and the parent must tolerate finding the
        // directory on disk.
        let _ = fs::remove_dir_all(&out_dir);
    }
    panic!("child: compact_to exceeded MAX_RECORDS without being killed");
}

/// Child: in a tight loop, open the seeded source `DataWal` and
/// export to a fresh per-iteration JSONL path. The file lives in
/// the same parent dir as the source so all I/O is on the same
/// filesystem. The export uses `safeatomic_rs::write_atomic` under
/// the hood, so the only externally visible states are "missing" or
/// "complete".
fn child_export_jsonl(dir: &Path) -> ! {
    let source = dir.join("source");
    let mut stdout = std::io::stdout().lock();
    for i in 0..MAX_RECORDS {
        let out = dir.join(format!("export_{i}.jsonl"));
        let mut kv = DataWal::open(&source).expect("child: open source");
        kv.export_jsonl(&out).expect("child: export_jsonl");
        drop(kv);
        let _ = writeln!(stdout, "exported {i}");
        let _ = stdout.flush();
        // Remove the output to bound disk usage. As with compact_to,
        // a crash between announce and removal is legitimate.
        let _ = fs::remove_file(&out);
    }
    panic!("child: export_jsonl exceeded MAX_RECORDS without being killed");
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

/// Outcome of a single kill/reopen attempt for the append-style
/// scenarios.
struct Outcome {
    /// Highest `i` the parent observed on stdout before issuing
    /// `kill()`. For `append_no_fsync` this is "appended <i>"; for
    /// `append_fsync` and `rotate` this is "appended <i>" printed
    /// after fsync. `None` if the child was killed before producing
    /// any output (legitimate at very short sleeps).
    last_observed: Option<u64>,
    /// Records actually recovered from the log on reopen, ordered as
    /// they appear across all segments.
    recovered_indices: Vec<u64>,
}

/// Spawn the child for `test_name` with `scenario` in the env, sleep
/// `sleep`, kill it, reopen the log and return the outcome. Used by
/// `append_no_fsync`, `append_fsync` and `rotate`.
fn run_attempt(test_name: &str, scenario: &str, dir: &Path, sleep: Duration) -> Outcome {
    let (last_observed, _events) = run_child(test_name, scenario, dir, sleep);

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

/// Spawn the child, sleep, kill, and return `(last_observed_index,
/// total_announce_lines)`. Does not reopen anything; callers do
/// scenario-specific recovery and assertions.
fn run_child(test_name: &str, scenario: &str, dir: &Path, sleep: Duration) -> (Option<u64>, u64) {
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
    let mut count: u64 = 0;
    while let Ok(i) = rx.try_recv() {
        count += 1;
        last_observed = Some(last_observed.map(|prev| prev.max(i)).unwrap_or(i));
    }

    (last_observed, count)
}

/// Parse a `<verb> <i>` progress line into `i`. Recognises every
/// verb a child handler can emit (`appended`, `fsynced`, `compacted`,
/// `exported`). Returns `None` for non-matching lines (e.g. the
/// `rotated` marker, which carries no `i`).
fn parse_progress_line(line: &str) -> Option<u64> {
    for prefix in ["appended ", "fsynced ", "compacted ", "exported "] {
        if let Some(rest) = line.strip_prefix(prefix) {
            return rest.trim().parse::<u64>().ok();
        }
    }
    None
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

/// Seed a `DataWal` at `source` with a deterministic, sorted key set.
/// Used by the `compact_to` and `export_jsonl` scenarios. Returns the
/// canonical key/value map for later equality checks.
fn seed_source(source: &Path) -> BTreeMap<Vec<u8>, Vec<u8>> {
    let mut kv = DataWal::open(source).expect("parent: seed open");
    let mut expected = BTreeMap::new();
    for i in 0..SEED_KEYS {
        let key = format!("key-{i:08}").into_bytes();
        let value = make_payload(i);
        kv.put(&key, &value).expect("parent: seed put");
        expected.insert(key, value);
    }
    kv.fsync().expect("parent: seed fsync");
    drop(kv);
    expected
}

/// Hash the byte contents of every `*.dwal` file in `dir`. Used to
/// assert that `compact_to` leaves the source bit-for-bit untouched.
fn segment_digest(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let mut out: Vec<(String, Vec<u8>)> = Vec::new();
    for entry in fs::read_dir(dir).expect("parent: read_dir source") {
        let entry = entry.expect("parent: dir entry");
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if !name.ends_with(".dwal") {
            continue;
        }
        let bytes = fs::read(&path).expect("parent: read segment");
        out.push((name.to_string(), bytes));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Parse one JSONL export line into `(key, value)` using the same
/// schema as `DataWal::export_jsonl`.
#[derive(Deserialize)]
struct ExportRow {
    key_b64: String,
    value_b64: String,
}

/// Parse a JSONL file written by `export_jsonl`. Returns the parsed
/// rows in encounter order; the caller can compare to the expected
/// map. Panics on any malformed line (the export contract promises
/// only "absent" or "complete and parseable" outputs).
fn parse_jsonl(path: &Path) -> Vec<(Vec<u8>, Vec<u8>)> {
    let text = fs::read_to_string(path).expect("parent: read jsonl");
    let mut out = Vec::new();
    for (lineno, line) in text.lines().enumerate() {
        let row: ExportRow = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("parent: jsonl line {lineno} not valid json: {e}: {line}"));
        let key = B64
            .decode(row.key_b64.as_bytes())
            .unwrap_or_else(|e| panic!("parent: jsonl line {lineno} bad key_b64: {e}"));
        let value = B64
            .decode(row.value_b64.as_bytes())
            .unwrap_or_else(|e| panic!("parent: jsonl line {lineno} bad value_b64: {e}"));
        out.push((key, value));
    }
    out
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

#[test]
fn crash_rotate() {
    if is_child("rotate") {
        return;
    }
    for (attempt, &ms) in SLEEPS_MS.iter().enumerate() {
        let tmp = TempDir::new().expect("parent: tempdir");
        let outcome = run_attempt(
            "crash_rotate",
            "rotate",
            tmp.path(),
            Duration::from_millis(ms),
        );

        // Invariant 1: recovered records form a contiguous prefix
        // across all segments. `RecordLog::scan` walks segments in
        // order so the indices must come out as `0..n`.
        let recovered_n = assert_contiguous_prefix(
            &outcome.recovered_indices,
            &format!("rotate attempt {attempt} (sleep {ms}ms)"),
        );

        // Invariant 2: durability oracle still holds. Every
        // `appended <i>` the child printed was emitted strictly
        // after `log.fsync()` returned (the print comes after fsync
        // in the child loop), so reopen must recover at least i+1
        // records.
        if let Some(observed) = outcome.last_observed {
            assert!(
                recovered_n > observed,
                "rotate attempt {attempt} (sleep {ms}ms): parent observed appended {observed} \
                 but only recovered {recovered_n} records (need at least {})",
                observed + 1
            );
        }

        let observed = outcome.last_observed.unwrap_or(0);
        eprintln!(
            "rotate attempt={attempt} sleep={ms}ms observed_appended={observed} \
             recovered_n={recovered_n}"
        );
    }
}

#[test]
fn crash_compact_to() {
    if is_child("compact_to") {
        return;
    }
    for (attempt, &ms) in SLEEPS_MS.iter().enumerate() {
        let tmp = TempDir::new().expect("parent: tempdir");
        let source = tmp.path().join("source");
        fs::create_dir(&source).expect("parent: mkdir source");
        let expected = seed_source(&source);
        let pre_digest = segment_digest(&source);

        let (last_observed, events) = run_child(
            "crash_compact_to",
            "compact_to",
            tmp.path(),
            Duration::from_millis(ms),
        );

        // Invariant 1: the source directory is byte-for-byte
        // unchanged. `compact_to` is a snapshot-style read of the
        // source; it must not mutate it.
        let post_digest = segment_digest(&source);
        assert_eq!(
            pre_digest, post_digest,
            "compact_to attempt {attempt} (sleep {ms}ms): source mutated by compaction"
        );

        // Invariant 2: the source still opens, scans, and projects
        // to the original key set. This catches subtler corruption
        // that segment_digest might miss (e.g. a lock-file artefact
        // we don't account for would be caught by the digest, but
        // we also want to verify the projection is correct).
        let mut kv = DataWal::open(&source).expect("parent: reopen source");
        let mut actual: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
        for k in expected.keys() {
            let v = kv
                .get(k)
                .expect("parent: get from source")
                .unwrap_or_else(|| panic!("parent: source lost key {k:?}"));
            actual.insert(k.clone(), v);
        }
        assert_eq!(
            actual.len(),
            expected.len(),
            "compact_to attempt {attempt} (sleep {ms}ms): unexpected key count after reopen"
        );
        assert_eq!(
            actual, expected,
            "compact_to attempt {attempt} (sleep {ms}ms): source projection changed"
        );
        drop(kv);

        // Invariant 3: any leftover `out_<i>` directories must
        // either open cleanly (possibly with a truncated tail) or
        // be empty. They are scratch output of an interrupted
        // compaction; the worst they can be is a partial log, never
        // a panic source.
        let mut leftovers: Vec<PathBuf> = Vec::new();
        for entry in fs::read_dir(tmp.path()).expect("parent: list tmpdir") {
            let entry = entry.expect("parent: dir entry");
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if name.starts_with("out_") {
                leftovers.push(entry.path());
            }
        }
        for out_dir in &leftovers {
            // Empty dir is fine.
            let is_empty = fs::read_dir(out_dir)
                .expect("parent: read_dir leftover")
                .next()
                .is_none();
            if is_empty {
                continue;
            }
            // Non-empty: must open without panic. Truncated tail is
            // legitimate; CRC mismatch in a *sealed* segment would
            // be a hard error, but compact_to only writes into a
            // single active segment per call so there are no sealed
            // segments to worry about.
            let mut leftover_log =
                RecordLog::open(out_dir).expect("parent: reopen leftover compact_to output");
            let _ = leftover_log
                .scan()
                .expect("parent: scan leftover compact_to output");
        }

        eprintln!(
            "compact_to attempt={attempt} sleep={ms}ms observed_compacted={:?} events={events} \
             leftovers={}",
            last_observed,
            leftovers.len()
        );
    }
}

#[test]
fn crash_export_jsonl() {
    if is_child("export_jsonl") {
        return;
    }
    for (attempt, &ms) in SLEEPS_MS.iter().enumerate() {
        let tmp = TempDir::new().expect("parent: tempdir");
        let source = tmp.path().join("source");
        fs::create_dir(&source).expect("parent: mkdir source");
        let expected = seed_source(&source);
        let pre_digest = segment_digest(&source);

        let (last_observed, events) = run_child(
            "crash_export_jsonl",
            "export_jsonl",
            tmp.path(),
            Duration::from_millis(ms),
        );

        // Invariant 1: source is byte-for-byte unchanged. Export is
        // a pure read.
        let post_digest = segment_digest(&source);
        assert_eq!(
            pre_digest, post_digest,
            "export_jsonl attempt {attempt} (sleep {ms}ms): source mutated by export"
        );

        // Invariant 2: source still projects to the original key
        // set.
        let mut kv = DataWal::open(&source).expect("parent: reopen source");
        for (k, v) in &expected {
            let got = kv
                .get(k)
                .expect("parent: get from source")
                .unwrap_or_else(|| panic!("parent: source lost key {k:?}"));
            assert_eq!(
                &got, v,
                "export_jsonl attempt {attempt} (sleep {ms}ms): source projection changed for key {k:?}"
            );
        }
        drop(kv);

        // Invariant 3: any leftover `export_<i>.jsonl` file must be
        // a complete, valid export of the source. `export_jsonl`
        // uses `safeatomic_rs::write_atomic`, which renames into
        // place after the temp file is fully written and fsynced,
        // so a partially-written file is never visible at the final
        // path.
        let mut leftovers: Vec<PathBuf> = Vec::new();
        for entry in fs::read_dir(tmp.path()).expect("parent: list tmpdir") {
            let entry = entry.expect("parent: dir entry");
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if name.starts_with("export_") && name.ends_with(".jsonl") {
                leftovers.push(entry.path());
            }
        }
        for path in &leftovers {
            let rows = parse_jsonl(path);
            let got: BTreeMap<Vec<u8>, Vec<u8>> = rows.into_iter().collect();
            assert_eq!(
                got, expected,
                "export_jsonl attempt {attempt} (sleep {ms}ms): leftover {} is not a complete export",
                path.display()
            );
        }

        eprintln!(
            "export_jsonl attempt={attempt} sleep={ms}ms observed_exported={:?} events={events} \
             leftovers={}",
            last_observed,
            leftovers.len()
        );
    }
}
