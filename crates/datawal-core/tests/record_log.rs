//! Integration tests for `RecordLog`.
//!
//! Covers the 12 cases required by the v0.1-pre spec. Every test uses
//! a fresh `tempfile::tempdir()`; nothing here touches the real
//! filesystem outside of `TMPDIR`.

use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};

use datawal::format::{HEADER_LEN, MAX_KEY_LEN};
use datawal::{RecordLog, RecordType};
use tempfile::tempdir;

// ---------------------------------------------------------------------------
// 1. open_new_dir_creates_first_segment
// ---------------------------------------------------------------------------
#[test]
fn open_new_dir_creates_first_segment() {
    let dir = tempdir().unwrap();
    let log = RecordLog::open(dir.path()).unwrap();
    drop(log);

    let seg = dir.path().join("00000001.dwal");
    assert!(seg.exists(), "first segment should be created on open");
    let lock = dir.path().join(".lock");
    // The sentinel `.lock` file persists across runs; the lock is held by
    // a file descriptor, not by the existence of the file. After drop we
    // must be able to re-acquire.
    assert!(lock.exists(), "sentinel lock file persists across runs");
    let _log2 = RecordLog::open(dir.path()).unwrap();
}

// ---------------------------------------------------------------------------
// 1b. second_open_fails_while_first_open
// ---------------------------------------------------------------------------
#[test]
fn second_open_fails_while_first_open() {
    let dir = tempdir().unwrap();
    let log1 = RecordLog::open(dir.path()).unwrap();
    let err = RecordLog::open(dir.path()).unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("already locked") || msg.contains("locked"),
        "expected lock-conflict error, got: {msg}"
    );
    drop(log1);
    // After dropping the first holder, a second open must succeed.
    let _log2 = RecordLog::open(dir.path()).unwrap();
}

// ---------------------------------------------------------------------------
// 2. append_then_scan_roundtrip
// ---------------------------------------------------------------------------
#[test]
fn append_then_scan_roundtrip() {
    let dir = tempdir().unwrap();
    let mut log = RecordLog::open(dir.path()).unwrap();
    let r = log.append(b"payload-1").unwrap();
    assert_eq!(r.segment, 1);
    assert_eq!(r.offset, 0);
    log.fsync().unwrap();

    let records = log.scan().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].record_type, RecordType::Raw);
    assert_eq!(records[0].payload, b"payload-1");
    assert_eq!(records[0].txid, 1);
}

// ---------------------------------------------------------------------------
// 3. append_n_records_scan_yields_same_order
// ---------------------------------------------------------------------------
#[test]
fn append_n_records_scan_yields_same_order() {
    let dir = tempdir().unwrap();
    let mut log = RecordLog::open(dir.path()).unwrap();
    let n: usize = 50;
    for i in 0..n {
        log.append(format!("rec-{i}").as_bytes()).unwrap();
    }
    log.fsync().unwrap();

    let records = log.scan().unwrap();
    assert_eq!(records.len(), n);
    for (i, rec) in records.iter().enumerate() {
        assert_eq!(rec.payload, format!("rec-{i}").as_bytes());
        assert_eq!(rec.txid as usize, i + 1, "txid is monotonic from 1");
    }
}

// ---------------------------------------------------------------------------
// 4. scan_empty_log_yields_empty
// ---------------------------------------------------------------------------
#[test]
fn scan_empty_log_yields_empty() {
    let dir = tempdir().unwrap();
    let mut log = RecordLog::open(dir.path()).unwrap();
    let records = log.scan().unwrap();
    assert!(records.is_empty());

    let report = log.recovery_report().unwrap();
    assert_eq!(report.records_replayed, 0);
    assert_eq!(report.tail_truncated, 0);
    assert_eq!(report.last_txid_seen, 0);
}

// ---------------------------------------------------------------------------
// 5. crc_mismatch_in_payload_returns_error_or_report
//
// CRC mismatch in a *closed* (non-tail) segment is a hard error. In
// v0.1-pre we can only force this case by rotating: that way the
// corrupted record is no longer in the active tail segment.
// ---------------------------------------------------------------------------
#[test]
fn crc_mismatch_in_payload_returns_error_or_report() {
    let dir = tempdir().unwrap();
    let r2;
    {
        let mut log = RecordLog::open(dir.path()).unwrap();
        log.append(b"first").unwrap();
        r2 = log.append(b"second").unwrap();
        log.append(b"third").unwrap();
        // Rotate so segment 1 becomes a *closed* (non-tail) segment.
        log.rotate().unwrap();
        log.append(b"fourth-in-seg2").unwrap();
        log.fsync().unwrap();
    }

    // Corrupt one payload byte of `r2` inside the now-closed segment 1.
    let seg = dir.path().join("00000001.dwal");
    let mut f = OpenOptions::new()
        .read(true)
        .write(true)
        .open(seg.as_path())
        .unwrap();
    let payload_byte_offset = r2.offset + HEADER_LEN as u64;
    f.seek(SeekFrom::Start(payload_byte_offset)).unwrap();
    f.write_all(&[0xFF]).unwrap();
    drop(f);

    // Reopen and scan: must error because the corruption is in a
    // non-tail (closed) segment.
    let res = RecordLog::open(dir.path());
    assert!(
        res.is_err(),
        "CRC mismatch in a closed segment must surface as error"
    );
}

// ---------------------------------------------------------------------------
// 6. truncated_tail_record_is_ignored_or_reported
//
// We append 3 records, then truncate the file in the middle of the
// last record. Reopen+scan must succeed, return the first 2, and
// report tail truncation in `RecoveryReport`.
// ---------------------------------------------------------------------------
#[test]
fn truncated_tail_record_is_ignored_or_reported() {
    let dir = tempdir().unwrap();
    let mut log = RecordLog::open(dir.path()).unwrap();
    log.append(b"alpha").unwrap();
    log.append(b"beta").unwrap();
    let r3 = log.append(b"gamma").unwrap();
    log.fsync().unwrap();
    drop(log);

    let seg = dir.path().join("00000001.dwal");
    let full_len = std::fs::metadata(&seg).unwrap().len();
    let truncate_to = r3.offset + (HEADER_LEN as u64) + 1;
    assert!(truncate_to < full_len);
    let f = OpenOptions::new().write(true).open(&seg).unwrap();
    f.set_len(truncate_to).unwrap();
    drop(f);

    let mut log = RecordLog::open(dir.path()).unwrap();
    let records = log.scan().unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].payload, b"alpha");
    assert_eq!(records[1].payload, b"beta");

    let report = log.recovery_report().unwrap();
    assert_eq!(report.records_replayed, 2);
    assert_eq!(report.tail_truncated, 1);
    assert!(report.tail_bytes_discarded > 0);
}

// ---------------------------------------------------------------------------
// 7. rotate_creates_new_segment_and_scan_preserves_order
// ---------------------------------------------------------------------------
#[test]
fn rotate_creates_new_segment_and_scan_preserves_order() {
    let dir = tempdir().unwrap();
    let mut log = RecordLog::open(dir.path()).unwrap();
    log.append(b"seg1-a").unwrap();
    log.append(b"seg1-b").unwrap();
    log.rotate().unwrap();
    log.append(b"seg2-a").unwrap();
    log.append(b"seg2-b").unwrap();
    log.fsync().unwrap();

    assert!(dir.path().join("00000001.dwal").exists());
    assert!(dir.path().join("00000002.dwal").exists());

    let records = log.scan().unwrap();
    let payloads: Vec<&[u8]> = records.iter().map(|r| r.payload.as_slice()).collect();
    assert_eq!(
        payloads,
        vec![&b"seg1-a"[..], b"seg1-b", b"seg2-a", b"seg2-b"]
    );
    let segs: Vec<u32> = records.iter().map(|r| r.segment).collect();
    assert_eq!(segs, vec![1, 1, 2, 2]);
}

// ---------------------------------------------------------------------------
// 8. fsync_is_idempotent
// ---------------------------------------------------------------------------
#[test]
fn fsync_is_idempotent() {
    let dir = tempdir().unwrap();
    let mut log = RecordLog::open(dir.path()).unwrap();
    log.append(b"x").unwrap();
    log.fsync().unwrap();
    log.fsync().unwrap();
    log.fsync().unwrap();
    let records = log.scan().unwrap();
    assert_eq!(records.len(), 1);
}

// ---------------------------------------------------------------------------
// 9. append_rejects_payload_over_max
//
// The payload bound (64 MiB) is exercised in `format::tests`. Here we
// exercise the key bound through the public `append_record` API,
// which is the only public path that takes a key. We also confirm a
// normal small payload still works.
// ---------------------------------------------------------------------------
#[test]
fn append_rejects_payload_over_max() {
    let dir = tempdir().unwrap();
    let mut log = RecordLog::open(dir.path()).unwrap();

    let big_key = vec![0u8; (MAX_KEY_LEN as usize) + 1];
    let res = log.append_record(RecordType::Put, &big_key, b"");
    assert!(res.is_err(), "oversize key must be rejected");

    log.append(b"normal").unwrap();
    let records = log.scan().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].payload, b"normal");
}

// ---------------------------------------------------------------------------
// 10. unknown_magic_errors
//
// Write garbage at the start of the segment file: open must fail.
// ---------------------------------------------------------------------------
#[test]
fn unknown_magic_errors() {
    let dir = tempdir().unwrap();
    {
        let mut log = RecordLog::open(dir.path()).unwrap();
        log.append(b"hello").unwrap();
        log.fsync().unwrap();
    }
    let seg = dir.path().join("00000001.dwal");
    {
        let mut f = OpenOptions::new().write(true).open(seg.as_path()).unwrap();
        f.seek(SeekFrom::Start(0)).unwrap();
        f.write_all(b"XXXX").unwrap();
        drop(f);
    }
    let res = RecordLog::open(dir.path());
    assert!(res.is_err(), "bad magic must be a hard error");
}

// ---------------------------------------------------------------------------
// 11. unknown_version_errors
//
// Append one record, then overwrite its version field with a bogus
// value. Open must fail.
// ---------------------------------------------------------------------------
#[test]
fn unknown_version_errors() {
    let dir = tempdir().unwrap();
    {
        let mut log = RecordLog::open(dir.path()).unwrap();
        log.append(b"hello").unwrap();
        log.fsync().unwrap();
    }
    let seg = dir.path().join("00000001.dwal");
    {
        // version is at offset 4 (right after MAGIC), 2 bytes LE.
        let mut f = OpenOptions::new().write(true).open(seg.as_path()).unwrap();
        f.seek(SeekFrom::Start(4)).unwrap();
        f.write_all(&[0xFF, 0xFF]).unwrap();
        drop(f);
    }
    let res = RecordLog::open(dir.path());
    assert!(res.is_err(), "unknown version must be a hard error");
}

// ---------------------------------------------------------------------------
// 12. unknown_record_type_errors
//
// Append one record, then overwrite its record_type field with a
// bogus value. Open must fail.
// ---------------------------------------------------------------------------
#[test]
fn unknown_record_type_errors() {
    let dir = tempdir().unwrap();
    {
        let mut log = RecordLog::open(dir.path()).unwrap();
        log.append(b"hello").unwrap();
        log.fsync().unwrap();
    }
    let seg = dir.path().join("00000001.dwal");
    {
        // record_type is at offset 6 (after MAGIC+version), 1 byte.
        let mut f = OpenOptions::new().write(true).open(seg.as_path()).unwrap();
        f.seek(SeekFrom::Start(6)).unwrap();
        f.write_all(&[0xEE]).unwrap();
        drop(f);
    }
    let res = RecordLog::open(dir.path());
    assert!(res.is_err(), "unknown record_type must be a hard error");
}

// ---------------------------------------------------------------------------
// 13. append_fsync_reopen
//
// Append some records, fsync, drop the log, reopen, and verify that the
// records survive. This is the durability boundary contract: append +
// fsync = durable.
// ---------------------------------------------------------------------------
#[test]
fn append_fsync_reopen() {
    let dir = tempdir().unwrap();
    {
        let mut log = RecordLog::open(dir.path()).unwrap();
        log.append(b"alpha").unwrap();
        log.append(b"beta").unwrap();
        log.append(b"gamma").unwrap();
        log.fsync().unwrap();
        // drop releases lock
    }
    let mut log2 = RecordLog::open(dir.path()).unwrap();
    let records = log2.scan().unwrap();
    let payloads: Vec<&[u8]> = records.iter().map(|r| r.payload.as_slice()).collect();
    assert_eq!(payloads, vec![&b"alpha"[..], &b"beta"[..], &b"gamma"[..]]);
    let report = log2.recovery_report().unwrap();
    assert_eq!(report.records_replayed, 3);
    assert_eq!(report.tail_truncated, 0);
    assert_eq!(report.tail_bytes_discarded, 0);
}

// Helper: confirm File::open works (used to silence lint).
#[allow(dead_code)]
fn _unused(_: File) {}

// ---------------------------------------------------------------------------
// scan_iter parity tests
//
// These verify that `RecordLog::scan_iter` (the record-level lazy iterator,
// segment-buffered) returns the same records and the same `RecoveryReport`
// as the eager `RecordLog::scan` in every scenario the legacy tests above
// already cover.
//
// Lazy-at-record-level only. Each segment is still bulk-loaded into memory
// before any record from it is yielded; see the rustdoc on `scan_iter`.
// ---------------------------------------------------------------------------

// scan_iter_matches_scan
//
// Build a log with several records in a single segment; assert that
// `scan()` and `scan_iter().collect()` return identical Vec<Record>.
#[test]
fn scan_iter_matches_scan() {
    let dir = tempdir().unwrap();
    {
        let mut log = RecordLog::open(dir.path()).unwrap();
        log.append(b"alpha").unwrap();
        log.append(b"beta").unwrap();
        log.append(b"gamma").unwrap();
        log.fsync().unwrap();
    }
    let mut eager_log = RecordLog::open(dir.path()).unwrap();
    let eager = eager_log.scan().unwrap();
    drop(eager_log);

    let lazy_log = RecordLog::open(dir.path()).unwrap();
    let lazy: Vec<_> = lazy_log
        .scan_iter()
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();

    assert_eq!(eager.len(), lazy.len(), "record counts differ");
    for (a, b) in eager.iter().zip(lazy.iter()) {
        assert_eq!(a.record_type, b.record_type);
        assert_eq!(a.txid, b.txid);
        assert_eq!(a.key, b.key);
        assert_eq!(a.payload, b.payload);
        assert_eq!(a.segment, b.segment);
        assert_eq!(a.offset, b.offset);
        assert_eq!(a.len, b.len);
    }
}

// scan_iter_preserves_order_across_segments
//
// With a rotation between writes, both `scan` and `scan_iter` yield
// records in segment-then-offset order.
#[test]
fn scan_iter_preserves_order_across_segments() {
    let dir = tempdir().unwrap();
    {
        let mut log = RecordLog::open(dir.path()).unwrap();
        log.append(b"a1").unwrap();
        log.append(b"a2").unwrap();
        log.fsync().unwrap();
        log.rotate().unwrap();
        log.append(b"b1").unwrap();
        log.append(b"b2").unwrap();
        log.fsync().unwrap();
    }

    let mut eager_log = RecordLog::open(dir.path()).unwrap();
    let eager = eager_log.scan().unwrap();
    drop(eager_log);

    let lazy_log = RecordLog::open(dir.path()).unwrap();
    let lazy: Vec<_> = lazy_log
        .scan_iter()
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();

    let eager_payloads: Vec<&[u8]> = eager.iter().map(|r| r.payload.as_slice()).collect();
    let lazy_payloads: Vec<&[u8]> = lazy.iter().map(|r| r.payload.as_slice()).collect();
    assert_eq!(
        eager_payloads,
        vec![&b"a1"[..], &b"a2"[..], &b"b1"[..], &b"b2"[..]]
    );
    assert_eq!(eager_payloads, lazy_payloads);

    let eager_segments: Vec<u32> = eager.iter().map(|r| r.segment).collect();
    let lazy_segments: Vec<u32> = lazy.iter().map(|r| r.segment).collect();
    assert_eq!(eager_segments, vec![1, 1, 2, 2]);
    assert_eq!(eager_segments, lazy_segments);
}

// scan_iter_tail_truncation_matches_scan
//
// Truncate the active segment mid-record. Both eager and lazy paths must
// recover the same valid prefix and surface the same `RecoveryReport`.
#[test]
fn scan_iter_tail_truncation_matches_scan() {
    let dir = tempdir().unwrap();
    {
        let mut log = RecordLog::open(dir.path()).unwrap();
        log.append(b"r1").unwrap();
        log.append(b"r2").unwrap();
        log.append(b"r3").unwrap();
        log.fsync().unwrap();
    }

    // Truncate inside r3's payload: keep r1+r2 + r3's header + 1 byte.
    // We don't need exact byte arithmetic because the spec is "longest
    // valid prefix"; cutting at total_len - 5 is enough to invalidate r3.
    let path = dir.path().join("00000001.dwal");
    let f = OpenOptions::new().write(true).open(&path).unwrap();
    let total = f.metadata().unwrap().len();
    assert!(total > 5, "log should have grown past 5 bytes");
    f.set_len(total - 5).unwrap();
    drop(f);

    // Eager path.
    let mut eager_log = RecordLog::open(dir.path()).unwrap();
    let eager = eager_log.scan().unwrap();
    let eager_report = eager_log.recovery_report().unwrap();
    drop(eager_log);

    // Lazy path.
    let lazy_log = RecordLog::open(dir.path()).unwrap();
    let mut lazy_iter = lazy_log.scan_iter().unwrap();
    let mut lazy: Vec<_> = Vec::new();
    for item in &mut lazy_iter {
        lazy.push(item.unwrap());
    }
    let lazy_report = lazy_iter.recovery_report();

    assert_eq!(eager.len(), 2);
    assert_eq!(lazy.len(), 2);
    let eager_payloads: Vec<&[u8]> = eager.iter().map(|r| r.payload.as_slice()).collect();
    let lazy_payloads: Vec<&[u8]> = lazy.iter().map(|r| r.payload.as_slice()).collect();
    assert_eq!(eager_payloads, vec![&b"r1"[..], &b"r2"[..]]);
    assert_eq!(eager_payloads, lazy_payloads);

    assert_eq!(eager_report.records_replayed, 2);
    assert_eq!(eager_report.tail_truncated, 1);
    assert!(eager_report.tail_bytes_discarded > 0);
    assert_eq!(lazy_report.records_replayed, eager_report.records_replayed);
    assert_eq!(lazy_report.tail_truncated, eager_report.tail_truncated);
    assert_eq!(
        lazy_report.tail_bytes_discarded,
        eager_report.tail_bytes_discarded
    );
    assert_eq!(lazy_report.last_txid_seen, eager_report.last_txid_seen);
}

// scan_iter_closed_segment_crc_error_matches_scan
//
// Corrupt one byte inside a sealed (non-active) segment. Both eager and
// lazy paths must surface a hard error. We exercise both paths in this
// test: the eager `RecordLog::open` already calls scan, so we corrupt
// AFTER opening, while the writer is still holding the lock, then call
// `scan_iter()` directly and verify it yields `Err` on the corrupt
// segment.
#[test]
fn scan_iter_closed_segment_crc_error_matches_scan() {
    let dir = tempdir().unwrap();
    let mut log = RecordLog::open(dir.path()).unwrap();
    log.append(b"seg1-a").unwrap();
    log.append(b"seg1-b").unwrap();
    log.fsync().unwrap();
    log.rotate().unwrap();
    log.append(b"seg2-a").unwrap();
    log.fsync().unwrap();

    // Corrupt one byte inside seg1's payload. The fs2 lock is exclusive
    // for `RecordLog::open` only; raw OpenOptions on the underlying file
    // is allowed within the same process.
    let path = dir.path().join("00000001.dwal");
    let mut f = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    // Flip a byte deep into the file (any byte inside record bytes).
    f.seek(SeekFrom::Start((HEADER_LEN as u64) + 2)).unwrap();
    f.write_all(&[0xFF]).unwrap();
    drop(f);

    // Lazy path: iterator should produce an Err when it reaches seg1.
    let mut lazy_iter = log.scan_iter().unwrap();
    let mut saw_err = false;
    for item in &mut lazy_iter {
        if item.is_err() {
            saw_err = true;
            break;
        }
    }
    assert!(saw_err, "lazy iter should report CRC error in sealed seg1");

    drop(log);

    // Eager path on reopen: scan-during-open must fail loudly.
    let result = RecordLog::open(dir.path());
    assert!(
        result.is_err(),
        "eager reopen should fail on sealed CRC mismatch"
    );
}

// scan_iter_can_stop_early
//
// Dropping the iterator mid-stream must be safe: no panic, no side
// effect on disk. A subsequent `scan_iter` or `scan` returns the full
// log.
#[test]
fn scan_iter_can_stop_early() {
    let dir = tempdir().unwrap();
    {
        let mut log = RecordLog::open(dir.path()).unwrap();
        for i in 0..5 {
            let payload = format!("rec-{i}");
            log.append(payload.as_bytes()).unwrap();
        }
        log.fsync().unwrap();
    }

    let log = RecordLog::open(dir.path()).unwrap();
    let mut iter = log.scan_iter().unwrap();
    let first = iter.next().unwrap().unwrap();
    let second = iter.next().unwrap().unwrap();
    assert_eq!(first.payload, b"rec-0");
    assert_eq!(second.payload, b"rec-1");

    // Mid-iter snapshot of recovery_report: at least 2 records have
    // been yielded already.
    let partial = iter.recovery_report();
    assert!(
        partial.records_replayed >= 2,
        "expected >=2 records_replayed mid-iter, got {}",
        partial.records_replayed
    );

    drop(iter);
    drop(log);

    // Reopen and run a full eager scan: must see all 5 records.
    let mut log2 = RecordLog::open(dir.path()).unwrap();
    let all = log2.scan().unwrap();
    let payloads: Vec<&[u8]> = all.iter().map(|r| r.payload.as_slice()).collect();
    assert_eq!(
        payloads,
        vec![
            &b"rec-0"[..],
            &b"rec-1"[..],
            &b"rec-2"[..],
            &b"rec-3"[..],
            &b"rec-4"[..]
        ]
    );
}
