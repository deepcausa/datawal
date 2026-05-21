# Benchmarks

This document describes **what** the benchmarks measure, **how** to read
their output, and **what to watch out for**. It does not commit numbers.
Numbers belong to the machine they ran on; they are reproduced locally
via `cargo bench` or `just bench`.

For a single example reference run (numbers from one machine, one date,
generic stack description), see
[`benchmarks/v0.1.4-reference.md`](benchmarks/v0.1.4-reference.md).

## What is measured

All benches live under `crates/datawal-core/benches/` and use
[Criterion](https://github.com/bheisler/criterion.rs) with statistical
sampling. They are excluded from the default test job; CI only compiles
them via `cargo bench --workspace --no-run`.

### `record_log.rs`

| Group | Inputs | Throughput axis | What it tells you |
| ----- | ------ | --------------- | ----------------- |
| `record_log_append_no_fsync` | payload 64 B / 1 KiB / 64 KiB | bytes/sec | append throughput excluding durability cost |
| `record_log_append_fsync_each` | payload 64 B / 1 KiB / 64 KiB | bytes/sec | append + per-record `RecordLog::fsync` cost |
| `record_log_scan` | 1 000 / 10 000 records (256 B each) | records/sec | full-log scan rate over a single segment |

The append benches reuse one open `RecordLog` across iterations; only
the per-record cost is measured. Small payloads expose framing/syscall
overhead; large payloads approach the memory/page-cache bandwidth
ceiling.

The fsync delta between the two append groups is the **per-fsync
latency**. Treat it with care -- see *fsync gotchas* below.

### `datawal_kv.rs`

| Group | Inputs | What it tells you |
| ----- | ------ | ----------------- |
| `datawal_put` | keydir of 1k / 10k / 100k existing keys | cost of an `append + HashMap::insert` of a new key |
| `datawal_get` | keydir of 1k / 10k / 100k existing keys | cost of an `HashMap::get` round-trip |
| `datawal_delete` | keydir of 1k / 10k / 100k existing keys | cost of a tombstone append + `HashMap::remove` (with subsequent re-`put` to keep the keyspace stable across iterations) |
| `datawal_open_rebuild` | log of 1k / 10k / 100k records | cold-open cost: parsing the whole log and rebuilding the keydir |

`get` should be nanoseconds; `put`/`delete` should be in the
sub-microsecond to microsecond range; `open_rebuild` should scale
roughly linearly with record count.

If `get` grows substantially with keydir size, you are seeing HashMap
cache pressure -- expected. If `put` grows substantially with keydir
size, something is wrong: an append should not depend on existing
keydir contents.

### `compaction.rs`

| Group | Inputs | What it tells you |
| ----- | ------ | ----------------- |
| `datawal_compact_to_delete_heavy` | live ratio 100 % / 50 % / 10 % via tombstones | scaling of `compact_to` with **live key count** |
| `datawal_compact_to_overwrite_heavy` | live ratio 100 % / 50 % / 10 % via overwrites (keydir stays full) | confirms `compact_to` iterates the **keydir**, not the source log |
| `datawal_export_jsonl` | live ratio 100 % / 50 % / 10 % via tombstones | `export_jsonl` cost vs `compact_to` for the same live set |

The two `compact_to_*` groups are the most revealing benches in the
suite. `delete_heavy` should scale linearly with live keys.
`overwrite_heavy` should be approximately constant across live ratios
because the keydir size is constant -- only the source log grows. If
that constant-time property breaks, compaction has started reading the
log instead of the keydir.

### `recovery.rs`

| Group | Inputs | What it tells you |
| ----- | ------ | ----------------- |
| `recovery_open_clean` | log of 1k / 10k records, one segment | open + `recovery_report` over a clean log |
| `recovery_open_multi_segment` | 1 / 4 / 16 segments, 500 records each | overhead per additional sealed segment |
| `recovery_open_with_tail_truncation` | last segment truncated by 1 / 64 / 1024 bytes | cost of valid-prefix recovery on torn tail |

`open_clean` should be roughly linear in record count.
`open_multi_segment` should be sub-linear in segment count (fixed
per-segment overhead amortises). `open_with_tail_truncation` should
be **independent of truncation size**: the cost is the scan up to the
last valid record, not the size of the trailing garbage.

## How to run

```sh
# whole suite
cargo bench --workspace
just bench

# one bench file
cargo bench -p datawal --bench record_log
just bench-record-log

# fast smoke (sample-size 10, ~30 s total)
just bench-smoke

# named baseline + later compare
just bench-baseline before
# ... change something ...
just bench-compare before
```

Outputs land in `target/criterion/`, which is gitignored. The HTML
report opens at `target/criterion/report/index.html`.

## Reading Criterion output

Each measurement prints three numbers: lower / point / upper estimates
of the bootstrap mean. With 100 default samples, the confidence
interval is the 95th percentile of the bootstrap distribution. The
**point estimate** is what you read; the spread tells you how noisy
the measurement was.

`Throughput::Bytes` adds a `thrpt:` line in MiB/s or GiB/s.
`Throughput::Elements` adds it in elem/s. Both are derived from the
time estimate; they are not independent measurements.

The `change: [...]` line and "Performance has improved/regressed"
messages compare the current run against a Criterion **baseline**
stored under `target/criterion/<bench>/base/`. By default, that
baseline is whatever was there from the previous run -- including
warm-up smoke runs. **Do not trust these comparisons unless you
explicitly saved a baseline first** with `just bench-baseline NAME`.

Common warning: `Unable to complete 100 samples in 5.0s`. Means a
single iteration exceeded ~50 ms and Criterion ran out of measurement
budget. Resolution: increase `--measurement-time`, reduce
`--sample-size`, or enable `--flat-sampling`. Not a correctness
problem; the measurement is still valid, just with fewer samples.

Outlier counts ("Found N outliers among 100 measurements") at 5-10 %
are normal on a busy desktop. 0 % outliers usually means the bench is
running on a too-short timescale and the noise is being hidden. Treat
0 % as suspicious, not as a clean signal.

## What to watch out for

### fsync gotchas

`fsync` in a benchmark only tells the truth if every layer between
your code and the storage media honours it. Possible lies:

| Layer | Lies about fsync when... |
| ----- | ------------------------ |
| tmpfs / overlayfs | always -- they have no underlying device |
| ext4 | `nobarrier` mount option set |
| ZFS | `sync=disabled` set on the dataset, or `sync=standard` without SLOG on a slow pool |
| Hypervisor virtual disk | guest fsync not passed through to host fsync (`cache=writeback` etc.) |
| Consumer SSD without PLP | controller acks before NAND program |
| Network filesystems (NFS, SMB) | client/server fsync semantics vary |

For meaningful fsync benches, point `DATAWAL_BENCH_DIR` at a directory
on a **real local block device** that you trust. For laptops without
power-loss-protected storage, expect fsync latencies in the hundreds
of microseconds to single-digit milliseconds. Latencies under 10 µs
on consumer hardware almost certainly indicate a lying layer.

```sh
DATAWAL_BENCH_DIR=/mnt/realdisk/bench cargo bench
```

### Baselines drift silently

Criterion stores the most recent run as the implicit baseline. If you
run the smoke variant once, then the full bench, the full bench will
report enormous "improvements" against the smoke baseline. They are
meaningless. Always:

1. Snapshot a sober baseline: `just bench-baseline before`
2. Make your change.
3. Compare: `just bench-compare before`

### Don't measure tempdir creation

Every bench is structured to construct the `TempDir` (or
`DATAWAL_BENCH_DIR` subdirectory) **outside** `b.iter()` or via
`iter_with_setup`. If you add a bench, do the same. Tempdir creation
and `fsync_dir` on first use are expensive and would dominate small
benches if measured inside the closure.

### What is *not* measured

By design, the benches do not cover:

- Concurrent readers (reader API does not exist yet -- see issue #5)
- Concurrent writers (single-writer is a hard contract -- see
  `RecordLog` invariant in `AGENTS.md`)
- Cross-crate comparisons (out of scope per issue #1)
- Microbenches of CRC32C or serde_json (out of scope per issue #1)
- Failure-mode timings (covered conceptually by crash injection --
  see issue #8)
- Memory footprint (covered structurally by keydir-by-offset work --
  see issue #4)

## Policy

- Benches are permanent in the repository.
- Numbers from any single run are not committed as truth.
- A reference run with generic stack description is checked in at
  [`benchmarks/v0.1.4-reference.md`](benchmarks/v0.1.4-reference.md)
  for *order-of-magnitude orientation only*.
- CI verifies that benches compile (`cargo bench --workspace --no-run`).
  CI does not run real benches; the numbers would be nonsense on
  shared runners.
