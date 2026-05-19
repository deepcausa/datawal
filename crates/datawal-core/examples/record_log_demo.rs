//! Demo: `RecordLog` open → append → fsync → scan.
//!
//! Run with:
//!
//! ```text
//! cargo run -p datawal-core --example record_log_demo
//! ```
//!
//! This example writes a few raw records to a temporary directory,
//! flushes them to disk, then scans the log back and prints what was
//! read. It is intentionally small: it shows the surface area of
//! `RecordLog` without touching `DataWal`, compaction, or export.

use std::path::PathBuf;

use anyhow::Result;
use datawal_core::RecordLog;

fn main() -> Result<()> {
    // Use a fresh subdirectory under the OS temp dir.
    let dir: PathBuf = std::env::temp_dir().join("datawal-record-log-demo");
    let _ = std::fs::remove_dir_all(&dir);

    println!("== datawal record_log_demo ==");
    println!("log dir: {}", dir.display());

    // Open the log (creates the directory and the first segment).
    let mut log = RecordLog::open(&dir)?;

    // Append 3 raw payloads.
    let payloads: &[&[u8]] = &[b"hello", b"world", b"datawal v0.1.0-alpha"];
    for payload in payloads {
        let r = log.append(payload)?;
        println!(
            "append: segment={} offset={} len={} bytes",
            r.segment, r.offset, r.len,
        );
    }

    // Flush.
    log.fsync()?;
    println!("fsync: ok");

    // Scan the log back.
    let records = log.scan()?;
    println!("scan: {} record(s)", records.len());
    for (i, rec) in records.iter().enumerate() {
        println!(
            "  [{i}] type={:?} txid={} key_len={} payload_len={} segment={} offset={}",
            rec.record_type,
            rec.txid,
            rec.key.len(),
            rec.payload.len(),
            rec.segment,
            rec.offset,
        );
        if let Ok(s) = std::str::from_utf8(&rec.payload) {
            println!("       payload_utf8={s:?}");
        }
    }

    log.close()?;
    println!("close: ok");
    Ok(())
}
