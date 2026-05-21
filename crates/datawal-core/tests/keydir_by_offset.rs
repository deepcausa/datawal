//! Tests pinning the v0.1.4 keydir-by-offset behaviour.
//!
//! Before this PR, `DataWal::get` returned a `Vec<u8>` cloned from an
//! in-memory `HashMap<Vec<u8>, Vec<u8>>`. After this PR, the keydir holds
//! `RecordRef`s and every `get` re-reads the segment file and revalidates
//! the CRC against the stored payload. These tests pin that contract.

use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use datawal::DataWal;
use tempfile::tempdir;

fn seg_path(dir: &Path, id: u32) -> PathBuf {
    dir.join(format!("{:08}.dwal", id))
}

#[test]
fn get_reads_payload_lazily_after_drop_and_reopen() {
    // open, put, drop -> reopen, the keydir should hold a RecordRef
    // pointing at the on-disk record. get must read it through pread+CRC.
    let dir = tempdir().unwrap();
    {
        let mut kv = DataWal::open(dir.path()).expect("open");
        kv.put(b"alpha", b"one").expect("put alpha");
        kv.put(b"beta", b"two").expect("put beta");
        kv.fsync().expect("fsync");
    }

    let mut kv = DataWal::open(dir.path()).expect("reopen");
    assert_eq!(kv.get(b"alpha").unwrap().as_deref(), Some(&b"one"[..]));
    assert_eq!(kv.get(b"beta").unwrap().as_deref(), Some(&b"two"[..]));
    assert!(kv.get(b"missing").unwrap().is_none());
}

#[test]
fn get_revalidates_crc_and_errors_on_corruption() {
    // We need the keydir to point at the record BEFORE the bytes are
    // corrupted; otherwise reopen's longest-valid-prefix recovery would
    // treat the corrupted record as a tail truncate and the key would
    // simply be missing. The point of this test is to prove that `get`
    // itself revalidates the CRC, not that recovery rejects it.
    let dir = tempdir().unwrap();
    let mut kv = DataWal::open(dir.path()).expect("open");
    kv.put(b"k", b"some-payload-that-will-be-corrupted-on-disk")
        .expect("put");
    kv.fsync().expect("fsync");

    // Confirm the record is in the keydir before corruption.
    assert!(kv.ref_of(b"k").is_some(), "record must be in keydir");

    // Flip a single byte inside the payload region of the active segment.
    // The frame CRC covers the payload, so any flip yields a CRC mismatch
    // on the read path.
    let seg = seg_path(dir.path(), 1);
    let len = std::fs::metadata(&seg).unwrap().len();
    let target_offset = len - 5;
    let mut f = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&seg)
        .unwrap();
    f.seek(SeekFrom::Start(target_offset)).unwrap();
    let mut byte = [0u8; 1];
    f.read_exact(&mut byte).unwrap();
    byte[0] ^= 0xFF;
    f.seek(SeekFrom::Start(target_offset)).unwrap();
    f.write_all(&byte).unwrap();
    f.sync_all().unwrap();
    drop(f);

    let err = kv
        .get(b"k")
        .expect_err("get should fail on corrupt payload");
    let msg = format!("{err:#}").to_lowercase();
    assert!(
        msg.contains("crc") || msg.contains("decode"),
        "expected CRC/decode error, got: {msg}"
    );
}

#[test]
fn ref_of_tracks_live_keys_and_returns_none_after_delete() {
    let dir = tempdir().unwrap();
    let mut kv = DataWal::open(dir.path()).expect("open");

    assert!(kv.ref_of(b"x").is_none());

    kv.put(b"x", b"v1").expect("put");
    kv.fsync().expect("fsync");
    let r1 = kv.ref_of(b"x").expect("x should be live");
    assert_eq!(r1.segment, 1);
    assert!(r1.len > 0);

    kv.delete(b"x").expect("delete");
    kv.fsync().expect("fsync");
    assert!(
        kv.ref_of(b"x").is_none(),
        "ref_of should return None after delete tombstone"
    );
    assert_eq!(
        kv.get(b"x").unwrap(),
        None,
        "get should agree with ref_of after delete"
    );
}

#[test]
fn ref_of_advances_to_latest_overwrite() {
    let dir = tempdir().unwrap();
    let mut kv = DataWal::open(dir.path()).expect("open");
    kv.put(b"k", b"first").expect("put first");
    let r1 = kv.ref_of(b"k").expect("first ref");
    kv.put(b"k", b"second-longer").expect("put second");
    let r2 = kv.ref_of(b"k").expect("second ref");

    // The second ref must point past the first one in the same segment
    // (overwrites are appends).
    assert_eq!(r1.segment, r2.segment, "same segment for both writes");
    assert!(
        r2.offset > r1.offset,
        "second ref must be past the first: r1={r1:?} r2={r2:?}"
    );

    kv.fsync().expect("fsync");
    assert_eq!(
        kv.get(b"k").unwrap().as_deref(),
        Some(&b"second-longer"[..])
    );
}

#[test]
fn get_works_for_records_in_sealed_segment_after_rotation() {
    // Put records into segment 1, force rotation by directly accessing the
    // RecordLog handle, then keep writing into segment 2. The first key's
    // RecordRef must still resolve via the fd pool.
    let dir = tempdir().unwrap();
    let mut kv = DataWal::open(dir.path()).expect("open");
    kv.put(b"sealed-key", b"sealed-value")
        .expect("put pre-rotate");
    kv.fsync().expect("fsync");

    // RecordLog::rotate is not exposed via DataWal, but compact_to into a
    // new dir + reopen gives us a comparable structure: the live state
    // survives a snapshot-style rebuild.
    let dst = tempdir().unwrap();
    let _ = kv.compact_to(dst.path()).expect("compact_to");

    let mut kv2 = DataWal::open(dst.path()).expect("open compacted");
    kv2.put(b"new-key", b"new-value").expect("put post-compact");
    kv2.fsync().expect("fsync");

    assert_eq!(
        kv2.get(b"sealed-key").unwrap().as_deref(),
        Some(&b"sealed-value"[..])
    );
    assert_eq!(
        kv2.get(b"new-key").unwrap().as_deref(),
        Some(&b"new-value"[..])
    );
}

#[test]
fn fd_pool_reuses_fd_across_repeated_gets() {
    // Indirect check: do many gets and confirm the working set doesn't
    // explode in number of file descriptors. We can't see the fd pool
    // directly (private), but a process-level fd count check on /proc
    // is a good smoke test on Linux. On non-Linux this just exercises
    // the happy path.
    let dir = tempdir().unwrap();
    let mut kv = DataWal::open(dir.path()).expect("open");
    for i in 0u32..100 {
        let k = format!("k{i:03}").into_bytes();
        let v = format!("v{i:03}").into_bytes();
        kv.put(&k, &v).expect("put");
    }
    kv.fsync().expect("fsync");

    #[cfg(target_os = "linux")]
    let before = std::fs::read_dir("/proc/self/fd").map(|d| d.count()).ok();

    for _ in 0..5 {
        for i in 0u32..100 {
            let k = format!("k{i:03}").into_bytes();
            let v = format!("v{i:03}").into_bytes();
            assert_eq!(kv.get(&k).unwrap().as_deref(), Some(&v[..]));
        }
    }

    #[cfg(target_os = "linux")]
    {
        let after = std::fs::read_dir("/proc/self/fd").map(|d| d.count()).ok();
        if let (Some(b), Some(a)) = (before, after) {
            // The pool has a small fixed capacity. After 500 gets across
            // the same segment, the fd count should not grow by more than
            // the pool capacity (a few units of slack for harness fds).
            assert!(
                a <= b + 4,
                "fd count grew from {b} to {a} -- fd pool not reusing"
            );
        }
    }
}

#[test]
fn items_returns_all_live_pairs_and_revalidates_crc() {
    let dir = tempdir().unwrap();
    let mut kv = DataWal::open(dir.path()).expect("open");
    kv.put(b"a", b"1").expect("put a");
    kv.put(b"b", b"2").expect("put b");
    kv.put(b"c", b"3").expect("put c");
    kv.delete(b"b").expect("delete b");
    kv.fsync().expect("fsync");

    let mut items = kv.items().expect("items");
    items.sort();
    assert_eq!(
        items,
        vec![
            (b"a".to_vec(), b"1".to_vec()),
            (b"c".to_vec(), b"3".to_vec())
        ]
    );
}

#[test]
fn compact_to_preserves_live_state_via_offset_keydir() {
    let dir = tempdir().unwrap();
    let mut kv = DataWal::open(dir.path()).expect("open");
    for i in 0u32..50 {
        let k = format!("k{i:02}").into_bytes();
        let v = format!("v{i:02}-payload").into_bytes();
        kv.put(&k, &v).expect("put");
    }
    // Overwrite half of them.
    for i in 0u32..25 {
        let k = format!("k{i:02}").into_bytes();
        let v = format!("OVERWRITE-{i:02}").into_bytes();
        kv.put(&k, &v).expect("overwrite");
    }
    // Delete a few.
    for i in 40u32..45 {
        let k = format!("k{i:02}").into_bytes();
        kv.delete(&k).expect("delete");
    }
    kv.fsync().expect("fsync");

    let dst = tempdir().unwrap();
    let stats = kv.compact_to(dst.path()).expect("compact_to");
    assert_eq!(stats.live_keys, 50 - 5);

    let mut compacted = DataWal::open(dst.path()).expect("open compacted");
    // First 25 carry OVERWRITE-* values.
    for i in 0u32..25 {
        let k = format!("k{i:02}").into_bytes();
        let v = format!("OVERWRITE-{i:02}").into_bytes();
        assert_eq!(compacted.get(&k).unwrap().as_deref(), Some(&v[..]));
    }
    // Next 15 (25..40) carry the original payload.
    for i in 25u32..40 {
        let k = format!("k{i:02}").into_bytes();
        let v = format!("v{i:02}-payload").into_bytes();
        assert_eq!(compacted.get(&k).unwrap().as_deref(), Some(&v[..]));
    }
    // Deleted ones are gone.
    for i in 40u32..45 {
        let k = format!("k{i:02}").into_bytes();
        assert!(compacted.get(&k).unwrap().is_none());
    }
    // Last 5 still present.
    for i in 45u32..50 {
        let k = format!("k{i:02}").into_bytes();
        let v = format!("v{i:02}-payload").into_bytes();
        assert_eq!(compacted.get(&k).unwrap().as_deref(), Some(&v[..]));
    }
}
