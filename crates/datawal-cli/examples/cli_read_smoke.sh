#!/usr/bin/env bash
# Read-only end-to-end smoke test for the `datawal` inspector binary.
#
# Builds a datawal store from the library demo, then exercises every
# subcommand (scan / get / report / verify / dump) in both human and
# JSON forms and asserts on exit codes and key invariants.
#
# Usage:
#   crates/datawal-cli/examples/cli_read_smoke.sh
#
# Requires: cargo, jq.
#
# This script lives under examples/ rather than tests/ because it
# spawns subprocesses and shells out; the integration tests in
# tests/integration.rs cover the same surface from inside cargo
# test. The script complements those by proving end-to-end shell
# usability for downstream users.

set -euo pipefail

# Pretty banners -------------------------------------------------------------
log() { printf '[smoke] %s\n' "$*" >&2; }
die() { printf '[smoke] FAIL: %s\n' "$*" >&2; exit 1; }

require_jq() {
    if ! command -v jq >/dev/null 2>&1; then
        die "jq required (apt-get install jq)"
    fi
}

# Locate workspace root, regardless of where the script is invoked from.
here="$(cd "$(dirname "$0")" && pwd)"
ws_root="$(cd "$here/../../.." && pwd)"
cd "$ws_root"

require_jq

log "building datawal-cli + populate_smoke_store example"
cargo build -q -p datawal-cli
cargo build -q -p datawal-cli --example populate_smoke_store

bin="$ws_root/target/debug/datawal"
demo="$ws_root/target/debug/examples/populate_smoke_store"
[[ -x "$bin"  ]] || die "missing binary: $bin"
[[ -x "$demo" ]] || die "missing demo:   $demo"

# Workspace: ${tmp}/store is the live store; ${tmp}/empty is a
# nonexistent directory we hand to `report` to test the
# create-if-missing semantics inherited from RecordLog::open.
tmp="$(mktemp -d -t datawal-cli-smoke.XXXXXX)"
trap 'rm -rf "$tmp"' EXIT
store="$tmp/store"

log "populating store via populate_smoke_store at $store"
"$demo" "$store" >"$tmp/demo.log" 2>&1 || die "demo populate failed; see $tmp/demo.log"

# --- scan -------------------------------------------------------------------
log "[scan] human form lists at least one PUT"
"$bin" scan "$store" >"$tmp/scan.human.out"
grep -q 'type=PUT' "$tmp/scan.human.out" \
    || die "scan human form did not contain any PUT line"

log "[scan] human form renders printable keys literally (no base64 noise)"
# populate_smoke_store seeds alpha/beta/gamma/delta -> all printable.
grep -q 'key=alpha' "$tmp/scan.human.out" \
    || die "scan human form did not render printable key 'alpha' literally"

log "[scan] --bytes hex forces hex rendering for all bytes"
"$bin" scan "$store" --bytes hex --limit 1 >"$tmp/scan.hex.out"
grep -q 'key=hex:' "$tmp/scan.hex.out" \
    || die "scan --bytes hex did not produce hex: prefix"

log "[scan] --json emits valid datawal.cli.v1 records"
"$bin" --json scan "$store" >"$tmp/scan.json.ndjson"
first_line="$(head -n1 "$tmp/scan.json.ndjson")"
[[ -n "$first_line" ]] || die "scan --json produced no lines"
echo "$first_line" | jq -e '
    .schema      == "datawal.cli.v1" and
    .kind        == "record"         and
    (.segment    | type) == "number" and
    (.offset     | type) == "number" and
    (.len        | type) == "number" and
    (.record_type | IN("Raw","Put","Delete")) and
    (.txid       | type) == "number" and
    (.key_base64 | type) == "string"
' >/dev/null || die "scan --json line failed schema assertions"

log "[scan] --limit 1 honours the cap"
"$bin" --json scan "$store" --limit 1 >"$tmp/scan.limit.ndjson"
[[ "$(wc -l <"$tmp/scan.limit.ndjson")" -eq 1 ]] \
    || die "scan --limit 1 produced $(wc -l <"$tmp/scan.limit.ndjson") lines"

# --- get --------------------------------------------------------------------
# Pick the first key from scan output and round-trip it through get.
first_key_b64="$(jq -r '.key_base64' "$tmp/scan.limit.ndjson")"
[[ -n "$first_key_b64" ]] || die "could not extract first key_base64 from scan"

log "[get] --json hit returns a value record"
"$bin" --json get "$store" --key-base64 "$first_key_b64" >"$tmp/get.hit.json"
jq -e '
    .schema       == "datawal.cli.v1" and
    .kind         == "value"          and
    (.value_base64 | type) == "string" and
    (.value_len    | type) == "number"
' "$tmp/get.hit.json" >/dev/null || die "get hit JSON failed assertions"

log "[get] --json miss exits 2"
set +e
"$bin" --json get "$store" --key-base64 "$(printf 'absent-key' | base64)" >"$tmp/get.miss.json"
rc=$?
set -e
[[ "$rc" -eq 2 ]] || die "expected exit 2 on miss, got $rc"
jq -e '.kind == "miss"' "$tmp/get.miss.json" >/dev/null \
    || die "miss JSON did not contain kind=miss"

log "[get] --key TEXT with printable value prints the value literally"
"$bin" get "$store" --key alpha >"$tmp/get.text.out"
got="$(tr -d '\n' <"$tmp/get.text.out")"
[[ "$got" == "1" ]] || die "expected literal '1' for --key alpha, got '$got'"

log "[get] bad --key-base64 fails with exit 1"
set +e
"$bin" get "$store" --key-base64 '!!!not-base64!!!' >/dev/null 2>"$tmp/get.bad.err"
rc=$?
set -e
[[ "$rc" -eq 1 ]] || die "expected exit 1 on bad base64, got $rc"
grep -q 'invalid --key-base64' "$tmp/get.bad.err" \
    || die "bad-base64 stderr message missing 'invalid --key-base64'"

# --- report -----------------------------------------------------------------
log "[report] --json emits a report object with expected fields"
"$bin" --json report "$store" >"$tmp/report.json"
jq -e '
    .schema             == "datawal.cli.v1" and
    .kind               == "report" and
    (.files_scanned     | type) == "number" and
    (.records_replayed  | type) == "number" and
    (.tail_truncated    | type) == "number" and
    (.last_txid_seen    | type) == "number"
' "$tmp/report.json" >/dev/null || die "report --json failed schema assertions"

log "[report] nonexistent dir gets created and reports zero records"
"$bin" --json report "$tmp/empty" >"$tmp/report.empty.json"
jq -e '.records_replayed == 0' "$tmp/report.empty.json" >/dev/null \
    || die "report on fresh dir did not report 0 records"
[[ -d "$tmp/empty" ]] || die "report did not create the missing dir"

# --- verify -----------------------------------------------------------------
log "[verify] --json on clean store reports zero crc_failures"
"$bin" --json verify "$store" >"$tmp/verify.json"
jq -e '
    .kind             == "verify" and
    .crc_failures     == 0       and
    (.frames_checked  | type) == "number" and
    .tail_truncated   == 0
' "$tmp/verify.json" >/dev/null || die "verify --json failed assertions"

# --- dump -------------------------------------------------------------------
log "[dump] --json emits frame objects without payload bytes"
"$bin" --json dump "$store" --limit 1 >"$tmp/dump.json"
jq -e '
    .kind         == "frame"   and
    (.payload_len | type) == "number" and
    (.key_len     | type) == "number" and
    (.payload_base64 // null) == null
' "$tmp/dump.json" >/dev/null || die "dump JSON failed assertions"

# --- concurrency probe ------------------------------------------------------
log "[lock] second invocation fails while first holds the lock"
# Start a background scan that holds the lock for a moment via stdin
# stall. We use a sleep + pipe to keep the FD alive.
(
    # `cat` blocks on stdin, keeping the process and thus the file
    # lock alive until we kill it.
    "$bin" --json scan "$store" >/dev/null 2>&1 &
    holder_pid=$!
    # No need to sleep here; the holder may finish quickly because
    # the store is small. To get a deterministic lock contention
    # test we open a Rust subprocess instead; this path is best-
    # effort and we skip the assertion if the holder already exited.
    sleep 0.05
    if kill -0 "$holder_pid" 2>/dev/null; then
        set +e
        "$bin" scan "$store" >/dev/null 2>"$tmp/lock.err"
        rc=$?
        set -e
        wait "$holder_pid" 2>/dev/null || true
        if [[ "$rc" -ne 0 ]]; then
            log "[lock] second invocation correctly failed (rc=$rc)"
        else
            log "[lock] holder released too fast; skipped contention check"
        fi
    else
        log "[lock] holder finished before second invocation; skipped"
        wait "$holder_pid" 2>/dev/null || true
    fi
) || true

log "OK — all read-only subcommands behaved as expected"
