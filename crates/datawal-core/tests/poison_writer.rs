//! Tests for the writer-poisoning contract on [`RecordLog`].
//!
//! Poisoning is set on the live handle whenever a mutating I/O path
//! (`append_record`, `fsync`, `rotate`) fails after the kernel may
//! already have accepted some bytes. Once set:
//!
//! - Every subsequent mutating call returns an `anyhow::Error` whose
//!   `Display` starts with `datawal: writer poisoned:` and ends with
//!   `; drop handle and reopen`. The intermediate `<reason>` is
//!   diagnostic and not part of the contract.
//! - Read-only methods (`scan_iter`, `recovery_report`,
//!   `active_segment`, `dir`, `is_poisoned`) still work.
//! - After dropping the handle, `RecordLog::open` recovers via the
//!   normal longest-valid-prefix path and the resulting log is
//!   fully writable again.
//!
//! Forcing a real write/fsync/rotate I/O failure on the live handle
//! is platform-specific (`ENOSPC`, `EIO`, etc.) and is exercised by
//! the dedicated `disk_full.rs` test. This file uses a small
//! white-box helper (`record_log::testing::poison_for_test`, gated on
//! `cfg(test)`) to set the poison flag synthetically so that the
//! contract itself can be tested deterministically on any host.

use datawal::format::RecordType;
use datawal::RecordLog;
use tempfile::tempdir;

/// Helper: open a log, append two `Put` records, fsync, return the
/// handle.
fn seed_log(dir: &std::path::Path) -> RecordLog {
    let mut log = RecordLog::open(dir).expect("open fresh log");
    log.append_record(RecordType::Put, b"a", b"1")
        .expect("seed append a");
    log.append_record(RecordType::Put, b"b", b"22")
        .expect("seed append b");
    log.fsync().expect("seed fsync");
    log
}

#[test]
fn fresh_log_is_not_poisoned() {
    let tmp = tempdir().unwrap();
    let log = seed_log(tmp.path());
    assert!(!log.is_poisoned(), "fresh log must not be poisoned");
}

#[test]
fn append_failure_poisons_subsequent_appends() {
    let tmp = tempdir().unwrap();
    let mut log = seed_log(tmp.path());

    // Force the live handle into the poisoned state without a real
    // I/O failure (covered separately by `disk_full.rs`).
    datawal::testing::poison_record_log_for_test(&mut log, "append_record write_all failed");
    assert!(log.is_poisoned());

    let err = log
        .append_record(RecordType::Put, b"c", b"333")
        .expect_err("append after poison must fail");
    let msg = format!("{err}");
    assert!(
        msg.starts_with("datawal: writer poisoned: "),
        "unexpected poison message prefix: {msg}"
    );
    assert!(
        msg.ends_with("; drop handle and reopen"),
        "unexpected poison message suffix: {msg}"
    );
}

#[test]
fn fsync_and_rotate_also_return_poison_error() {
    let tmp = tempdir().unwrap();
    let mut log = seed_log(tmp.path());
    datawal::testing::poison_record_log_for_test(&mut log, "synthetic");

    let fsync_err = log.fsync().expect_err("fsync must fail when poisoned");
    assert!(format!("{fsync_err}").starts_with("datawal: writer poisoned: "));

    let rotate_err = log.rotate().expect_err("rotate must fail when poisoned");
    assert!(format!("{rotate_err}").starts_with("datawal: writer poisoned: "));
}

#[test]
fn read_only_ops_remain_available_after_poison() {
    let tmp = tempdir().unwrap();
    let mut log = seed_log(tmp.path());
    datawal::testing::poison_record_log_for_test(&mut log, "synthetic");

    // `active_segment`, `dir`, `is_poisoned`: pure accessors.
    assert!(log.is_poisoned());
    assert_eq!(log.active_segment(), 1);
    assert_eq!(log.dir(), tmp.path());

    // `recovery_report`: returns the cached report from open().
    let report = log
        .recovery_report()
        .expect("recovery_report must work after poison");
    assert_eq!(report.files_scanned, 1);

    // `scan_iter`: lazy iterator on the live segments.
    let iter = log.scan_iter().expect("scan_iter after poison");
    let collected: Vec<_> = iter.map(|r| r.expect("record decode")).collect();
    assert_eq!(collected.len(), 2);
    assert_eq!(collected[0].key, b"a");
    assert_eq!(collected[1].key, b"b");
}

#[test]
fn reopen_after_drop_clears_poison_and_recovers_prefix() {
    let tmp = tempdir().unwrap();
    let dir = tmp.path().to_path_buf();

    {
        let mut log = seed_log(&dir);
        datawal::testing::poison_record_log_for_test(&mut log, "synthetic");
        assert!(log.is_poisoned());
        // Handle drops here, releasing the fs2 lock.
    }

    // Reopen: longest-valid-prefix recovery on the bytes that are on
    // disk. No partial tail was written (we poisoned synthetically),
    // so all seeded records survive.
    let mut log2 = RecordLog::open(&dir).expect("reopen after drop");
    assert!(!log2.is_poisoned(), "reopen must start unpoisoned");

    let records: Vec<_> = log2
        .scan_iter()
        .expect("scan_iter after reopen")
        .map(|r| r.expect("decode"))
        .collect();
    assert_eq!(records.len(), 2);

    // The reopened handle is fully writable again.
    log2.append_record(RecordType::Put, b"c", b"333")
        .expect("append after reopen");
    log2.fsync().expect("fsync after reopen");

    let n = log2.scan_iter().unwrap().count();
    assert_eq!(n, 3);
}

#[test]
fn poison_message_is_stable_format() {
    // The exact format is part of the public contract documented on
    // `RecordLog`. The `<reason>` is diagnostic and may change; the
    // prefix and suffix may not.
    let tmp = tempdir().unwrap();
    let mut log = seed_log(tmp.path());
    datawal::testing::poison_record_log_for_test(&mut log, "append_record write_all failed");

    let err = log.append_record(RecordType::Put, b"x", b"1").unwrap_err();
    let msg = format!("{err}");
    assert_eq!(
        msg, "datawal: writer poisoned: append_record write_all failed; drop handle and reopen",
        "the exact wording is part of the public contract"
    );
}
