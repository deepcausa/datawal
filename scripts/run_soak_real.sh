#!/usr/bin/env bash
# Run the soak example in `real` mode with caller-supplied JSONL fixtures.
#
# Required env vars (no defaults — the script refuses to invent them):
#   DATAWAL_SOAK_INPUT_SMALL
#   DATAWAL_SOAK_INPUT_MEDIUM
#   DATAWAL_SOAK_INPUT_LARGE
#
# Optional env vars (forwarded to the example when set; see docs/soak.md):
#   DATAWAL_SOAK_DURATION
#   DATAWAL_SOAK_ROTATE_EVERY
#   DATAWAL_SOAK_COMPACT_EVERY_ROTATIONS
#   DATAWAL_SOAK_PROGRESS_SECS
#   DATAWAL_SOAK_LOG_DIR
#   DATAWAL_SOAK_WORK_DIR
#
# Exit codes mirror the example: 0 = invariant held, 1 = invariant broken,
# 2 = setup error.

set -euo pipefail

: "${DATAWAL_SOAK_INPUT_SMALL:?set DATAWAL_SOAK_INPUT_SMALL to a JSONL path}"
: "${DATAWAL_SOAK_INPUT_MEDIUM:?set DATAWAL_SOAK_INPUT_MEDIUM to a JSONL path}"
: "${DATAWAL_SOAK_INPUT_LARGE:?set DATAWAL_SOAK_INPUT_LARGE to a JSONL path}"

for var in DATAWAL_SOAK_INPUT_SMALL DATAWAL_SOAK_INPUT_MEDIUM DATAWAL_SOAK_INPUT_LARGE; do
  path="${!var}"
  if [[ ! -r "$path" ]]; then
    echo "datawal: soak: $var=$path is not a readable file" >&2
    exit 2
  fi
done

exec cargo run --release -p datawal --example soak -- --mode real
