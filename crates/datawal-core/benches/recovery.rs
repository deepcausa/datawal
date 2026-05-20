//! Benchmarks for `RecordLog` recovery cost.
//!
//! Measures the cost of `RecordLog::open` followed by reading the
//! `recovery_report`, as a function of:
//!
//! - total log size (number of records and segments),
//! - a truncated tail on the active segment (last N bytes dropped).
//!
//! Run with:
//!
//! ```text
//! cargo bench -p datawal --bench recovery
//! ```

mod common;

use std::fs::OpenOptions;
use std::path::Path;

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use datawal::RecordLog;
use tempfile::TempDir;

use crate::common::{bench_tempdir, payload};

/// Total record counts for the recovery-vs-size bench.
const RECORD_COUNTS: &[usize] = &[1_000, 10_000];

/// Number of segments for the recovery-vs-segments bench. Each segment
/// is rotated explicitly after a fixed number of records.
const SEGMENT_COUNTS: &[usize] = &[1, 4, 16];

/// Records per segment in the multi-segment bench.
const RECORDS_PER_SEGMENT: usize = 500;

const RECORD_PAYLOAD_SIZE: usize = 256;

/// Tail truncation lengths exercised in bytes.
const TAIL_TRUNC_BYTES: &[usize] = &[1, 64, 1024];

/// Build a populated log directory with `n` records in a single segment,
/// fsync, then drop the writer so the on-disk state is stable.
fn populate(n: usize) -> TempDir {
    let dir = bench_tempdir();
    let mut log = RecordLog::open(dir.path()).expect("open");
    let buf = payload(RECORD_PAYLOAD_SIZE);
    for _ in 0..n {
        log.append(&buf).expect("append");
    }
    log.fsync().expect("fsync");
    drop(log);
    dir
}

/// Build a log with `segments` segments, each holding `RECORDS_PER_SEGMENT`
/// records, calling `rotate` between segments.
fn populate_segments(segments: usize) -> TempDir {
    let dir = bench_tempdir();
    let mut log = RecordLog::open(dir.path()).expect("open");
    let buf = payload(RECORD_PAYLOAD_SIZE);
    for s in 0..segments {
        for _ in 0..RECORDS_PER_SEGMENT {
            log.append(&buf).expect("append");
        }
        log.fsync().expect("fsync");
        // Rotate after every segment except the last, so the last is
        // the active segment with full records and no tail damage.
        if s + 1 < segments {
            log.rotate().expect("rotate");
        }
    }
    drop(log);
    dir
}

/// Truncate the last `n` bytes off the highest-numbered `.dwal` file
/// in `dir`. Used to simulate a partially-written tail record.
fn truncate_tail(dir: &Path, n: usize) {
    let mut active: Option<std::path::PathBuf> = None;
    for entry in std::fs::read_dir(dir).expect("read_dir").flatten() {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) == Some("dwal") {
            match &active {
                Some(prev) if prev >= &p => {}
                _ => active = Some(p),
            }
        }
    }
    let active = active.expect("at least one .dwal segment");
    let f = OpenOptions::new()
        .write(true)
        .open(&active)
        .expect("open active segment");
    let len = f.metadata().expect("metadata").len();
    let new_len = len.saturating_sub(n as u64);
    f.set_len(new_len).expect("set_len");
}

fn bench_open_clean(c: &mut Criterion) {
    let mut group = c.benchmark_group("recovery_open_clean");

    for &n in RECORD_COUNTS {
        let dir = populate(n);
        group.bench_function(BenchmarkId::from_parameter(n), |b| {
            b.iter(|| {
                let log = RecordLog::open(dir.path()).expect("open");
                let report = log.recovery_report().expect("report");
                black_box(report);
                // log drops here, releasing the writer lock.
            });
        });
    }

    group.finish();
}

fn bench_open_multi_segment(c: &mut Criterion) {
    let mut group = c.benchmark_group("recovery_open_multi_segment");

    for &segments in SEGMENT_COUNTS {
        let dir = populate_segments(segments);
        group.bench_function(BenchmarkId::from_parameter(segments), |b| {
            b.iter(|| {
                let log = RecordLog::open(dir.path()).expect("open");
                let report = log.recovery_report().expect("report");
                black_box(report);
            });
        });
    }

    group.finish();
}

fn bench_open_with_tail_truncation(c: &mut Criterion) {
    let mut group = c.benchmark_group("recovery_open_with_tail_truncation");

    for &trunc in TAIL_TRUNC_BYTES {
        // Fresh tempdir per scenario: tail truncation mutates the
        // on-disk state and must not leak between scenarios.
        let dir = populate(1_000);
        truncate_tail(dir.path(), trunc);

        group.bench_function(BenchmarkId::from_parameter(trunc), |b| {
            b.iter(|| {
                let log = RecordLog::open(dir.path()).expect("open");
                let report = log.recovery_report().expect("report");
                black_box(report);
            });
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_open_clean,
    bench_open_multi_segment,
    bench_open_with_tail_truncation
);
criterion_main!(benches);
