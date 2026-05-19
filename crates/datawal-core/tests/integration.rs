//! End-to-end integration tests across `RecordLog` + `DataWal`.

use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};

use datawal::format::HEADER_LEN;
use datawal::{DataWal, RecordLog, RecordType};
use tempfile::tempdir;

// ---------------------------------------------------------------------------
// 1. recordlog_reopen_after_append
// ---------------------------------------------------------------------------
#[test]
fn recordlog_reopen_after_append() {
    let dir = tempdir().unwrap();
    {
        let mut log = RecordLog::open(dir.path()).unwrap();
        for i in 0..10u32 {
            log.append(format!("rec-{i}").as_bytes()).unwrap();
        }
        log.fsync().unwrap();
    }
    let mut log = RecordLog::open(dir.path()).unwrap();
    let records = log.scan().unwrap();
    assert_eq!(records.len(), 10);
    for (i, rec) in records.iter().enumerate() {
        assert_eq!(rec.payload, format!("rec-{i}").as_bytes());
        assert_eq!(rec.record_type, RecordType::Raw);
    }
    let report = log.recovery_report().unwrap();
    assert_eq!(report.records_replayed, 10);
    assert_eq!(report.tail_truncated, 0);
}

// ---------------------------------------------------------------------------
// 2. datawal_reopen_after_put_delete
// ---------------------------------------------------------------------------
#[test]
fn datawal_reopen_after_put_delete() {
    let dir = tempdir().unwrap();
    {
        let mut kv = DataWal::open(dir.path()).unwrap();
        for i in 0..20u32 {
            kv.put(format!("k{i:02}").as_bytes(), format!("v{i}").as_bytes())
                .unwrap();
        }
        // Delete every even key.
        for i in (0..20u32).step_by(2) {
            kv.delete(format!("k{i:02}").as_bytes()).unwrap();
        }
        // Overwrite one survivor.
        kv.put(b"k01", b"v1-bis").unwrap();
        kv.fsync().unwrap();
    }
    let kv = DataWal::open(dir.path()).unwrap();
    assert_eq!(kv.len(), 10, "10 odd keys should survive");
    for i in (0..20u32).step_by(2) {
        let key = format!("k{i:02}");
        assert!(!kv.contains_key(key.as_bytes()), "even key should be gone");
    }
    assert_eq!(kv.get(b"k01").unwrap().as_deref(), Some(&b"v1-bis"[..]));
    for i in [3u32, 5, 7, 9, 11, 13, 15, 17, 19] {
        let key = format!("k{i:02}");
        let val = format!("v{i}");
        assert_eq!(
            kv.get(key.as_bytes()).unwrap().as_deref(),
            Some(val.as_bytes())
        );
    }
}

// ---------------------------------------------------------------------------
// 3. tail_truncation_after_reopen
//
// Append several DataWal records, then truncate the log file mid-record.
// Reopen must succeed, drop the partial tail, and `RecordLog::recovery_report`
// must report it.
// ---------------------------------------------------------------------------
#[test]
fn tail_truncation_after_reopen() {
    let dir = tempdir().unwrap();
    let last_offset;
    {
        // Use RecordLog directly so we can capture the byte offset
        // returned by the last `append_record` call.
        let mut log = RecordLog::open(dir.path()).unwrap();
        log.append_record(RecordType::Put, b"a", b"alpha").unwrap();
        let r = log.append_record(RecordType::Put, b"b", b"beta").unwrap();
        last_offset = r.offset;
        log.fsync().unwrap();
    }

    // Truncate inside the last record's payload region.
    let seg = dir.path().join("00000001.dwal");
    let full_len = std::fs::metadata(&seg).unwrap().len();
    let truncate_to = last_offset + (HEADER_LEN as u64) + 1;
    assert!(truncate_to < full_len);
    let f = OpenOptions::new().write(true).open(&seg).unwrap();
    f.set_len(truncate_to).unwrap();
    drop(f);

    // Reopen and verify recovery report + state.
    {
        let mut log = RecordLog::open(dir.path()).unwrap();
        let records = log.scan().unwrap();
        assert_eq!(records.len(), 1, "only the first record should survive");
        let report = log.recovery_report().unwrap();
        assert_eq!(report.records_replayed, 1);
        assert_eq!(report.tail_truncated, 1);
        assert!(report.tail_bytes_discarded > 0);
    }

    // And DataWal reopens consistently.
    let kv2 = DataWal::open(dir.path()).unwrap();
    assert_eq!(kv2.len(), 1);
    assert!(kv2.contains_key(b"a"));
    assert!(!kv2.contains_key(b"b"));
}

// Silence unused imports in some build configurations.
#[allow(dead_code)]
fn _write_one_byte(path: &std::path::Path, at: u64, b: u8) {
    let mut f = OpenOptions::new().write(true).open(path).unwrap();
    f.seek(SeekFrom::Start(at)).unwrap();
    f.write_all(&[b]).unwrap();
}
