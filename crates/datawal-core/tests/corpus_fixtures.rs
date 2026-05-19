//! Wire-format corpus fixture tests.
//!
//! Each fixture under `tests/corpus/` is a small, hand-checked snapshot of
//! the v0.1-pre on-disk format. These tests freeze the wire format: any
//! incompatible change to the encoding/decoding must regenerate the
//! corpus AND bump `WIRE_VERSION`.
//!
//! Fixtures are read-only by design. Each test copies the relevant
//! fixture to a temp dir before opening it (RecordLog::open creates a
//! `.lock` and may rotate segments), so the committed bytes never
//! change.

use std::fs;
use std::path::{Path, PathBuf};

use datawal_core::{DataWal, RecordLog, RecordType};
use tempfile::TempDir;

fn corpus_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("corpus")
}

fn copy_fixture(name: &str) -> (TempDir, PathBuf) {
    let tmp = TempDir::new().expect("tempdir");
    let dst = tmp.path().join(name);
    fs::create_dir_all(&dst).expect("mkdir dst");
    let src = corpus_root().join(name);
    for entry in fs::read_dir(&src).expect("read fixture dir") {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        let fname = path.file_name().expect("file name");
        // Skip any stray sentinel file from a previous open (none should ship).
        if fname == ".lock" {
            continue;
        }
        fs::copy(&path, dst.join(fname)).expect("copy fixture file");
    }
    (tmp, dst)
}

// -----------------------------------------------------------------------------
// valid_log
// -----------------------------------------------------------------------------

#[test]
fn valid_log_scans_three_records() {
    let (_tmp, dir) = copy_fixture("valid_log");
    let mut log = RecordLog::open(&dir).expect("open");
    let records = log.scan().expect("scan");
    assert_eq!(records.len(), 3);
    assert_eq!(records[0].payload, b"alpha");
    assert_eq!(records[1].payload, b"beta");
    assert_eq!(records[2].payload, b"gamma");
    for r in &records {
        assert_eq!(r.record_type, RecordType::Raw);
    }
    let report = log.recovery_report().expect("report");
    assert_eq!(report.records_replayed, 3);
    assert_eq!(report.tail_truncated, 0);
    assert_eq!(report.tail_bytes_discarded, 0);
}

#[test]
fn valid_log_scan_is_idempotent() {
    let (_tmp, dir) = copy_fixture("valid_log");
    let mut log = RecordLog::open(&dir).expect("open");
    let first = log.scan().expect("scan 1");
    let second = log.scan().expect("scan 2");
    assert_eq!(first.len(), second.len());
    for (a, b) in first.iter().zip(second.iter()) {
        assert_eq!(a.payload, b.payload);
        assert_eq!(a.txid, b.txid);
        assert_eq!(a.segment, b.segment);
        assert_eq!(a.offset, b.offset);
        assert_eq!(a.len, b.len);
    }
}

#[test]
fn valid_log_reopen_is_idempotent() {
    let (_tmp, dir) = copy_fixture("valid_log");
    let (records_a, report_a) = {
        let mut log = RecordLog::open(&dir).expect("open 1");
        let r = log.scan().expect("scan 1");
        let rp = log.recovery_report().expect("report 1");
        (r, rp)
    };
    let (records_b, report_b) = {
        let mut log = RecordLog::open(&dir).expect("open 2");
        let r = log.scan().expect("scan 2");
        let rp = log.recovery_report().expect("report 2");
        (r, rp)
    };
    assert_eq!(records_a.len(), records_b.len());
    for (a, b) in records_a.iter().zip(records_b.iter()) {
        assert_eq!(a.payload, b.payload);
        assert_eq!(a.txid, b.txid);
    }
    assert_eq!(report_a.records_replayed, report_b.records_replayed);
    assert_eq!(report_a.tail_truncated, report_b.tail_truncated);
    assert_eq!(report_a.tail_bytes_discarded, report_b.tail_bytes_discarded);
}

// -----------------------------------------------------------------------------
// truncated_tail
// -----------------------------------------------------------------------------

#[test]
fn truncated_tail_recovers_valid_prefix() {
    let (_tmp, dir) = copy_fixture("truncated_tail");
    let mut log = RecordLog::open(&dir).expect("open");
    let records = log.scan().expect("scan");
    // Original wrote 3 records; the last one was truncated.
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].payload, b"one");
    assert_eq!(records[1].payload, b"two");
    let report = log.recovery_report().expect("report");
    assert_eq!(report.records_replayed, 2);
    assert_eq!(report.tail_truncated, 1);
    assert!(report.tail_bytes_discarded > 0);
}

// -----------------------------------------------------------------------------
// bad_crc (corruption lives in a CLOSED segment -> hard error)
// -----------------------------------------------------------------------------

#[test]
fn bad_crc_in_closed_segment_is_hard_error() {
    let (_tmp, dir) = copy_fixture("bad_crc");
    let err = match RecordLog::open(&dir) {
        Ok(_) => panic!("expected open to fail on closed-segment CRC mismatch"),
        Err(e) => e,
    };
    let s = format!("{:?}", err).to_lowercase();
    assert!(
        s.contains("crc") || s.contains("mismatch") || s.contains("corrupt"),
        "error did not mention CRC corruption: {:?}",
        err
    );
}

// -----------------------------------------------------------------------------
// unknown_version
// -----------------------------------------------------------------------------

#[test]
fn unknown_version_is_hard_error() {
    let (_tmp, dir) = copy_fixture("unknown_version");
    let err = match RecordLog::open(&dir) {
        Ok(_) => panic!("expected open to fail on unknown wire version"),
        Err(e) => e,
    };
    let s = format!("{:?}", err).to_lowercase();
    assert!(
        s.contains("version") || s.contains("unknown"),
        "error did not mention version: {:?}",
        err
    );
}

// -----------------------------------------------------------------------------
// delete_tombstone
// -----------------------------------------------------------------------------

#[test]
fn delete_tombstone_projects_to_lww_keydir() {
    let (_tmp, dir) = copy_fixture("delete_tombstone");
    let kv = DataWal::open(&dir).expect("open");
    // Source wrote: put alpha=1, put beta=2, put alpha=3, del beta.
    assert_eq!(kv.len(), 1);
    assert_eq!(kv.get(b"alpha").expect("get"), Some(b"3".to_vec()));
    assert!(!kv.contains_key(b"beta"));
}

#[test]
fn delete_tombstone_underlying_log_replays_all_records() {
    // The DataWal projection collapses to one live key, but the
    // underlying RecordLog must still replay every framed record,
    // including the tombstone.
    let (_tmp, dir) = copy_fixture("delete_tombstone");
    let mut log = RecordLog::open(&dir).expect("open log");
    let records = log.scan().expect("scan");
    assert_eq!(records.len(), 4);
    assert_eq!(records[0].record_type, RecordType::Put);
    assert_eq!(records[1].record_type, RecordType::Put);
    assert_eq!(records[2].record_type, RecordType::Put);
    assert_eq!(records[3].record_type, RecordType::Delete);
    // Every record carries a key (Puts and the Delete).
    for r in &records {
        assert!(!r.key.is_empty(), "Put/Delete record had empty key");
    }
}

// -----------------------------------------------------------------------------
// compact_to_output
// -----------------------------------------------------------------------------

#[test]
fn compact_to_output_contains_only_live_keys_as_puts() {
    let (_tmp, dir) = copy_fixture("compact_to_output");
    let kv = DataWal::open(&dir).expect("open");
    assert_eq!(kv.len(), 2);
    assert_eq!(kv.get(b"keep").expect("keep"), Some(b"final".to_vec()));
    assert_eq!(kv.get(b"other").expect("other"), Some(b"value".to_vec()));
    assert!(!kv.contains_key(b"gone"));
}

#[test]
fn compact_to_output_has_no_tombstones() {
    let (_tmp, dir) = copy_fixture("compact_to_output");
    let mut log = RecordLog::open(&dir).expect("open log");
    let records = log.scan().expect("scan");
    // Each live key should appear exactly once, as a Put. No tombstones.
    assert_eq!(records.len(), 2);
    for r in &records {
        assert_eq!(
            r.record_type,
            RecordType::Put,
            "compacted log must only contain Put records"
        );
    }
    let mut keys: Vec<&[u8]> = records.iter().map(|r| r.key.as_slice()).collect();
    keys.sort();
    assert_eq!(keys, vec![&b"keep"[..], &b"other"[..]]);
}

// -----------------------------------------------------------------------------
// Recovery idempotence across all valid fixtures.
// -----------------------------------------------------------------------------

#[test]
fn reopen_twice_yields_same_records() {
    for name in [
        "valid_log",
        "truncated_tail",
        "delete_tombstone",
        "compact_to_output",
    ] {
        let (_tmp, dir) = copy_fixture(name);
        let scan_a = {
            let mut log = RecordLog::open(&dir).expect("open 1");
            log.scan().expect("scan 1")
        };
        let scan_b = {
            let mut log = RecordLog::open(&dir).expect("open 2");
            log.scan().expect("scan 2")
        };
        assert_eq!(
            scan_a.len(),
            scan_b.len(),
            "fixture {} length mismatch",
            name
        );
        for (a, b) in scan_a.iter().zip(scan_b.iter()) {
            assert_eq!(a.payload, b.payload, "fixture {}", name);
            assert_eq!(a.key, b.key, "fixture {}", name);
            assert_eq!(a.record_type, b.record_type, "fixture {}", name);
            assert_eq!(a.txid, b.txid, "fixture {}", name);
            assert_eq!(a.segment, b.segment, "fixture {}", name);
            assert_eq!(a.offset, b.offset, "fixture {}", name);
            assert_eq!(a.len, b.len, "fixture {}", name);
        }
    }
}
