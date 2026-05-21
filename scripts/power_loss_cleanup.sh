#!/usr/bin/env bash
# Idempotent teardown for any device-mapper / loop / mount artefacts left
# behind by an interrupted `power_loss_dm_flakey.sh` run.
#
# Safety:
#   - Only touches dm-mapper devices named `datawal-test-*`.
#   - Only touches mount points / work dirs under `/tmp/datawal-powerloss-*`.
#   - Only detaches loop devices whose backing file matches the pattern above.
#   - Refuses to run on non-Linux or as non-root (no-op exit 2).
#
# Exit codes:
#   0 — nothing was wrong, or everything was cleaned up cleanly.
#   2 — preflight failed (not Linux / not root / missing tool).
#
# This script never returns 1; "something was dirty but I cleaned it"
# is still a success.

set -euo pipefail

readonly SCRIPT_NAME="${0##*/}"
readonly DM_PREFIX="datawal-test-"
readonly WORK_PREFIX="/tmp/datawal-powerloss-"

log() {
  printf '%s: %s\n' "$SCRIPT_NAME" "$*" >&2
}

die_setup() {
  log "setup error: $*"
  exit 2
}

if [[ "$(uname -s)" != "Linux" ]]; then
  die_setup "Linux-only (uname=$(uname -s))"
fi
if [[ "$(id -u)" -ne 0 ]]; then
  die_setup "must run as root"
fi
for tool in dmsetup losetup umount findmnt awk; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    die_setup "missing required tool: $tool"
  fi
done

# 1. Unmount anything under /tmp/datawal-powerloss-*/mnt
while IFS= read -r line; do
  [[ -n "$line" ]] || continue
  log "umount $line"
  umount -f "$line" 2>/dev/null || umount -l "$line" 2>/dev/null || log "  (failed; continuing)"
done < <(findmnt -rn -o TARGET | awk -v p="$WORK_PREFIX" 'index($0, p)==1')

# 2. Remove dm-flakey devices named datawal-test-*
while IFS= read -r name; do
  [[ -n "$name" ]] || continue
  log "dmsetup remove $name"
  dmsetup remove "$name" 2>/dev/null || log "  (failed; continuing)"
done < <(dmsetup ls 2>/dev/null | awk '{print $1}' | awk -v p="$DM_PREFIX" 'index($0, p)==1')

# 3. Detach loop devices whose backing file lives under the work prefix
while IFS= read -r line; do
  [[ -n "$line" ]] || continue
  dev="$(awk '{print $1}' <<< "$line" | tr -d ':')"
  log "losetup -d $dev"
  losetup -d "$dev" 2>/dev/null || log "  (failed; continuing)"
done < <(losetup -a 2>/dev/null | awk -v p="$WORK_PREFIX" '$0 ~ p')

# 4. Remove work directories that match the pattern. Only after we have
#    unmounted everything that could have been pointing at them.
shopt -s nullglob
for d in "${WORK_PREFIX}"*; do
  if [[ -d "$d" ]]; then
    if findmnt -rn "$d" >/dev/null 2>&1 || findmnt -rn "$d/mnt" >/dev/null 2>&1; then
      log "$d still has mounts after step 1; leaving it alone"
      continue
    fi
    log "rm -rf $d"
    rm -rf "$d"
  fi
done

log "cleanup complete"
exit 0
