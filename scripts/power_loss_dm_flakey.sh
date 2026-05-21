#!/usr/bin/env bash
# Orchestrate a dm-flakey-based power-loss test for `datawal`.
#
# The harness:
#   1. Creates a sparse backing file under /tmp.
#   2. Wraps it in a loop device.
#   3. Stacks a dm-flakey target on top with `up_interval` only (no faults).
#   4. mkfs.ext4 on the flakey device, mounts it under /tmp/datawal-powerloss-<id>/mnt.
#   5. Runs `power_loss_workload` against that mount.
#   6. After the workload exits, reloads the flakey table with
#      `error_writes` for a configurable window — every subsequent write
#      to the device errors. Then forces umount and remount.
#   7. Runs `power_loss_validate` against the remounted filesystem.
#   8. Cleans up (umount, dmsetup remove, losetup -d, rm).
#
# This is a *manual validation tool*, not a CI job. It requires root,
# Linux, dmsetup, losetup, mkfs.ext4. It refuses to run on non-Linux,
# without root, or without the required tools. It does NOT operate on
# operator-supplied block devices in v1 — the backing file is always a
# sparse file under /tmp scoped to a `datawal-powerloss-<id>` directory.
#
# Required env vars (no defaults — the script refuses to invent them):
#   none (sensible defaults below; override via env).
#
# Optional env vars:
#   DATAWAL_POWERLOSS_SIZE_MB        size of backing file (default: 256)
#   DATAWAL_POWERLOSS_FS             filesystem (default: ext4; only ext4 in v1)
#   DATAWAL_POWERLOSS_OPS            workload op budget (default: 50000)
#   DATAWAL_POWERLOSS_DURATION       workload wall-clock seconds (default: 0 = no limit)
#   DATAWAL_POWERLOSS_SEED           workload PRNG seed (default: 42)
#   DATAWAL_POWERLOSS_KEY_LEN        bytes per key (default: 16)
#   DATAWAL_POWERLOSS_VALUE_LEN      bytes per value (default: 96)
#   DATAWAL_POWERLOSS_KEEP_ARTIFACTS retain work dir & oracle on success
#                                    (default: 0; set to 1 to keep)
#
# Exit codes mirror the Rust examples:
#   0 — workload + validate both clean.
#   1 — validator surfaced an invariant violation (this is the
#       interesting failure mode and the reason the harness exists).
#   2 — setup error (missing tool, not Linux, not root, mkfs failed, etc.).
#
# Safety: every artefact lives under /tmp/datawal-powerloss-<id>. The
# dm-mapper device name is `datawal-test-<id>`. The cleanup trap fires
# on any exit path. If the script is killed mid-flight, run
# `scripts/power_loss_cleanup.sh` to reclaim devices and mounts.

set -euo pipefail

readonly SCRIPT_NAME="${0##*/}"
readonly ID="$$"
readonly WORK_ROOT="/tmp/datawal-powerloss-${ID}"
readonly DM_NAME="datawal-test-${ID}"
readonly BACKING_FILE="${WORK_ROOT}/backing.img"
readonly MNT="${WORK_ROOT}/mnt"
readonly WAL_DIR="${MNT}/wal"
readonly ORACLE="${WORK_ROOT}/oracle.jsonl"

readonly SIZE_MB="${DATAWAL_POWERLOSS_SIZE_MB:-256}"
readonly FS_TYPE="${DATAWAL_POWERLOSS_FS:-ext4}"
readonly OPS="${DATAWAL_POWERLOSS_OPS:-50000}"
readonly DURATION="${DATAWAL_POWERLOSS_DURATION:-0}"
readonly SEED="${DATAWAL_POWERLOSS_SEED:-42}"
readonly KEY_LEN="${DATAWAL_POWERLOSS_KEY_LEN:-16}"
readonly VALUE_LEN="${DATAWAL_POWERLOSS_VALUE_LEN:-96}"
readonly KEEP="${DATAWAL_POWERLOSS_KEEP_ARTIFACTS:-0}"

LOOP_DEV=""
DM_CREATED=0
MOUNTED=0
WORK_CREATED=0

log() {
  printf '%s: %s\n' "$SCRIPT_NAME" "$*" >&2
}

die_setup() {
  log "setup error: $*"
  exit 2
}

cleanup() {
  local rc=$?
  set +e
  if (( MOUNTED )); then
    umount -f "$MNT" 2>/dev/null || umount -l "$MNT" 2>/dev/null
  fi
  if (( DM_CREATED )); then
    dmsetup remove "$DM_NAME" 2>/dev/null
  fi
  if [[ -n "$LOOP_DEV" ]]; then
    losetup -d "$LOOP_DEV" 2>/dev/null
  fi
  if (( WORK_CREATED )) && [[ "$KEEP" != "1" || $rc -ne 0 ]]; then
    # On non-zero exit OR when KEEP is off, wipe the artefacts.
    # On rc==0 with KEEP=1, leave them in place for inspection.
    if [[ $rc -eq 0 && "$KEEP" == "1" ]]; then
      log "keeping artefacts under $WORK_ROOT (DATAWAL_POWERLOSS_KEEP_ARTIFACTS=1)"
    else
      rm -rf "$WORK_ROOT" 2>/dev/null
    fi
  fi
  exit "$rc"
}
trap cleanup EXIT INT TERM

# --- preflight ---

if [[ "$(uname -s)" != "Linux" ]]; then
  die_setup "Linux-only harness (uname=$(uname -s))"
fi

if [[ "$(id -u)" -ne 0 ]]; then
  die_setup "must run as root (need losetup, dmsetup, mount); try sudo $0"
fi

# Try to locate cargo from invoking user (sudo on Debian/Ubuntu re-writes PATH
# via secure_path, so 'sudo -E' is not enough; do not depend on PATH passthrough).
if ! command -v cargo >/dev/null 2>&1; then
  found=""
  if [[ -n "${SUDO_USER:-}" ]]; then
    user_home=$(getent passwd "$SUDO_USER" | cut -d: -f6 2>/dev/null || true)
    for cand in "${user_home:-}/.cargo/bin/cargo" "/home/${SUDO_USER}/.cargo/bin/cargo"; do
      [[ -n "$cand" && -x "$cand" ]] && { found="$cand"; break; }
    done
  fi
  if [[ -z "$found" ]]; then
    for cand in /home/*/.cargo/bin/cargo /root/.cargo/bin/cargo; do
      [[ -x "$cand" ]] && { found="$cand"; break; }
    done
  fi
  if [[ -n "$found" ]]; then
    export PATH="$(dirname "$found"):$PATH"
    log "preflight: using cargo from $found"
  fi
fi

# The rustup shim resolves the toolchain from $RUSTUP_HOME (default $HOME/.rustup).
# When invoked via sudo, $HOME is /root and the default toolchain is unset,
# producing 'no default is configured'. Re-point at the invoking user's home.
if [[ -n "${SUDO_USER:-}" ]]; then
  sudo_home=$(getent passwd "$SUDO_USER" | cut -d: -f6 2>/dev/null || true)
  if [[ -n "$sudo_home" && -d "$sudo_home/.rustup" ]]; then
    export RUSTUP_HOME="$sudo_home/.rustup"
    export CARGO_HOME="$sudo_home/.cargo"
    log "preflight: RUSTUP_HOME=$RUSTUP_HOME"
  fi
fi
if ! cargo --version >/dev/null 2>&1; then
  export RUSTUP_TOOLCHAIN="${RUSTUP_TOOLCHAIN:-stable}"
  log "preflight: cargo had no default; forcing RUSTUP_TOOLCHAIN=$RUSTUP_TOOLCHAIN"
fi

for tool in losetup dmsetup mkfs."$FS_TYPE" mount umount blockdev cargo dd; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    die_setup "missing required tool: $tool"
  fi
done

if [[ "$FS_TYPE" != "ext4" ]]; then
  die_setup "only ext4 is supported in v1 (got $FS_TYPE)"
fi

if [[ "$SIZE_MB" -lt 32 ]]; then
  die_setup "DATAWAL_POWERLOSS_SIZE_MB must be >= 32 (got $SIZE_MB)"
fi

if [[ "$OPS" -eq 0 && "$DURATION" -eq 0 ]]; then
  die_setup "set at least one of DATAWAL_POWERLOSS_OPS / DATAWAL_POWERLOSS_DURATION non-zero"
fi

# --- build the harness binaries up front (no compile during the run) ---

log "preflight: building examples in release mode"
cd "$(dirname "$0")/.."
cargo build --release \
  --example power_loss_workload \
  --example power_loss_validate \
  >&2 || die_setup "cargo build of examples failed"

readonly WORKLOAD_BIN="$(pwd)/target/release/examples/power_loss_workload"
readonly VALIDATE_BIN="$(pwd)/target/release/examples/power_loss_validate"

[[ -x "$WORKLOAD_BIN" ]] || die_setup "workload binary missing at $WORKLOAD_BIN"
[[ -x "$VALIDATE_BIN" ]] || die_setup "validate binary missing at $VALIDATE_BIN"

# --- stage the work root ---

mkdir -p "$WORK_ROOT"
WORK_CREATED=1
mkdir -p "$MNT"

log "stage: WORK_ROOT=$WORK_ROOT DM_NAME=$DM_NAME size=${SIZE_MB}MiB fs=$FS_TYPE"

# Create sparse backing file.
truncate -s "${SIZE_MB}M" "$BACKING_FILE" || die_setup "truncate backing file failed"

# Attach to a loop device.
LOOP_DEV="$(losetup --show -f "$BACKING_FILE")" || die_setup "losetup attach failed"
log "loop: $LOOP_DEV"

# Build the initial dm-flakey table. Sector count = file size in 512-byte sectors.
SECTORS="$(blockdev --getsz "$LOOP_DEV")"
[[ "$SECTORS" -gt 0 ]] || die_setup "blockdev --getsz returned $SECTORS"

# Healthy table: up_interval=600 down_interval=0 -> never faults, just a
# passthrough we can later reload with a faulting variant.
HEALTHY_TABLE="0 $SECTORS flakey $LOOP_DEV 0 600 0"
dmsetup create "$DM_NAME" --table "$HEALTHY_TABLE" || die_setup "dmsetup create failed"
DM_CREATED=1
log "dm-flakey created: /dev/mapper/$DM_NAME table='$HEALTHY_TABLE'"

# mkfs and mount.
mkfs."$FS_TYPE" -q -F "/dev/mapper/$DM_NAME" || die_setup "mkfs.$FS_TYPE failed"
mount -t "$FS_TYPE" "/dev/mapper/$DM_NAME" "$MNT" || die_setup "mount failed"
MOUNTED=1
log "mount: /dev/mapper/$DM_NAME on $MNT ($FS_TYPE)"

mkdir -p "$WAL_DIR"

# --- workload phase ---

log "workload: starting ops=$OPS duration=${DURATION}s seed=$SEED key_len=$KEY_LEN value_len=$VALUE_LEN"
WL_RC=0
"$WORKLOAD_BIN" \
  --work-dir "$WAL_DIR" \
  --oracle   "$ORACLE" \
  --seed     "$SEED" \
  --ops      "$OPS" \
  --duration "$DURATION" \
  --key-len  "$KEY_LEN" \
  --value-len "$VALUE_LEN" \
  || WL_RC=$?

if (( WL_RC != 0 )); then
  log "workload exited with code $WL_RC; aborting before fault injection"
  exit "$WL_RC"
fi

[[ -s "$ORACLE" ]] || die_setup "oracle file is empty at $ORACLE — workload produced no claims"

# --- fault injection ---
#
# Flip the dm-flakey table to error every write for a window long enough
# to cover the umount flush. The kernel will see EIO on writes; the
# subsequent umount may report errors (we use umount -f / -l to detach
# regardless). Then we flip back to the healthy table and remount, so
# the validator runs against a filesystem whose post-workload writes
# were dropped — the proxy for power loss.

FAULT_TABLE="0 $SECTORS flakey $LOOP_DEV 0 0 60 1 error_writes"
log "fault: reloading dm-flakey to error_writes; sync may fail (expected)"

# `sync` first so dirty pages from the workload's final fsync are
# already pushed; what we then drop is anything generated by umount
# itself.
sync || true

dmsetup suspend "$DM_NAME"
dmsetup reload  "$DM_NAME" --table "$FAULT_TABLE"
dmsetup resume  "$DM_NAME"

# Force umount; errors are expected because writes now EIO.
log "umount -f (errors expected)"
if ! umount -f "$MNT" 2>&1 | sed -e "s/^/$SCRIPT_NAME: umount: /" >&2; then
  log "umount -f failed; retrying with -l (lazy)"
  umount -l "$MNT" 2>/dev/null || true
fi
MOUNTED=0

# Restore the healthy table so the remount can flush its journal.
HEALTHY_TABLE="0 $SECTORS flakey $LOOP_DEV 0 600 0"
dmsetup suspend "$DM_NAME"
dmsetup reload  "$DM_NAME" --table "$HEALTHY_TABLE"
dmsetup resume  "$DM_NAME"
log "fault: reloaded to healthy table"

# Remount.
mount -t "$FS_TYPE" "/dev/mapper/$DM_NAME" "$MNT" || die_setup "remount failed"
MOUNTED=1
log "remount: /dev/mapper/$DM_NAME on $MNT"

# --- validate phase ---

log "validate: starting against $WAL_DIR (oracle=$ORACLE)"
VR_RC=0
"$VALIDATE_BIN" \
  --work-dir "$WAL_DIR" \
  --oracle   "$ORACLE" \
  || VR_RC=$?

if (( VR_RC == 0 )); then
  log "validate: OK"
  exit 0
elif (( VR_RC == 1 )); then
  log "validate: INVARIANT VIOLATED (rc=1); artefacts kept at $WORK_ROOT"
  # Force-keep on failure regardless of KEEP flag.
  KEEP=1
  exit 1
else
  log "validate: setup error (rc=$VR_RC)"
  exit "$VR_RC"
fi
