# datawal-fuzz

Coverage-guided fuzz targets for the wire-format decoder and a few
adjacent surfaces. Built on
[`cargo-fuzz`](https://github.com/rust-fuzz/cargo-fuzz)
(libFuzzer, nightly Rust only).

`fuzz/` is intentionally **outside the Cargo workspace**
(`exclude = ["fuzz"]` in the root `Cargo.toml`). `cargo-fuzz` and
`libfuzzer-sys` require nightly; keeping this crate out of the
workspace lets `cargo check --workspace`, the MSRV 1.75 CI job, and
`cargo bench --no-run` ignore it entirely.

## Targets

| Target          | Tier        | Exercises                                                |
| --------------- | ----------- | -------------------------------------------------------- |
| `decode_frame`  | primary     | `format::decode_next` on a single buffer of fuzz bytes   |
| `scan_log`      | integration | `RecordLog::open` + `recovery_report` over fuzz bytes    |
| `roundtrip`     | integration | `DataWal::put` then `get`, asserts bytes are preserved   |

`decode_frame` is the fast, no-I/O target meant to run many
millions of executions. `scan_log` and `roundtrip` are slower
because they touch the filesystem (each iteration creates a
tempdir); they are smoke targets, not stress targets. Plan for
~30s to a few minutes per local run, not hours-long campaigns.

## Setup

```sh
# Pinned to match CI. cargo-fuzz 0.13.1's published Cargo.lock fixes
# `rustix 0.36.5`, which only compiles on nightlies up to mid-2024.
cargo install cargo-fuzz --version 0.13.1 --locked
rustup toolchain install nightly-2024-08-01
```

Any compatible (cargo-fuzz, nightly) pair works locally; the pin above
is the one CI uses.

## Build

```sh
cd fuzz
cargo +nightly fuzz build
```

This compiles all three targets. CI runs this on a nightly job; it
does not run the fuzzers.

## Run

```sh
# Primary target, 30 seconds.
cargo +nightly fuzz run decode_frame -- -max_total_time=30

# Integration smoke, 30 seconds each.
cargo +nightly fuzz run scan_log    -- -max_total_time=30
cargo +nightly fuzz run roundtrip   -- -max_total_time=30
```

The `justfile` at the repo root has matching shortcuts:

```sh
just fuzz-build
just fuzz-run-decode    # decode_frame, 30s
just fuzz-run-scan      # scan_log,    30s
just fuzz-run-roundtrip # roundtrip,   30s

# Override duration:
just fuzz-run-decode TIME=300
```

## Seeds

Hand-picked small seeds live under `corpus_seeds/<target>/`. They
are kept under ~150 bytes each:

- A few wire-format corpus fixtures copied from
  `crates/datawal-core/tests/corpus/` (valid log, bad CRC,
  truncated tail, unknown version, compact output).
- Synthesised pathological frames: empty input, partial header,
  wrong magic, reserved flags set, declared oversize key/payload,
  unknown record type.

When you run `cargo fuzz`, libFuzzer keeps its own working corpus
under `fuzz/corpus/<target>/`. Treat `corpus_seeds/` as the
versioned starting point and `fuzz/corpus/` as scratch.

## Triaging a crash

If libFuzzer finds something, it writes the crashing input under
`fuzz/artifacts/<target>/`. Reproduce with:

```sh
cargo +nightly fuzz run decode_frame fuzz/artifacts/decode_frame/crash-...
```

Open an issue with the artifact attached (hex dump if small) and
the panic backtrace.

## Out of scope

- Differential fuzzing against another decoder.
- Long-running fuzz jobs in CI. CI only verifies the targets
  *compile* on nightly. Running them is a developer / maintainer
  task.
- Stress-testing the static limits (64 KiB keys, 64 MiB payloads).
  Those have unit tests; the fuzzers focus on input *shapes*, not
  on maximum sizes.

See [issue #2](https://github.com/deepcausa/datawal/issues/2) for
the original scope.
