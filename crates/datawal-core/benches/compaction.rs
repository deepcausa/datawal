//! Benchmarks for `DataWal::compact_to` and `DataWal::export_jsonl`.
//!
//! Compaction cost is measured as a function of live-key ratio: how
//! many keys are still alive vs how many tombstones / overwritten
//! versions the log carries.
//!
//! Run with:
//!
//! ```text
//! cargo bench -p datawal --bench compaction
//! ```

mod common;

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use datawal::DataWal;
use tempfile::TempDir;

use crate::common::bench_tempdir;

/// Total number of distinct keys ever written. Kept modest to keep
/// the bench wallclock reasonable; the *ratio* of live-to-dead is the
/// interesting axis.
const TOTAL_KEYS: usize = 10_000;

/// Live-key ratios: fraction of `TOTAL_KEYS` that survive into the
/// compacted output. The complement is killed via either overwrite
/// (still 1 record per key) or delete (tombstone added).
const LIVE_RATIOS: &[(&str, f64)] = &[("100", 1.0), ("50", 0.5), ("10", 0.1)];

const VALUE_SIZE: usize = 64;

fn key_for(i: usize) -> [u8; 16] {
    let mut k = [0u8; 16];
    k[..8].copy_from_slice(&(i as u64).to_le_bytes());
    k
}

/// Populate a fresh `DataWal` with `TOTAL_KEYS` distinct keys, then
/// delete a `(1.0 - live_ratio)` fraction of them. The remaining log
/// thus contains a mix of live entries and tombstones.
fn populated_with_deletes(live_ratio: f64) -> (TempDir, DataWal) {
    let dir = bench_tempdir();
    let mut kv = DataWal::open(dir.path()).expect("open");
    let value = vec![0xCDu8; VALUE_SIZE];

    for i in 0..TOTAL_KEYS {
        kv.put(&key_for(i), &value).expect("put");
    }

    let kill_from = (TOTAL_KEYS as f64 * live_ratio) as usize;
    for i in kill_from..TOTAL_KEYS {
        kv.delete(&key_for(i)).expect("delete");
    }

    (dir, kv)
}

/// Like `populated_with_deletes`, but generates dead versions via
/// repeated *overwrite* instead of delete. Keydir size stays constant
/// at `TOTAL_KEYS`; the log accumulates obsolete versions.
fn populated_with_overwrites(live_ratio: f64) -> (TempDir, DataWal) {
    let dir = bench_tempdir();
    let mut kv = DataWal::open(dir.path()).expect("open");
    let value = vec![0xCDu8; VALUE_SIZE];

    for i in 0..TOTAL_KEYS {
        kv.put(&key_for(i), &value).expect("put");
    }

    // Number of *additional* overwrites we want, on top of the
    // initial puts, so that live/total = live_ratio.
    let overwrites = ((TOTAL_KEYS as f64 / live_ratio) as usize).saturating_sub(TOTAL_KEYS);
    for j in 0..overwrites {
        let i = j % TOTAL_KEYS;
        kv.put(&key_for(i), &value).expect("overwrite");
    }

    (dir, kv)
}

fn bench_compact_to_delete_heavy(c: &mut Criterion) {
    let mut group = c.benchmark_group("datawal_compact_to_delete_heavy");

    for &(label, ratio) in LIVE_RATIOS {
        let (_src, mut kv) = populated_with_deletes(ratio);

        group.bench_function(BenchmarkId::from_parameter(label), |b| {
            b.iter_with_setup(bench_tempdir, |out_dir| {
                let stats = kv.compact_to(out_dir.path()).expect("compact_to");
                black_box(stats);
                // out_dir drops here (post-measurement teardown).
            });
        });
    }

    group.finish();
}

fn bench_compact_to_overwrite_heavy(c: &mut Criterion) {
    let mut group = c.benchmark_group("datawal_compact_to_overwrite_heavy");

    for &(label, ratio) in LIVE_RATIOS {
        let (_src, mut kv) = populated_with_overwrites(ratio);

        group.bench_function(BenchmarkId::from_parameter(label), |b| {
            b.iter_with_setup(bench_tempdir, |out_dir| {
                let stats = kv.compact_to(out_dir.path()).expect("compact_to");
                black_box(stats);
            });
        });
    }

    group.finish();
}

fn bench_export_jsonl(c: &mut Criterion) {
    let mut group = c.benchmark_group("datawal_export_jsonl");

    for &(label, ratio) in LIVE_RATIOS {
        let (_src, mut kv) = populated_with_deletes(ratio);

        group.bench_function(BenchmarkId::from_parameter(label), |b| {
            b.iter_with_setup(
                || {
                    let dir = bench_tempdir();
                    let path = dir.path().join("export.jsonl");
                    (dir, path)
                },
                |(dir, path)| {
                    kv.export_jsonl(&path).expect("export_jsonl");
                    black_box(&path);
                    drop(dir);
                },
            );
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_compact_to_delete_heavy,
    bench_compact_to_overwrite_heavy,
    bench_export_jsonl
);
criterion_main!(benches);
