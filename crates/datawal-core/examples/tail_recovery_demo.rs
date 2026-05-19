//! Demo: tail-truncation recovery on `RecordLog::open`.
//!
//! Run with:
//!
//! ```text
//! cargo run -p datawal-core --example tail_recovery_demo
//! ```
//!
//! Scenario simulated:
//!
//! 1. Open a fresh `RecordLog`, append three records, `fsync`, then
//!    drop the log so the OS-level advisory lock is released.
//! 2. Simulate a crash mid-write by truncating the last few bytes of
//!    the active segment file on disk. This leaves a partial record at
//!    the end of the segment.
//! 3. Reopen the log. `scan()` must return only the valid prefix
//!    (the first two records), and `recovery_report()` must report
//!    exactly one truncated tail.
//!
//! This illustrates the v0.1.0-alpha recovery contract: the active
//! (last) segment tolerates a truncated tail without error, the
//! tolerated damage is *reported*, and the log file is **not**
//! physically truncated by recovery — the damaged bytes are simply
//! ignored.

use std::fs::OpenOptions;
use std::path::PathBuf;

use anyhow::{Context, Result};
use datawal_core::RecordLog;

fn main() -> Result<()> {
    let dir: PathBuf = std::env::temp_dir().join("datawal-tail-recovery-demo");
    let _ = std::fs::remove_dir_all(&dir);

    println!("== datawal tail_recovery_demo ==");
    println!("log dir: {}", dir.display());

    // Phase 1: write three records and fsync.
    {
        let mut log = RecordLog::open(&dir)?;
        log.append(b"alpha")?;
        log.append(b"beta")?;
        log.append(b"gamma")?;
        log.fsync()?;
        println!("phase 1: appended 3 records, fsync ok");
        // Drop here releases the fs2 fd-based lock.
    }

    // Phase 2: simulate a crash by truncating the last 5 bytes of the
    // active segment file. This leaves a record without its full CRC,
    // i.e. a partial record at the tail.
    let active = dir.join("00000001.dwal");
    let original_len = std::fs::metadata(&active)?.len();
    let truncate_to = original_len.saturating_sub(5);
    OpenOptions::new()
        .write(true)
        .open(&active)
        .context("open active segment for truncation")?
        .set_len(truncate_to)?;
    println!(
        "phase 2: truncated {} from {} bytes -> {} bytes (simulated crash)",
        active.display(),
        original_len,
        truncate_to,
    );

    // Phase 3: reopen and observe recovery.
    let mut log = RecordLog::open(&dir)?;
    let records = log.scan()?;
    let report = log.recovery_report()?;

    println!("phase 3: scan returned {} record(s)", records.len());
    for (i, rec) in records.iter().enumerate() {
        let utf8 = std::str::from_utf8(&rec.payload).unwrap_or("<non-utf8>");
        println!(
            "  [{i}] type={:?} txid={} payload={:?}",
            rec.record_type, rec.txid, utf8,
        );
    }
    println!(
        "recovery_report: files_scanned={} records_replayed={} tail_truncated={} tail_bytes_discarded={} mid_stream_errors={} unsupported_versions={} last_txid_seen={}",
        report.files_scanned,
        report.records_replayed,
        report.tail_truncated,
        report.tail_bytes_discarded,
        report.mid_stream_errors,
        report.unsupported_versions,
        report.last_txid_seen,
    );

    // Sanity checks: exactly two valid records survive; tail damage
    // is reported once. Use asserts so the example is also a smoke
    // test if anyone runs it.
    assert_eq!(
        records.len(),
        2,
        "expected the truncated tail to drop the last record",
    );
    assert_eq!(
        report.tail_truncated, 1,
        "expected exactly one truncated tail",
    );
    assert!(
        report.tail_bytes_discarded > 0,
        "expected some bytes to be discarded",
    );

    log.close()?;
    println!("ok");
    Ok(())
}
