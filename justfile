# datawal — task runner
#
# Requires `just` (https://github.com/casey/just).
# Run `just` (no args) for the default validation cycle, or `just --list`
# to see every available recipe.

# Default recipe: list every available recipe.
default:
    @just --list

# ---------------------------------------------------------------------------
# Validation cycle
# ---------------------------------------------------------------------------

# Format check (no writes). Fails if any file would change.
fmt-check:
    cargo fmt --all -- --check

# Apply formatting in place.
fmt:
    cargo fmt --all

# `cargo check` across the workspace, including tests/examples/benches.
check-rust:
    cargo check --workspace --all-targets

# Clippy with warnings denied (matches CI).
clippy:
    cargo clippy --workspace --all-targets -- -D warnings

# Run the full test suite, including examples and integration tests.
test:
    cargo test --workspace --all-targets

# Compile the benches without running them (matches CI's bench step).
bench-check:
    cargo bench --workspace --no-run

# Build rustdoc with warnings denied (matches CI).
doc:
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps

# Full local CI-equivalent cycle. Run before pushing.
check: fmt-check check-rust clippy test bench-check doc

# ---------------------------------------------------------------------------
# Examples
# ---------------------------------------------------------------------------

# Run every example end-to-end.
examples: example-record-log example-datawal-kv example-tail-recovery

example-record-log:
    cargo run -p datawal --example record_log_demo

example-datawal-kv:
    cargo run -p datawal --example datawal_kv_demo

example-tail-recovery:
    cargo run -p datawal --example tail_recovery_demo

# ---------------------------------------------------------------------------
# CLI (datawal-cli)
# ---------------------------------------------------------------------------
#
# `datawal-cli` ships the read-only `datawal` binary (scan/get/report/
# verify/dump). It is not yet on crates.io; build from the workspace.
#
# Quick usage:
#
#     just cli-build                     # produces target/release/datawal
#     just cli-run -- scan /tmp/store    # forwards args after `--`
#     just cli-install-local             # installs to ~/.cargo/bin/datawal
#     just cli-smoke                     # end-to-end smoke (needs jq)

# Release build of the CLI binary. Binary path: target/release/datawal
cli-build:
    cargo build -p datawal-cli --release

# Debug build (faster, larger). Binary path: target/debug/datawal
cli-build-debug:
    cargo build -p datawal-cli

# Run the CLI via cargo. Pass args after `--`.
# Usage: just cli-run -- scan /tmp/store --json --limit 5
cli-run *ARGS:
    cargo run -p datawal-cli --release -- {{ARGS}}

# Print the path of the release binary (after `just cli-build`).
cli-path:
    @echo "$(pwd)/target/release/datawal"

# Install the CLI into ~/.cargo/bin (or $CARGO_HOME/bin). Uses --path so
# crates.io is not needed.
cli-install-local:
    cargo install --path crates/datawal-cli --force

# Show CLI help (release build).
cli-help: cli-build
    ./target/release/datawal --help

# End-to-end smoke: builds, seeds a temp store, exercises every subcommand
# with jq assertions. Requires `jq` on PATH.
cli-smoke:
    crates/datawal-cli/examples/cli_read_smoke.sh

# ---------------------------------------------------------------------------
# Benchmarks
# ---------------------------------------------------------------------------
#
# Numbers are NOT committed; benches are diagnostic, machine-dependent.
# For meaningful fsync numbers, point DATAWAL_BENCH_DIR at a real local
# disk (not tmpfs/overlayfs/NFS):
#
#     DATAWAL_BENCH_DIR=/mnt/nvme/datawal-bench just bench
#
# Or pass it explicitly to a single bench:
#
#     DATAWAL_BENCH_DIR=/mnt/nvme/datawal-bench just bench-record-log

# Run every bench in the workspace (slow: ~minutes).
bench:
    cargo bench --workspace

bench-record-log:
    cargo bench -p datawal --bench record_log

bench-datawal-kv:
    cargo bench -p datawal --bench datawal_kv

bench-compaction:
    cargo bench -p datawal --bench compaction

bench-recovery:
    cargo bench -p datawal --bench recovery

# Fast smoke run of every bench (lousy numbers, just verifies they run).
bench-smoke:
    cargo bench --workspace -- --sample-size 10 --warm-up-time 1 --measurement-time 2

# Save the current run as a named baseline for later --baseline comparison.
# Usage: just bench-baseline before
bench-baseline name:
    cargo bench --workspace -- --save-baseline {{name}}

# Compare the current bench run against a previously saved baseline.
# Usage: just bench-compare before
bench-compare name:
    cargo bench --workspace -- --baseline {{name}}

# Open Criterion's HTML report (after running benches at least once).
bench-report:
    @if [ -f target/criterion/report/index.html ]; then \
        echo "Report: file://$(pwd)/target/criterion/report/index.html"; \
    else \
        echo "No report yet. Run \`just bench\` first."; \
        exit 1; \
    fi

# ---------------------------------------------------------------------------
# Fuzzing
# ---------------------------------------------------------------------------
#
# Requires `cargo install cargo-fuzz` and a nightly toolchain
# (`rustup toolchain install nightly`). The fuzz crate at `fuzz/` is
# intentionally outside the workspace; see `fuzz/README.md` for the
# target catalogue and triage notes.

# Build every fuzz target (compile only, no fuzzing).
fuzz-build:
    cd fuzz && cargo +nightly fuzz build

# Run the primary decoder fuzz target. Override TIME for longer runs.
# Usage: just fuzz-run-decode TIME=300
fuzz-run-decode TIME="30":
    cd fuzz && cargo +nightly fuzz run decode_frame -- -max_total_time={{TIME}}

# Run the scan_log integration smoke target.
fuzz-run-scan TIME="30":
    cd fuzz && cargo +nightly fuzz run scan_log -- -max_total_time={{TIME}}

# Run the DataWal put/get roundtrip target.
fuzz-run-roundtrip TIME="30":
    cd fuzz && cargo +nightly fuzz run roundtrip -- -max_total_time={{TIME}}

# ---------------------------------------------------------------------------
# Wire-format corpus
# ---------------------------------------------------------------------------

# Regenerate the in-tree corpus fixtures. Only intentionally — committing
# the result is a wire-format change.
corpus-regen:
    cargo run -p datawal --example gen_corpus

# Compare a freshly generated corpus against the in-tree fixtures (matches
# the CI corpus job). Fails on drift.
corpus-check:
    @set -eu; \
    tmp="$(mktemp -d)"; \
    trap 'rm -rf "$tmp"' EXIT; \
    cargo run -p datawal --example gen_corpus -- "$tmp"; \
    ref=crates/datawal-core/tests/corpus; \
    ( cd "$ref" && find . -type f -name '*.dwal' -print0 | sort -z | xargs -0 sha256sum ) > "$tmp/ref.sha256"; \
    ( cd "$tmp"  && find . -type f -name '*.dwal' -print0 | sort -z | xargs -0 sha256sum ) > "$tmp/gen.sha256"; \
    if diff -u "$tmp/ref.sha256" "$tmp/gen.sha256"; then \
        echo "Corpus matches in-tree fixtures."; \
    else \
        echo "ERROR: wire-format corpus drifted."; \
        exit 1; \
    fi

# ---------------------------------------------------------------------------
# Formal (TLA+) models
# ---------------------------------------------------------------------------
#
# Requires tla2tools.jar on TLA_TOOLS or in the repo root, and a JDK.
# CI uses v1.8.0; bump TLA_TOOLS_URL in `.github/workflows/ci.yml` to match.

tla_tools := env_var_or_default('TLA_TOOLS', 'tla2tools.jar')

# Model-check every TLA+ spec.
formal: formal-record-log formal-keydir formal-compaction

formal-record-log:
    cd formal && java -XX:+UseParallelGC -cp ../{{tla_tools}} tlc2.TLC -workers 2 -config RecordLog.cfg RecordLog.tla

formal-keydir:
    cd formal && java -XX:+UseParallelGC -cp ../{{tla_tools}} tlc2.TLC -workers 2 -config KeydirProjection.cfg KeydirProjection.tla

formal-compaction:
    cd formal && java -XX:+UseParallelGC -cp ../{{tla_tools}} tlc2.TLC -workers 2 -config Compaction.cfg Compaction.tla

# ---------------------------------------------------------------------------
# Cleanup
# ---------------------------------------------------------------------------

# Wipe build outputs.
clean:
    cargo clean

# Wipe build outputs and Criterion bench data.
clean-all: clean
    rm -rf target/criterion
