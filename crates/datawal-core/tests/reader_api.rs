//! Integration tests for `RecordLogReader` (PR #5).
//!
//! These tests pin the public contract of the reader-only handle:
//!
//! 1. It never touches the writer's cooperative lock.
//! 2. It takes a segment-id snapshot at `open()` time.
//! 3. It re-uses the existing `RecordIter` recovery semantics
//!    (truncated tail on the last segment is tolerated; corruption
//!    in a sealed segment is a hard error).
//! 4. `scan_iter()` can be called repeatedly against the same snapshot.
//! 5. It rejects non-directory paths up front and tolerates an empty
//!    directory.
//!
//! What the tests deliberately do NOT cover:
//! - Cross-process writer + reader scenarios. Tracked separately;
//!   the in-process case below already exercises the no-lock contract.
//! - Byte-level live tail. The reader's snapshot is segment-id level,
//!   not byte-level; this is documented in `RecordLogReader`'s rustdoc.

use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};

use datawal::{RecordLog, RecordLogReader, RecordType};
use tempfile::tempdir;

fn collect(reader: &RecordLogReader) -> Vec<(RecordType, Vec<u8>, Vec<u8>)> {
    let iter = reader.scan_iter().expect("scan_iter");
    let mut out = Vec::new();
    for rec in iter {
        let rec = rec.expect("record without error");
        out.push((rec.record_type, rec.key, rec.payload));
    }
    out
}

fn seg_path(dir: &std::path::Path, id: u32) -> std::path::PathBuf {
    dir.join(format!("{:08}.dwal", id))
}

#[test]
fn reader_opens_without_taking_writer_lock() {
    // A live writer holds the cooperative lock. The reader must still open.
    let dir = tempdir().unwrap();
    let mut writer = RecordLog::open(dir.path()).unwrap();
    writer.append_record(RecordType::Put, b"k", b"v").unwrap();
    writer.fsync().unwrap();

    // Reader opens against the same dir without errors.
    let reader = RecordLogReader::open(dir.path()).expect("reader opens");
    assert!(!reader.segments().is_empty());

    // And the writer is still happy to mutate after the reader exists.
    writer.append_record(RecordType::Put, b"k2", b"v2").unwrap();
    writer.fsync().unwrap();

    drop(reader);
    drop(writer);
}

#[test]
fn reader_open_fails_on_missing_path() {
    let dir = tempdir().unwrap();
    let missing = dir.path().join("does-not-exist");
    let err = RecordLogReader::open(&missing).unwrap_err();
    let msg = format!("{:#}", err);
    assert!(
        msg.contains("does-not-exist") || msg.to_lowercase().contains("no such"),
        "unexpected error: {msg}"
    );
}

#[test]
fn reader_open_fails_on_non_directory_path() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("file.txt");
    std::fs::write(&file, b"not a dir").unwrap();
    let err = RecordLogReader::open(&file).unwrap_err();
    let msg = format!("{:#}", err);
    assert!(
        msg.to_lowercase().contains("not a directory") || msg.contains("file.txt"),
        "unexpected error: {msg}"
    );
}

#[test]
fn reader_open_succeeds_on_empty_dir() {
    let dir = tempdir().unwrap();
    let reader = RecordLogReader::open(dir.path()).expect("empty dir is fine");
    assert_eq!(reader.segments().len(), 0);
    let records = collect(&reader);
    assert!(records.is_empty());
}

#[test]
fn reader_segments_match_snapshot_taken_at_open() {
    // Writer creates 2 sealed segments + 1 active before the reader opens.
    let dir = tempdir().unwrap();
    let mut writer = RecordLog::open(dir.path()).unwrap();
    writer.append_record(RecordType::Put, b"a", b"1").unwrap();
    writer.fsync().unwrap();
    writer.rotate().unwrap();
    writer.append_record(RecordType::Put, b"b", b"2").unwrap();
    writer.fsync().unwrap();
    writer.rotate().unwrap();
    writer.append_record(RecordType::Put, b"c", b"3").unwrap();
    writer.fsync().unwrap();

    // Reader takes a snapshot. Should see 3 ids.
    let reader = RecordLogReader::open(dir.path()).unwrap();
    assert_eq!(reader.segments(), &[1u32, 2, 3]);

    // Writer rotates and appends a fourth segment AFTER the snapshot.
    writer.rotate().unwrap();
    writer.append_record(RecordType::Put, b"d", b"4").unwrap();
    writer.fsync().unwrap();

    // Reader still only sees the original 3 segments.
    assert_eq!(reader.segments(), &[1u32, 2, 3]);

    let records = collect(&reader);
    let keys: Vec<&[u8]> = records.iter().map(|(_, k, _)| k.as_slice()).collect();
    assert_eq!(keys, vec![b"a".as_ref(), b"b", b"c"]);
}

#[test]
fn reader_observes_writer_appends_within_snapshot_segments() {
    // The snapshot is segment-id level, not byte-level. If the writer
    // appends MORE records to a segment that was part of the snapshot,
    // a later scan_iter() call WILL see them. This is by design and
    // documented; the test pins the behaviour so it doesn't change
    // silently.
    let dir = tempdir().unwrap();
    let mut writer = RecordLog::open(dir.path()).unwrap();
    writer.append_record(RecordType::Put, b"a", b"1").unwrap();
    writer.fsync().unwrap();

    let reader = RecordLogReader::open(dir.path()).unwrap();
    assert_eq!(reader.segments(), &[1u32]);

    // Writer appends more bytes to the same (snapshotted) segment.
    writer.append_record(RecordType::Put, b"b", b"2").unwrap();
    writer.fsync().unwrap();

    // A fresh scan_iter sees the additional record. This is the
    // documented "snapshot is segment-id level" semantic.
    let records = collect(&reader);
    let keys: Vec<&[u8]> = records.iter().map(|(_, k, _)| k.as_slice()).collect();
    assert_eq!(keys, vec![b"a".as_ref(), b"b"]);
}

#[test]
fn reader_scan_iter_can_be_called_multiple_times() {
    let dir = tempdir().unwrap();
    let mut writer = RecordLog::open(dir.path()).unwrap();
    for i in 0..5u8 {
        writer
            .append_record(RecordType::Put, &[i], &[i, i, i])
            .unwrap();
    }
    writer.fsync().unwrap();

    let reader = RecordLogReader::open(dir.path()).unwrap();
    let first = collect(&reader);
    let second = collect(&reader);
    let third = collect(&reader);
    assert_eq!(first.len(), 5);
    assert_eq!(first, second);
    assert_eq!(second, third);
}

#[test]
fn reader_tolerates_unfsynced_tail_on_last_segment() {
    // Build a valid log, then append a few stray bytes (mimicking an
    // unfsynced partial frame) to the LAST segment. The iterator
    // should treat this as a clean tail truncation: yield all valid
    // records, return None at the end, and report the discarded bytes.
    let dir = tempdir().unwrap();
    {
        let mut writer = RecordLog::open(dir.path()).unwrap();
        writer.append_record(RecordType::Put, b"k1", b"v1").unwrap();
        writer.append_record(RecordType::Put, b"k2", b"v2").unwrap();
        writer.fsync().unwrap();
        // writer drops -> releases lock
    }

    // Corrupt the tail of the active segment by appending junk bytes
    // that are too short to be a valid header.
    let path = seg_path(dir.path(), 1);
    let mut f = OpenOptions::new().append(true).open(&path).unwrap();
    f.write_all(&[0xAA, 0xBB, 0xCC]).unwrap();
    f.sync_all().unwrap();
    drop(f);

    let reader = RecordLogReader::open(dir.path()).unwrap();
    let mut iter = reader.scan_iter().unwrap();
    let mut count = 0;
    for rec in iter.by_ref() {
        rec.expect("valid prefix should not error");
        count += 1;
    }
    assert_eq!(count, 2, "should yield exactly the valid prefix");

    let report = iter.recovery_report();
    assert!(
        report.tail_truncated >= 1,
        "expected tail_truncated >= 1, got {:?}",
        report
    );
    assert!(
        report.tail_bytes_discarded >= 3,
        "expected at least 3 bytes discarded, got {:?}",
        report
    );
}

#[test]
fn reader_hard_errors_on_corrupt_sealed_segment() {
    // Seal segment 1 by rotating, then write a record into segment 2.
    // Flip a byte inside segment 1's CRC region. The iterator must
    // refuse to advance past it.
    let dir = tempdir().unwrap();
    {
        let mut writer = RecordLog::open(dir.path()).unwrap();
        writer.append_record(RecordType::Put, b"k1", b"v1").unwrap();
        writer.fsync().unwrap();
        writer.rotate().unwrap();
        writer.append_record(RecordType::Put, b"k2", b"v2").unwrap();
        writer.fsync().unwrap();
    }

    // Corrupt one byte near the end of segment 1 (well past the magic +
    // header so we land inside the body/CRC and trigger CrcMismatch).
    let path = seg_path(dir.path(), 1);
    let mut f = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    let len = f.metadata().unwrap().len();
    assert!(len > 4, "segment too small to corrupt safely");
    let target = len - 2;
    f.seek(SeekFrom::Start(target)).unwrap();
    let mut buf = [0u8; 1];
    use std::io::Read;
    f.read_exact(&mut buf).unwrap();
    let flipped = [buf[0] ^ 0xFF];
    f.seek(SeekFrom::Start(target)).unwrap();
    f.write_all(&flipped).unwrap();
    f.sync_all().unwrap();
    drop(f);

    let reader = RecordLogReader::open(dir.path()).unwrap();
    let iter = reader.scan_iter().unwrap();
    let mut hard_error_seen = false;
    for rec in iter {
        if let Err(e) = rec {
            let msg = format!("{:#}", e);
            let lower = msg.to_lowercase();
            assert!(
                lower.contains("crc mismatch")
                    || lower.contains("truncated record")
                    || lower.contains("decode error"),
                "expected sealed-segment failure, got: {msg}"
            );
            assert!(
                lower.contains("non-tail") || lower.contains("segment 1"),
                "expected message to mention non-tail / segment 1, got: {msg}"
            );
            hard_error_seen = true;
            break;
        }
    }
    assert!(
        hard_error_seen,
        "iterator should have surfaced the corruption"
    );
}
