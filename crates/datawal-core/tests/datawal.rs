//! Integration tests for `DataWal` (bytes-first KV).
//!
//! Covers the 9 cases required by the v0.1-pre spec.

use datawal::DataWal;
use tempfile::tempdir;

// ---------------------------------------------------------------------------
// 1. put_get_roundtrip
// ---------------------------------------------------------------------------
#[test]
fn put_get_roundtrip() {
    let dir = tempdir().unwrap();
    let mut kv = DataWal::open(dir.path()).unwrap();
    kv.put(b"k1", b"v1").unwrap();
    kv.put(b"k2", b"v2").unwrap();
    assert_eq!(kv.get(b"k1").unwrap().as_deref(), Some(&b"v1"[..]));
    assert_eq!(kv.get(b"k2").unwrap().as_deref(), Some(&b"v2"[..]));
    assert_eq!(kv.get(b"missing").unwrap(), None);
    assert_eq!(kv.len(), 2);
    assert!(!kv.is_empty());
}

// ---------------------------------------------------------------------------
// 2. put_overwrites_last_write_wins
// ---------------------------------------------------------------------------
#[test]
fn put_overwrites_last_write_wins() {
    let dir = tempdir().unwrap();
    let mut kv = DataWal::open(dir.path()).unwrap();
    kv.put(b"k", b"v1").unwrap();
    kv.put(b"k", b"v2").unwrap();
    kv.put(b"k", b"v3").unwrap();
    assert_eq!(kv.get(b"k").unwrap().as_deref(), Some(&b"v3"[..]));
    assert_eq!(kv.len(), 1);
}

// ---------------------------------------------------------------------------
// 3. delete_removes_key
// ---------------------------------------------------------------------------
#[test]
fn delete_removes_key() {
    let dir = tempdir().unwrap();
    let mut kv = DataWal::open(dir.path()).unwrap();
    kv.put(b"k", b"v").unwrap();
    assert!(kv.contains_key(b"k"));
    kv.delete(b"k").unwrap();
    assert!(!kv.contains_key(b"k"));
    assert_eq!(kv.get(b"k").unwrap(), None);
    assert_eq!(kv.len(), 0);
    assert!(kv.is_empty());
}

// ---------------------------------------------------------------------------
// 4. put_after_delete_resurrects_new_value
// ---------------------------------------------------------------------------
#[test]
fn put_after_delete_resurrects_new_value() {
    let dir = tempdir().unwrap();
    let mut kv = DataWal::open(dir.path()).unwrap();
    kv.put(b"k", b"old").unwrap();
    kv.delete(b"k").unwrap();
    kv.put(b"k", b"new").unwrap();
    assert_eq!(kv.get(b"k").unwrap().as_deref(), Some(&b"new"[..]));
    assert_eq!(kv.len(), 1);
}

// ---------------------------------------------------------------------------
// 5. len_counts_live_keys_only
// ---------------------------------------------------------------------------
#[test]
fn len_counts_live_keys_only() {
    let dir = tempdir().unwrap();
    let mut kv = DataWal::open(dir.path()).unwrap();
    kv.put(b"a", b"1").unwrap();
    kv.put(b"b", b"2").unwrap();
    kv.put(b"c", b"3").unwrap();
    kv.delete(b"b").unwrap();
    kv.put(b"a", b"1-bis").unwrap();
    assert_eq!(kv.len(), 2);
    let mut keys = kv.keys();
    keys.sort();
    assert_eq!(keys, vec![b"a".to_vec(), b"c".to_vec()]);
}

// ---------------------------------------------------------------------------
// 6. open_rebuilds_keydir_from_log
// ---------------------------------------------------------------------------
#[test]
fn open_rebuilds_keydir_from_log() {
    let dir = tempdir().unwrap();
    {
        let mut kv = DataWal::open(dir.path()).unwrap();
        kv.put(b"a", b"1").unwrap();
        kv.put(b"b", b"2").unwrap();
        kv.put(b"a", b"3").unwrap();
        kv.delete(b"b").unwrap();
        kv.fsync().unwrap();
    }
    let mut kv = DataWal::open(dir.path()).unwrap();
    assert_eq!(kv.len(), 1);
    assert_eq!(kv.get(b"a").unwrap().as_deref(), Some(&b"3"[..]));
    assert_eq!(kv.get(b"b").unwrap(), None);
    assert!(!kv.contains_key(b"b"));
}

// ---------------------------------------------------------------------------
// 7. compact_to_preserves_live_state
// ---------------------------------------------------------------------------
#[test]
fn compact_to_preserves_live_state() {
    let src = tempdir().unwrap();
    let dst = tempdir().unwrap();
    let dst_path = dst.path().join("compacted");

    let mut kv = DataWal::open(src.path()).unwrap();
    kv.put(b"a", b"1").unwrap();
    kv.put(b"b", b"2").unwrap();
    kv.put(b"a", b"3").unwrap();
    kv.put(b"c", b"4").unwrap();
    kv.fsync().unwrap();

    let stats = kv.compact_to(&dst_path).unwrap();
    assert_eq!(stats.live_keys, 3);
    assert_eq!(stats.records_written, 3);
    assert!(stats.bytes_written > 0);

    let mut kv2 = DataWal::open(&dst_path).unwrap();
    assert_eq!(kv2.len(), 3);
    assert_eq!(kv2.get(b"a").unwrap().as_deref(), Some(&b"3"[..]));
    assert_eq!(kv2.get(b"b").unwrap().as_deref(), Some(&b"2"[..]));
    assert_eq!(kv2.get(b"c").unwrap().as_deref(), Some(&b"4"[..]));
}

// ---------------------------------------------------------------------------
// 8. compact_to_does_not_resurrect_deleted_key
// ---------------------------------------------------------------------------
#[test]
fn compact_to_does_not_resurrect_deleted_key() {
    let src = tempdir().unwrap();
    let dst = tempdir().unwrap();
    let dst_path = dst.path().join("compacted");

    let mut kv = DataWal::open(src.path()).unwrap();
    kv.put(b"keep", b"1").unwrap();
    kv.put(b"gone", b"2").unwrap();
    kv.delete(b"gone").unwrap();
    kv.fsync().unwrap();

    kv.compact_to(&dst_path).unwrap();

    let kv2 = DataWal::open(&dst_path).unwrap();
    assert_eq!(kv2.len(), 1);
    assert!(kv2.contains_key(b"keep"));
    assert!(!kv2.contains_key(b"gone"));
}

// ---------------------------------------------------------------------------
// 9. export_jsonl_contains_live_keys_only
// ---------------------------------------------------------------------------
#[test]
fn export_jsonl_contains_live_keys_only() {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine as _;

    let dir = tempdir().unwrap();
    let out = tempdir().unwrap();
    let out_path = out.path().join("export.jsonl");

    let mut kv = DataWal::open(dir.path()).unwrap();
    kv.put(b"a", b"alpha").unwrap();
    kv.put(b"b", b"beta").unwrap();
    kv.put(b"a", b"alpha2").unwrap();
    kv.delete(b"b").unwrap();
    kv.fsync().unwrap();

    kv.export_jsonl(&out_path).unwrap();
    let body = std::fs::read_to_string(&out_path).unwrap();
    let lines: Vec<&str> = body.lines().collect();
    assert_eq!(lines.len(), 1, "only one live key should be exported");
    let line = lines[0];
    let v: serde_json::Value = serde_json::from_str(line).unwrap();
    let k_b64 = v["key_b64"].as_str().unwrap();
    let v_b64 = v["value_b64"].as_str().unwrap();
    assert_eq!(STANDARD.decode(k_b64).unwrap(), b"a");
    assert_eq!(STANDARD.decode(v_b64).unwrap(), b"alpha2");
}
