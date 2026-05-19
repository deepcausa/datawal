//! Demo: `DataWal` put / get / delete / export / compact_to.
//!
//! Run with:
//!
//! ```text
//! cargo run -p datawal --example datawal_kv_demo
//! ```
//!
//! `DataWal` is a last-write-wins, bytes-first KV layered on top of
//! `RecordLog`. This example shows the full surface used in the
//! migration pilot: overwrite, delete, export to JSONL, and a clean
//! manual compaction into a fresh directory.

use std::path::PathBuf;

use anyhow::Result;
use datawal::DataWal;

fn main() -> Result<()> {
    let dir: PathBuf = std::env::temp_dir().join("datawal-kv-demo");
    let _ = std::fs::remove_dir_all(&dir);

    let export_path: PathBuf = std::env::temp_dir().join("datawal-kv-demo.export.jsonl");
    let _ = std::fs::remove_file(&export_path);

    let compact_dir: PathBuf = std::env::temp_dir().join("datawal-kv-demo.compact");
    let _ = std::fs::remove_dir_all(&compact_dir);

    println!("== datawal datawal_kv_demo ==");
    println!("log dir:     {}", dir.display());
    println!("export path: {}", export_path.display());
    println!("compact dir: {}", compact_dir.display());

    let mut kv = DataWal::open(&dir)?;

    kv.put(b"a", b"1")?;
    kv.put(b"b", b"2")?;
    kv.put(b"a", b"3")?; // overwrite "a"
    kv.delete(b"b")?; // tombstone "b"

    println!("len: {}", kv.len());
    println!("a = {:?}", kv.get(b"a")?);
    println!("b contains_key = {}", kv.contains_key(b"b"));

    // Export to JSONL (atomic write via safeatomic-rs).
    kv.export_jsonl(&export_path)?;
    let exported = std::fs::read_to_string(&export_path)?;
    println!("--- export jsonl ---");
    print!("{exported}");
    println!("--------------------");

    // Compact into a fresh directory.
    let stats = kv.compact_to(&compact_dir)?;
    println!(
        "compact_to: live_keys={} records_written={} bytes_written={}",
        stats.live_keys, stats.records_written, stats.bytes_written,
    );

    // Reopen the compacted directory and verify state.
    let kv2 = DataWal::open(&compact_dir)?;
    println!("after compact reopen: len={}", kv2.len());
    println!("  a = {:?}", kv2.get(b"a")?);
    println!("  b contains_key = {}", kv2.contains_key(b"b"));

    Ok(())
}
