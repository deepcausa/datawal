//! Randomised properties for `RecordLog` recovery and the `DataWal`
//! KV projection.
//!
//! Properties are framed against the existing public API (put/delete/
//! get/keys/compact_to/scan_iter). The strategies generate short
//! sequences (length 0..32) of mixed `put` / `delete` / `fsync` / `rotate`
//! / `reopen` / `compact_to` operations on a small key universe (8
//! single-byte keys), enough to exercise interleavings without
//! blowing test time.
//!
//! Fixed seed (`PROPTEST_CASES=64`) keeps the runtime modest on the
//! MSRV CI lane. To explore more, run locally with
//! `PROPTEST_CASES=2048 cargo test -p datawal --test proptest_recovery`.

use std::collections::BTreeMap;
use std::path::Path;

use datawal::format::RecordType;
use datawal::{DataWal, RecordLog};
use proptest::collection::vec;
use proptest::prelude::*;
use tempfile::tempdir;

/// One operation in a generated sequence. The on-disk side effects
/// happen via the public API; `Reopen` drops and re-opens the
/// underlying handle to exercise recovery.
#[derive(Debug, Clone)]
enum Op {
    Put { key: u8, val: Vec<u8> },
    Delete { key: u8 },
    Fsync,
    Reopen,
    Rotate,
}

/// Strategy: 8 possible keys, payloads up to 16 bytes, weighted ops.
fn op_strategy() -> impl Strategy<Value = Op> {
    let key = 0u8..8u8;
    let val = vec(any::<u8>(), 0..16);
    prop_oneof![
        4 => (key.clone(), val).prop_map(|(k, v)| Op::Put { key: k, val: v }),
        2 => key.prop_map(|k| Op::Delete { key: k }),
        2 => Just(Op::Fsync),
        1 => Just(Op::Reopen),
        1 => Just(Op::Rotate),
    ]
}

fn seq_strategy() -> impl Strategy<Value = Vec<Op>> {
    vec(op_strategy(), 0..32)
}

/// Build the expected in-memory KV state by replaying the sequence
/// in-process. `Fsync`, `Reopen`, `Rotate` do not change KV state by
/// themselves; they only affect on-disk durability. This is the
/// oracle.
fn replay_expected(seq: &[Op]) -> BTreeMap<Vec<u8>, Vec<u8>> {
    let mut state = BTreeMap::new();
    for op in seq {
        match op {
            Op::Put { key, val } => {
                state.insert(vec![*key], val.clone());
            }
            Op::Delete { key } => {
                state.remove(&vec![*key][..]);
            }
            Op::Fsync | Op::Reopen | Op::Rotate => {}
        }
    }
    state
}

/// Apply the sequence to a real `DataWal` rooted at `dir`, returning
/// the final live handle. `Reopen` is implemented as drop + reopen.
fn apply_seq_to_datawal(dir: &Path, seq: &[Op]) -> DataWal {
    let mut wal = DataWal::open(dir).expect("open");
    for op in seq {
        match op {
            Op::Put { key, val } => {
                wal.put(&[*key], val).expect("put");
            }
            Op::Delete { key } => {
                wal.delete(&[*key]).expect("delete");
            }
            Op::Fsync => {
                // `DataWal` exposes durability via the underlying log;
                // we can reach the same effect by reopening, but for
                // mid-sequence fsync we just skip the explicit
                // disk-sync because `put`/`delete` already wrote the
                // frame and a `Reopen` further down (or end-of-seq
                // reopen, see below) will replay it. The point of
                // generating `Fsync` is to interleave it with
                // `Rotate`/`Reopen` in the *RecordLog* property
                // below; for the KV oracle it is a no-op.
            }
            Op::Reopen => {
                drop(wal);
                wal = DataWal::open(dir).expect("reopen");
            }
            Op::Rotate => {
                // `DataWal` does not expose `rotate` directly. The
                // KV oracle does not care about segment boundaries,
                // so treat it as a no-op for this property. The
                // dedicated RecordLog property below exercises
                // rotate explicitly.
            }
        }
    }
    wal
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 64,
        // Keep the failure persistence file inside the per-target
        // build dir so repeated runs don't pollute the source tree.
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    /// **Property: reopen preserves the live KV state.**
    ///
    /// Applying the random op sequence to a `DataWal`, dropping the
    /// handle, reopening, and reading every key back must yield
    /// exactly the oracle map. This stresses the longest-valid-prefix
    /// recovery path against arbitrary put/delete interleavings.
    #[test]
    fn reopen_preserves_kv_state(seq in seq_strategy()) {
        let tmp = tempdir().unwrap();
        // Apply, then force one final reopen.
        {
            let _ = apply_seq_to_datawal(tmp.path(), &seq);
        }
        let wal = DataWal::open(tmp.path()).expect("reopen at end");

        let expected = replay_expected(&seq);
        let mut got: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
        for k in wal.keys() {
            let v = wal.get(&k).expect("get").expect("present");
            got.insert(k, v);
        }
        prop_assert_eq!(got, expected);
    }

    /// **Property: delete-then-put resurrects only the new value.**
    ///
    /// For every key in the universe, after `delete(k); put(k, v)`,
    /// the live state is `{k: v}` regardless of earlier history.
    #[test]
    fn delete_then_put_resurrects_only_new_value(
        prefix in seq_strategy(),
        k in 0u8..8u8,
        v in vec(any::<u8>(), 0..16),
    ) {
        let tmp = tempdir().unwrap();
        {
            let mut wal = apply_seq_to_datawal(tmp.path(), &prefix);
            wal.delete(&[k]).unwrap();
            wal.put(&[k], &v).unwrap();
        }
        let wal = DataWal::open(tmp.path()).unwrap();
        let got = wal.get(&[k]).unwrap();
        prop_assert_eq!(got, Some(v));
    }

    /// **Property: `compact_to` preserves the live state.**
    ///
    /// Compaction is a snapshot-style rebuild: opening the target
    /// directory must yield exactly the same KV map as the source.
    /// The source itself is read-only during compaction; reopening
    /// it must also yield the same map (sanity).
    #[test]
    fn compact_to_preserves_kv_state(seq in seq_strategy()) {
        let src = tempdir().unwrap();
        let out = tempdir().unwrap();
        // `compact_to` requires an empty target dir, so use a
        // never-created subpath.
        let out_dir = out.path().join("compacted");

        let stats_keys = {
            let _ = apply_seq_to_datawal(src.path(), &seq);
            let src_wal = DataWal::open(src.path()).expect("reopen src");
            src_wal.compact_to(&out_dir).expect("compact_to");
            src_wal.keys()
        };

        let compacted = DataWal::open(&out_dir).expect("open compacted");
        let mut got: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
        for k in compacted.keys() {
            got.insert(k.clone(), compacted.get(&k).unwrap().unwrap());
        }
        let mut expected: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
        let src_again = DataWal::open(src.path()).expect("reopen src again");
        for k in stats_keys {
            expected.insert(k.clone(), src_again.get(&k).unwrap().unwrap());
        }
        prop_assert_eq!(got, expected);
    }

    /// **Property: `scan_iter` and the eager scan agree.**
    ///
    /// `RecordLog::scan` collects every framed record; `scan_iter`
    /// streams them lazily. They must yield byte-identical record
    /// sequences in segment order.
    #[test]
    fn scan_iter_equals_scan(seq in seq_strategy()) {
        let tmp = tempdir().unwrap();
        // Use `RecordLog` directly so we can call both `scan` and
        // `scan_iter`. Translate ops one-to-one.
        let mut log = RecordLog::open(tmp.path()).unwrap();
        for op in &seq {
            match op {
                Op::Put { key, val } => {
                    log.append_record(RecordType::Put, &[*key], val).unwrap();
                }
                Op::Delete { key } => {
                    log.append_record(RecordType::Delete, &[*key], b"").unwrap();
                }
                Op::Fsync => log.fsync().unwrap(),
                Op::Reopen => {
                    drop(log);
                    log = RecordLog::open(tmp.path()).unwrap();
                }
                Op::Rotate => log.rotate().unwrap(),
            }
        }

        let eager = log.scan().expect("scan");
        let lazy: Vec<_> = log
            .scan_iter()
            .expect("scan_iter")
            .map(|r| r.expect("decode"))
            .collect();
        prop_assert_eq!(eager.len(), lazy.len());
        for (a, b) in eager.iter().zip(lazy.iter()) {
            prop_assert_eq!(&a.key, &b.key);
            prop_assert_eq!(&a.payload, &b.payload);
            prop_assert_eq!(a.txid, b.txid);
            prop_assert_eq!(a.segment, b.segment);
            prop_assert_eq!(a.offset, b.offset);
        }
    }

    /// **Property: rotate does not change observable KV state.**
    ///
    /// Inserting a `Rotate` between operations is a no-op at the KV
    /// projection level: the resulting live state, after a final
    /// reopen, equals the oracle.
    #[test]
    fn rotate_is_kv_transparent(
        before in seq_strategy(),
        after in seq_strategy(),
    ) {
        let tmp = tempdir().unwrap();
        {
            let mut wal = apply_seq_to_datawal(tmp.path(), &before);
            // Cross down into the underlying log for the rotate by
            // dropping and reopening as a RecordLog. This costs a
            // reopen but exercises the same recovery path the rest
            // of the property relies on.
            drop(wal);
            {
                let mut log = RecordLog::open(tmp.path()).unwrap();
                log.rotate().unwrap();
                log.fsync().unwrap();
            }
            wal = DataWal::open(tmp.path()).unwrap();
            for op in &after {
                match op {
                    Op::Put { key, val } => { wal.put(&[*key], val).unwrap(); }
                    Op::Delete { key } => { wal.delete(&[*key]).unwrap(); }
                    Op::Fsync | Op::Reopen | Op::Rotate => {}
                }
            }
        }
        let wal = DataWal::open(tmp.path()).unwrap();
        let mut got: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
        for k in wal.keys() {
            got.insert(k.clone(), wal.get(&k).unwrap().unwrap());
        }
        let mut combined = before.clone();
        combined.extend(after);
        let expected = replay_expected(&combined);
        prop_assert_eq!(got, expected);
    }
}
