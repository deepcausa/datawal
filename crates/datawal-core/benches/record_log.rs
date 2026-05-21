//! Benchmarks for `RecordLog`: append (with and without per-append fsync)
//! and full-log scan.
//!
//! Run with:
//!
//! ```text
//! cargo bench -p datawal --bench record_log
//! ```
//!
//! For fsync benchmarks on a real local disk, set:
//!
//! ```text
//! DATAWAL_BENCH_DIR=/mnt/nvme/datawal-bench cargo bench -p datawal --bench record_log
//! ```
//!
//! Without `DATAWAL_BENCH_DIR`, fsync numbers reflect whatever the system
//! tempdir happens to sit on (often tmpfs on Linux). See `benches/README.md`.

mod common;

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use datawal::RecordLog;

use crate::common::{bench_tempdir, payload};

/// Payload sizes exercised by the append bench. Span "header-dominated"
/// (64 B, where the 24-byte header + CRC dominate) up to
/// "payload-dominated" (64 KiB).
const APPEND_SIZES: &[usize] = &[64, 1024, 64 * 1024];

/// Record counts exercised by the scan bench.
const SCAN_COUNTS: &[usize] = &[1_000, 10_000];

/// Payload size used while populating logs for scan benches.
const SCAN_PAYLOAD_SIZE: usize = 256;

fn bench_append_no_fsync(c: &mut Criterion) {
    let mut group = c.benchmark_group("record_log_append_no_fsync");

    for &size in APPEND_SIZES {
        let buf = payload(size);
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &buf, |b, buf| {
            let dir = bench_tempdir();
            let mut log = RecordLog::open(dir.path()).expect("open");
            b.iter(|| {
                log.append(black_box(buf)).expect("append");
            });
        });
    }

    group.finish();
}

fn bench_append_fsync_each(c: &mut Criterion) {
    let mut group = c.benchmark_group("record_log_append_fsync_each");

    for &size in APPEND_SIZES {
        let buf = payload(size);
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &buf, |b, buf| {
            let dir = bench_tempdir();
            let mut log = RecordLog::open(dir.path()).expect("open");
            b.iter(|| {
                log.append(black_box(buf)).expect("append");
                log.fsync().expect("fsync");
            });
        });
    }

    group.finish();
}

fn bench_scan(c: &mut Criterion) {
    let mut group = c.benchmark_group("record_log_scan");

    for &n in SCAN_COUNTS {
        // Populate the log once per scenario, outside the measured loop.
        let dir = bench_tempdir();
        let mut log = RecordLog::open(dir.path()).expect("open");
        let buf = payload(SCAN_PAYLOAD_SIZE);
        for _ in 0..n {
            log.append(&buf).expect("append");
        }
        log.fsync().expect("fsync");

        group.throughput(Throughput::Elements(n as u64));
        group.bench_function(BenchmarkId::from_parameter(n), |b| {
            // `RecordLog::scan` takes `&mut self`; re-scanning the same
            // populated log is the unit of work.
            b.iter(|| {
                let records = log.scan().expect("scan");
                black_box(records);
            });
        });
    }

    group.finish();
}

/// Parity bench: collect every record from `scan_iter` into a `Vec`.
///
/// Expected to be in the same order of magnitude as `bench_scan`, with
/// some `Result`-per-item overhead from the iterator contract.
fn bench_scan_iter_collect(c: &mut Criterion) {
    let mut group = c.benchmark_group("record_log_scan_iter_collect");

    for &n in SCAN_COUNTS {
        // Populate the log once per scenario, outside the measured loop.
        let dir = bench_tempdir();
        let mut log = RecordLog::open(dir.path()).expect("open");
        let buf = payload(SCAN_PAYLOAD_SIZE);
        for _ in 0..n {
            log.append(&buf).expect("append");
        }
        log.fsync().expect("fsync");

        group.throughput(Throughput::Elements(n as u64));
        group.bench_function(BenchmarkId::from_parameter(n), |b| {
            // `scan_iter` takes `&self`; we can call it repeatedly on the
            // same handle without re-opening.
            b.iter(|| {
                let records: Vec<_> = log
                    .scan_iter()
                    .expect("scan_iter")
                    .collect::<anyhow::Result<Vec<_>>>()
                    .expect("collect");
                black_box(records);
            });
        });
    }

    group.finish();
}

/// Lazy-stop bench: open the iterator, decode one record, drop.
///
/// This is the value proposition of `scan_iter`: latency should not
/// scale with log size when the consumer only needs the head.
fn bench_scan_iter_early_stop(c: &mut Criterion) {
    let mut group = c.benchmark_group("record_log_scan_iter_early_stop");

    for &n in SCAN_COUNTS {
        // Populate the log once per scenario, outside the measured loop.
        let dir = bench_tempdir();
        let mut log = RecordLog::open(dir.path()).expect("open");
        let buf = payload(SCAN_PAYLOAD_SIZE);
        for _ in 0..n {
            log.append(&buf).expect("append");
        }
        log.fsync().expect("fsync");

        // The unit of work is "one record", not "n records": Elements(1).
        group.throughput(Throughput::Elements(1));
        group.bench_function(BenchmarkId::from_parameter(n), |b| {
            b.iter(|| {
                let mut it = log.scan_iter().expect("scan_iter");
                let first = it.next().expect("some").expect("ok");
                black_box(first);
                // `it` dropped here; nothing else decoded.
            });
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_append_no_fsync,
    bench_append_fsync_each,
    bench_scan,
    bench_scan_iter_collect,
    bench_scan_iter_early_stop
);
criterion_main!(benches);
