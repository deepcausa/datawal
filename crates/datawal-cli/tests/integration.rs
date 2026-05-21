//! Integration tests for the `datawal` binary.
//!
//! Each test constructs a fresh datawal store via the `datawal` library
//! crate, then invokes the CLI binary via `assert_cmd` against it.
//!
//! The store directory is dropped immediately before the assertion
//! step so the binary acquires the cooperative single-writer lock
//! without contention.

use std::path::Path;
use std::process::Command;

use assert_cmd::prelude::*;
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use datawal::{DataWal, RecordLog};
use predicates::prelude::*;
use serde_json::Value;
use tempfile::TempDir;

fn bin() -> Command {
    Command::cargo_bin("datawal").expect("datawal binary present")
}

fn populate_kv(dir: &Path, pairs: &[(&[u8], &[u8])]) {
    let mut wal = DataWal::open(dir).expect("open DataWal");
    for (k, v) in pairs {
        wal.put(k, v).expect("put");
    }
    wal.fsync().expect("fsync");
    // Drop wal here, releasing the lock.
}

fn populate_raw(dir: &Path, payloads: &[&[u8]]) {
    let mut log = RecordLog::open(dir).expect("open RecordLog");
    for p in payloads {
        log.append(p).expect("append");
    }
    log.fsync().expect("fsync");
}

fn parse_json_lines(out: &[u8]) -> Vec<Value> {
    std::str::from_utf8(out)
        .expect("utf-8 stdout")
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str::<Value>(l).expect("valid JSON line"))
        .collect()
}

// -----------------------------------------------------------------
// scan
// -----------------------------------------------------------------

#[test]
fn scan_emits_one_record_per_put() {
    let tmp = TempDir::new().unwrap();
    populate_kv(tmp.path(), &[(b"alpha", b"1"), (b"beta", b"22")]);

    let out = bin()
        .args(["--json", "scan"])
        .arg(tmp.path())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let lines = parse_json_lines(&out);
    assert_eq!(lines.len(), 2);
    for line in &lines {
        assert_eq!(line["schema"], "datawal.cli.v1");
        assert_eq!(line["kind"], "record");
        assert_eq!(line["record_type"], "Put");
    }
    let keys: Vec<String> = lines
        .iter()
        .map(|l| l["key_base64"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(keys[0], B64.encode(b"alpha"));
    assert_eq!(keys[1], B64.encode(b"beta"));
}

#[test]
fn scan_respects_limit() {
    let tmp = TempDir::new().unwrap();
    populate_kv(
        tmp.path(),
        &[(b"a", b"1"), (b"b", b"2"), (b"c", b"3"), (b"d", b"4")],
    );

    let out = bin()
        .args(["--json", "scan", "--limit", "2"])
        .arg(tmp.path())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let lines = parse_json_lines(&out);
    assert_eq!(lines.len(), 2);
}

#[test]
fn scan_human_form_is_one_line_per_record() {
    let tmp = TempDir::new().unwrap();
    populate_kv(tmp.path(), &[(b"k1", b"v1"), (b"k2", b"v2")]);

    bin()
        .args(["scan"])
        .arg(tmp.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("type=PUT").count(2));
}

// -----------------------------------------------------------------
// get
// -----------------------------------------------------------------

#[test]
fn get_hit_returns_value_base64() {
    let tmp = TempDir::new().unwrap();
    populate_kv(tmp.path(), &[(b"hello", b"world")]);

    let out = bin()
        .args(["--json", "get"])
        .arg(tmp.path())
        .args(["--key-base64", &B64.encode(b"hello")])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let v: Value = serde_json::from_slice(out.trim_ascii_end()).unwrap();
    assert_eq!(v["schema"], "datawal.cli.v1");
    assert_eq!(v["kind"], "value");
    assert_eq!(v["value_base64"], B64.encode(b"world"));
    assert_eq!(v["value_len"], 5);
}

#[test]
fn get_miss_exits_2() {
    let tmp = TempDir::new().unwrap();
    populate_kv(tmp.path(), &[(b"present", b"yes")]);

    let assertion = bin()
        .args(["--json", "get"])
        .arg(tmp.path())
        .args(["--key-base64", &B64.encode(b"absent")])
        .assert()
        .code(2);
    let out = assertion.get_output().stdout.clone();
    let v: Value = serde_json::from_slice(out.trim_ascii_end()).unwrap();
    assert_eq!(v["kind"], "miss");
}

#[test]
fn get_accepts_key_hex() {
    let tmp = TempDir::new().unwrap();
    populate_kv(tmp.path(), &[(&[0xDE, 0xAD, 0xBE, 0xEF], b"hex-keyed")]);

    bin()
        .args(["--json", "get"])
        .arg(tmp.path())
        .args(["--key-hex", "deadbeef"])
        .assert()
        .success();
}

#[test]
fn get_rejects_bad_encoding() {
    let tmp = TempDir::new().unwrap();
    populate_kv(tmp.path(), &[(b"k", b"v")]);

    bin()
        .args(["get"])
        .arg(tmp.path())
        .args(["--key-base64", "!!!not-base64!!!"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("invalid --key-base64"));
}

#[test]
fn get_requires_a_key_arg() {
    let tmp = TempDir::new().unwrap();
    populate_kv(tmp.path(), &[(b"k", b"v")]);

    bin()
        .args(["get"])
        .arg(tmp.path())
        .assert()
        .failure()
        .stderr(predicate::str::contains("pass --key-base64 or --key-hex"));
}

// -----------------------------------------------------------------
// report
// -----------------------------------------------------------------

#[test]
fn report_on_clean_log() {
    let tmp = TempDir::new().unwrap();
    populate_kv(tmp.path(), &[(b"a", b"1"), (b"b", b"2")]);

    let out = bin()
        .args(["--json", "report"])
        .arg(tmp.path())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: Value = serde_json::from_slice(out.trim_ascii_end()).unwrap();
    assert_eq!(v["schema"], "datawal.cli.v1");
    assert_eq!(v["kind"], "report");
    assert_eq!(v["files_scanned"], 1);
    assert_eq!(v["records_replayed"], 2);
    assert_eq!(v["tail_truncated"], 0);
    assert_eq!(v["mid_stream_errors"], 0);
    assert_eq!(v["unsupported_versions"], 0);
}

// -----------------------------------------------------------------
// verify
// -----------------------------------------------------------------

#[test]
fn verify_clean_store_succeeds() {
    let tmp = TempDir::new().unwrap();
    populate_kv(tmp.path(), &[(b"x", b"1"), (b"y", b"2"), (b"z", b"3")]);

    let out = bin()
        .args(["--json", "verify"])
        .arg(tmp.path())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: Value = serde_json::from_slice(out.trim_ascii_end()).unwrap();
    assert_eq!(v["kind"], "verify");
    assert_eq!(v["frames_checked"], 3);
    assert_eq!(v["crc_failures"], 0);
    assert_eq!(v["tail_truncated"], 0);
}

// -----------------------------------------------------------------
// dump
// -----------------------------------------------------------------

#[test]
fn dump_emits_frame_kind_without_payload() {
    let tmp = TempDir::new().unwrap();
    populate_raw(tmp.path(), &[b"raw-payload-1", b"raw-payload-2"]);

    let out = bin()
        .args(["--json", "dump"])
        .arg(tmp.path())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let lines = parse_json_lines(&out);
    assert_eq!(lines.len(), 2);
    for line in &lines {
        assert_eq!(line["kind"], "frame");
        assert_eq!(line["record_type"], "Raw");
        // No payload bytes in dump output.
        assert!(line.get("payload_base64").is_none());
        assert!(line.get("payload_len").is_some());
    }
}

#[test]
fn dump_respects_limit() {
    let tmp = TempDir::new().unwrap();
    populate_raw(tmp.path(), &[b"a", b"b", b"c", b"d"]);

    let out = bin()
        .args(["--json", "dump", "--limit", "2"])
        .arg(tmp.path())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let lines = parse_json_lines(&out);
    assert_eq!(lines.len(), 2);
}

// -----------------------------------------------------------------
// concurrency / lock
// -----------------------------------------------------------------

#[test]
fn store_locked_by_other_process_fails_clearly() {
    let tmp = TempDir::new().unwrap();
    populate_kv(tmp.path(), &[(b"k", b"v")]);

    // Hold the lock by keeping a live `RecordLog` open.
    let _hold = RecordLog::open(tmp.path()).unwrap();

    bin()
        .args(["scan"])
        .arg(tmp.path())
        .assert()
        .failure()
        .stderr(predicate::str::contains("error"));
}

#[test]
fn nonexistent_store_dir_creates_then_reports_empty() {
    // Matches `RecordLog::open` semantics: it creates the directory
    // if missing. We do not promise the inspector refuses to create
    // — that contract belongs to the library, not the CLI.
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("fresh");

    let out = bin()
        .args(["--json", "report"])
        .arg(&dir)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: Value = serde_json::from_slice(out.trim_ascii_end()).unwrap();
    assert_eq!(v["records_replayed"], 0);
}
