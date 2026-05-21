//! Benchmarks for `DataWal`: put / get / delete and reopen (keydir rebuild).
//!
//! Each operation is measured as a function of pre-existing keydir size
//! (`1k`, `10k`, `100k` keys).
//!
//! Run with:
//!
//! ```text
//! cargo bench -p datawal --bench datawal_kv
//! ```

mod common;

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use datawal::DataWal;
use tempfile::TempDir;

use crate::common::bench_tempdir;

/// Pre-populated keydir sizes used across the per-op benches.
const KEYDIR_SIZES: &[usize] = &[1_000, 10_000, 100_000];

/// Fixed value size used across KV benches. Keeps the comparison on
/// keydir-size and operation cost, not on payload bandwidth.
const VALUE_SIZE: usize = 64;

/// Encode an index as a fixed-width 16-byte key. Stable bytes across
/// iterations; the put/get/delete benches reuse the same key set.
fn key_for(i: usize) -> [u8; 16] {
    let mut k = [0u8; 16];
    k[..8].copy_from_slice(&(i as u64).to_le_bytes());
    k
}

/// Build a fresh `DataWal` populated with `n` keys `0..n`.
///
/// Returns the `TempDir` alongside the `DataWal` so the directory
/// outlives the handle.
fn populated_kv(n: usize) -> (TempDir, DataWal) {
    let dir = bench_tempdir();
    let mut kv = DataWal::open(dir.path()).expect("open");
    let value = vec![0xCDu8; VALUE_SIZE];
    for i in 0..n {
        kv.put(&key_for(i), &value).expect("put");
    }
    (dir, kv)
}

fn bench_put(c: &mut Criterion) {
    let mut group = c.benchmark_group("datawal_put");

    for &n in KEYDIR_SIZES {
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            // Pre-populate to `n`, then measure inserts of *new* keys
            // (i.e. keydir keeps growing).
            let (_dir, mut kv) = populated_kv(n);
            let value = vec![0xCDu8; VALUE_SIZE];
            let mut next = n;
            b.iter(|| {
                let k = key_for(next);
                kv.put(black_box(&k), black_box(&value)).expect("put");
                next += 1;
            });
        });
    }

    group.finish();
}

fn bench_get(c: &mut Criterion) {
    let mut group = c.benchmark_group("datawal_get");

    for &n in KEYDIR_SIZES {
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            let (_dir, mut kv) = populated_kv(n);
            // Round-robin over the populated key range.
            let mut i = 0usize;
            b.iter(|| {
                let k = key_for(i % n);
                let v = kv.get(black_box(&k)).expect("get");
                black_box(v);
                i = i.wrapping_add(1);
            });
        });
    }

    group.finish();
}

fn bench_delete(c: &mut Criterion) {
    let mut group = c.benchmark_group("datawal_delete");

    for &n in KEYDIR_SIZES {
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            // Each iteration deletes one key, then re-inserts it so
            // the keydir size and the keyspace stay constant across
            // iterations.
            let (_dir, mut kv) = populated_kv(n);
            let value = vec![0xCDu8; VALUE_SIZE];
            let mut i = 0usize;
            b.iter(|| {
                let k = key_for(i % n);
                kv.delete(black_box(&k)).expect("delete");
                kv.put(&k, &value).expect("re-put");
                i = i.wrapping_add(1);
            });
        });
    }

    group.finish();
}

fn bench_open_rebuild(c: &mut Criterion) {
    let mut group = c.benchmark_group("datawal_open_rebuild");

    for &n in KEYDIR_SIZES {
        // Build the on-disk log once, then re-open it on every iteration.
        // We measure the cost of replaying the log to rebuild the keydir.
        let (dir, kv) = populated_kv(n);
        drop(kv); // release the single-writer lock

        group.bench_function(BenchmarkId::from_parameter(n), |b| {
            b.iter(|| {
                let kv = DataWal::open(dir.path()).expect("open");
                black_box(kv.len());
                // kv drops here, releasing the lock for the next iter.
            });
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_put,
    bench_get,
    bench_delete,
    bench_open_rebuild
);
criterion_main!(benches);
