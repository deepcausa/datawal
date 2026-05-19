//! Regenerate the on-disk wire-format corpus fixtures under
//! `crates/datawal-core/tests/corpus/`.
//!
//! Run from the workspace root:
//!
//! ```text
//! cargo run -p datawal-core --example gen_corpus
//! ```
//!
//! This is **not** run as part of `cargo test`. It writes binary fixtures
//! that are then committed to the repository and consumed by
//! `tests/corpus_fixtures.rs`. Re-run only when the wire format changes
//! intentionally; otherwise the corpus is meant to stay frozen.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use datawal_core::{DataWal, RecordLog};

fn corpus_root() -> PathBuf {
    // examples/ lives next to Cargo.toml of datawal-core.
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    crate_dir.join("tests").join("corpus")
}

fn reset_dir(p: &Path) -> Result<()> {
    if p.exists() {
        fs::remove_dir_all(p).with_context(|| format!("remove {}", p.display()))?;
    }
    fs::create_dir_all(p).with_context(|| format!("create {}", p.display()))?;
    Ok(())
}

fn main() -> Result<()> {
    let root = corpus_root();
    fs::create_dir_all(&root)?;
    eprintln!("corpus root: {}", root.display());

    gen_valid_log(&root.join("valid_log"))?;
    gen_truncated_tail(&root.join("truncated_tail"))?;
    gen_bad_crc(&root.join("bad_crc"))?;
    gen_unknown_version(&root.join("unknown_version"))?;
    gen_delete_tombstone(&root.join("delete_tombstone"))?;
    gen_compact_to_output(&root.join("compact_to_output"))?;

    eprintln!("corpus regenerated.");
    Ok(())
}

fn gen_valid_log(dir: &Path) -> Result<()> {
    reset_dir(dir)?;
    let mut log = RecordLog::open(dir)?;
    log.append(b"alpha")?;
    log.append(b"beta")?;
    log.append(b"gamma")?;
    log.fsync()?;
    log.close()?;
    // Drop sentinel .lock so the fixture is a clean static artefact.
    let _ = fs::remove_file(dir.join(".lock"));
    eprintln!("  valid_log: 3 raw records");
    Ok(())
}

fn gen_truncated_tail(dir: &Path) -> Result<()> {
    reset_dir(dir)?;
    {
        let mut log = RecordLog::open(dir)?;
        log.append(b"one")?;
        log.append(b"two")?;
        log.append(b"three")?;
        log.fsync()?;
        log.close()?;
    }
    // Hard-truncate the active segment by 5 bytes (cuts into the last record).
    let seg = dir.join("00000001.dwal");
    let len = fs::metadata(&seg)?.len();
    let new_len = len.checked_sub(5).expect("segment large enough");
    let f = fs::OpenOptions::new().write(true).open(&seg)?;
    f.set_len(new_len)?;
    let _ = fs::remove_file(dir.join(".lock"));
    eprintln!(
        "  truncated_tail: 3 appends, segment {} truncated {} -> {} bytes",
        seg.file_name().unwrap().to_string_lossy(),
        len,
        new_len
    );
    Ok(())
}

fn gen_bad_crc(dir: &Path) -> Result<()> {
    reset_dir(dir)?;
    {
        let mut log = RecordLog::open(dir)?;
        log.append(b"good")?;
        // We need the second record to live in a CLOSED segment so a CRC
        // mismatch is a hard error, not treated as tail damage on the
        // active segment.
        log.rotate()?;
        log.append(b"this-will-be-corrupted")?;
        log.rotate()?;
        log.append(b"after")?;
        log.fsync()?;
        log.close()?;
    }
    // Corrupt one byte in the middle of the payload of segment 2.
    let seg2 = dir.join("00000002.dwal");
    let mut bytes = fs::read(&seg2)?;
    let idx = 24 + 5; // inside the payload (24-byte header, key_len = 0).
    bytes[idx] ^= 0xFF;
    fs::write(&seg2, &bytes)?;
    let _ = fs::remove_file(dir.join(".lock"));
    eprintln!(
        "  bad_crc: 3 records across 3 segments; {} byte {} flipped",
        seg2.file_name().unwrap().to_string_lossy(),
        idx
    );
    Ok(())
}

fn gen_unknown_version(dir: &Path) -> Result<()> {
    reset_dir(dir)?;
    {
        let mut log = RecordLog::open(dir)?;
        log.append(b"first")?;
        log.fsync()?;
        log.close()?;
    }
    // Overwrite the version field (offset 4..6) of the only record with
    // a future, unknown version.
    let seg = dir.join("00000001.dwal");
    let mut bytes = fs::read(&seg)?;
    bytes[4] = 0xFE;
    bytes[5] = 0xCA;
    fs::write(&seg, &bytes)?;
    let _ = fs::remove_file(dir.join(".lock"));
    eprintln!("  unknown_version: 1 record with wire version 0xCAFE");
    Ok(())
}

fn gen_delete_tombstone(dir: &Path) -> Result<()> {
    reset_dir(dir)?;
    {
        let mut kv = DataWal::open(dir)?;
        kv.put(b"alpha", b"1")?;
        kv.put(b"beta", b"2")?;
        kv.put(b"alpha", b"3")?;
        kv.delete(b"beta")?;
        kv.fsync()?;
    }
    let _ = fs::remove_file(dir.join(".lock"));
    eprintln!("  delete_tombstone: alpha=3 (after overwrite), beta deleted");
    Ok(())
}

fn gen_compact_to_output(dir: &Path) -> Result<()> {
    // We need a source log first; produce one, compact_to a sibling dir,
    // and ship the compacted dir.
    let source = dir.with_file_name("compact_to_source");
    reset_dir(&source)?;
    if dir.exists() {
        fs::remove_dir_all(dir)?;
    }
    {
        let mut kv = DataWal::open(&source)?;
        kv.put(b"keep", b"final")?;
        kv.put(b"keep", b"old")?;
        kv.put(b"keep", b"final")?;
        kv.put(b"gone", b"nope")?;
        kv.delete(b"gone")?;
        kv.put(b"other", b"value")?;
        kv.fsync()?;
        kv.compact_to(dir)?;
    }
    fs::remove_dir_all(&source)?;
    let _ = fs::remove_file(dir.join(".lock"));
    eprintln!("  compact_to_output: keep=final, other=value, gone absent");
    Ok(())
}
