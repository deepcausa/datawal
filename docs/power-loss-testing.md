# Power-loss testing (Linux device-mapper)

This page documents the **device-mapper power-loss harness**, which
exercises `DataWal` under simulated power-loss-class faults on Linux
using the kernel's `dm-flakey` target.

The harness is shipped as two runnable examples
(`examples/power_loss_workload.rs`, `examples/power_loss_validate.rs`)
plus two driver scripts (`scripts/power_loss_dm_flakey.sh`,
`scripts/power_loss_cleanup.sh`). It is intentionally **not** part of
the default test suite, **not** part of CI, and **not** run by
`cargo test`. It is a tool you run by hand, as root, on a Linux box
you trust to spawn loop devices and to leave a directory under
`/tmp/datawal-powerloss-*` lying around if something goes wrong.

It is **not** a real power-loss test. The host is up the whole time;
the kernel page cache, controller cache, and physical disk are
unaffected. Only the writes that reach the device-mapper layer of the
test stack are dropped. This catches a strict superset of the
`SIGKILL` cases already covered by
`crates/datawal-core/tests/crash_injection.rs`, but it is **not** a
substitute for testing on a machine where you actually pull the
power cord.

See issue [#31](https://github.com/deepcausa/datawal/issues/31) for
the design rationale.

## What it does

The orchestrator script does, in order:

1. Allocates a sparse backing file under `/tmp/datawal-powerloss-${ID}`
   (default size `256 MiB`).
2. Attaches it to a loop device (`losetup --show -f`).
3. Builds a `dm-flakey` device on top of the loop device, initially in
   the **healthy** mapping (`flakey ... 0 600 0` — 600 seconds up, 0
   seconds down).
4. Formats the dm device with `ext4` and mounts it under
   `${WORK_ROOT}/mnt`.
5. Runs `power_loss_workload` against `${WORK_ROOT}/mnt/wal`. The
   workload appends a deterministic stream of `put` / `delete` ops to
   the `DataWal`, calling `fsync` after every op. After each
   successful `fsync` it appends one base64 JSONL line to an oracle
   file kept on a **separate** filesystem (`${WORK_ROOT}/oracle.jsonl`,
   on the host's `/tmp`, never on the dm device).
6. Issues a host-side `sync` and then reloads the dm-flakey table to
   the **fault** mapping (`flakey ... 0 0 60 1 error_writes` — 0
   seconds up, 60 seconds down, error every write). All writes that
   the kernel has not yet drained to the device are now failed back
   to the caller.
7. Force-unmounts the mountpoint (`umount -f`, falling back to
   `umount -l` if necessary). I/O errors from the fault layer are
   expected at this point.
8. Reloads the dm-flakey table back to the **healthy** mapping and
   remounts the filesystem. This is the post-power-loss reopen path.
9. Runs `power_loss_validate` against the same `${WORK_ROOT}/mnt/wal`.
   The validator opens the store, runs `RecoveryReport`, then checks
   that the reopened keydir is a **prefix of the fsync-ordered
   oracle** per key.

The workload and the validator are independent binaries that
communicate only through (a) the on-disk store and (b) the JSONL
oracle. There is no shared state.

## What is being checked

The validator enforces three on-disk invariants against the oracle:

- **Inv 3 (per-key prefix).** For every key that the oracle says is
  live at some `fsync`-ordered prefix, the reopened store must
  return either the same payload or no value at all.
- **Inv 4 (no payload corruption).** A returned value must match the
  oracle's payload bytes exactly. A wrong payload is a hard failure.
- **Inv 5 (no extras).** No key in the reopened keydir may be absent
  from `oracle_live ∪ oracle_dead`. The store is a **truncation** of
  the workload, never a fabrication.

It also surfaces `RecoveryReport` from `RecordLog::open`:
`files_scanned`, `records_replayed`, `tail_truncated`,
`tail_bytes_discarded`, `mid_stream_errors`, `last_txid_seen`. A
non-zero `mid_stream_errors` is treated as a hard failure (datawal
`0.1.x` aborts recovery on the first CRC mismatch in a sealed
segment).

The validator deliberately does **not** assert anything about
keys whose oracle effect is `Dead` (a `delete` was issued). It cannot
distinguish "tombstone applied" from "put + delete both lost" using
only the on-disk state, and treating either outcome as wrong would
overconstrain the harness.

## Prerequisites

The orchestrator refuses to run unless all of these hold:

- Linux. macOS and Windows are explicitly out of scope.
- `root`. `dm-flakey` and `losetup` both require it.
- These binaries on `$PATH`: `losetup`, `dmsetup`, `mkfs.ext4`,
  `mount`, `umount`, `blockdev`, `cargo`, `dd`.
- Kernel module `dm-flakey` available
  (`modprobe dm-flakey || lsmod | grep dm-flakey`).
- Free space under `/tmp` of at least `DATAWAL_POWERLOSS_SIZE_MB`.

The harness only writes to:
- `/tmp/datawal-powerloss-${ID}/...` (backing file, mountpoint, oracle).
- The dm device named `datawal-test-${ID}`.
- The loop device returned by `losetup --show -f` for that backing file.

The cleanup script enforces the same prefix guard; it refuses to
touch anything that does not match.

## Environment variables

| Variable | Default | Meaning |
| --- | --- | --- |
| `DATAWAL_POWERLOSS_SIZE_MB` | `256` | Backing-file size in MiB. Min 32. |
| `DATAWAL_POWERLOSS_FS` | `ext4` | Filesystem on the dm device. Only `ext4` is supported in v1. |
| `DATAWAL_POWERLOSS_OPS` | `50000` | Op count for the workload. `0` = unbounded (must combine with `DURATION`). |
| `DATAWAL_POWERLOSS_DURATION` | `0` | Wall-clock budget in seconds. `0` = unbounded (must combine with `OPS`). |
| `DATAWAL_POWERLOSS_SEED` | `42` | PRNG seed for key and payload generation. |
| `DATAWAL_POWERLOSS_KEY_LEN` | `16` | Key length in bytes. `1..=512`. |
| `DATAWAL_POWERLOSS_VALUE_LEN` | `96` | Payload length in bytes. `1..=65536`. |
| `DATAWAL_POWERLOSS_KEEP_ARTIFACTS` | `0` | If `1`, do not delete `/tmp/datawal-powerloss-${ID}` on success. Always kept on failure. |

At least one of `OPS` or `DURATION` must be non-zero.

## Exit codes

| Code | Meaning |
| --- | --- |
| `0` | Reopen check passed: the post-fault store is a per-key prefix of the fsync-ordered oracle. |
| `1` | Invariant violated. Artefacts under `/tmp/datawal-powerloss-${ID}` are kept for inspection regardless of `KEEP_ARTIFACTS`. |
| `2` | Setup error: missing tool, not Linux, not root, dm-flakey unavailable, mkfs failed, mount failed, build failed. |

Both Rust binaries follow the same convention. The shell driver
propagates the highest-priority code it has seen: setup error beats
invariant violation beats success.

## Running

The simplest invocation, as root, from the repo root:

```bash
sudo ./scripts/power_loss_dm_flakey.sh
```

Override the workload size:

```bash
sudo DATAWAL_POWERLOSS_OPS=200000 \
     DATAWAL_POWERLOSS_VALUE_LEN=1024 \
     ./scripts/power_loss_dm_flakey.sh
```

Keep the artefacts on success (for cargo-fmt-style inspection of the
on-disk segments after a clean run):

```bash
sudo DATAWAL_POWERLOSS_KEEP_ARTIFACTS=1 \
     ./scripts/power_loss_dm_flakey.sh
```

To run the binaries by hand without the dm-flakey wrapper (useful for
sanity-checking the oracle / validator path on a real disk, with no
fault injection at all):

```bash
cargo build --release \
  --example power_loss_workload \
  --example power_loss_validate

mkdir -p /tmp/datawal-noflakey/wal
target/release/examples/power_loss_workload \
  --work-dir /tmp/datawal-noflakey/wal \
  --oracle /tmp/datawal-noflakey/oracle.jsonl \
  --ops 10000

target/release/examples/power_loss_validate \
  --work-dir /tmp/datawal-noflakey/wal \
  --oracle /tmp/datawal-noflakey/oracle.jsonl
```

Without fault injection the validator should always print `OK` and
exit `0`. If it does not, the bug is in the harness itself, not in
`datawal`.

## Cleanup

The orchestrator already cleans up on `EXIT`, `INT`, and `TERM`. The
separate cleanup script is for the case where the orchestrator was
killed before its trap ran (`SIGKILL`, host reboot, OOM kill, etc.):

```bash
sudo ./scripts/power_loss_cleanup.sh
```

It is idempotent. It only touches:
- Mountpoints whose path starts with `/tmp/datawal-powerloss-`.
- dm devices whose name starts with `datawal-test-`.
- Loop devices whose backing file path starts with
  `/tmp/datawal-powerloss-`.

Any other dm device, mount, or loop device on the host is left
untouched. The script returns `0` whenever it finishes; "something
was dirty but I cleaned it" is success.

## Interpreting a run

A clean run prints, in order:

- `[setup]` lines from the orchestrator (loop device, dm device,
  filesystem, mount).
- A workload progress line per ~10k ops.
- `[fault]` lines when the dm-flakey table is flipped.
- `[validate]` lines showing the `RecoveryReport` and the per-key
  totals.
- A final `OK observed_live=… oracle_live=… survived=… dropped=…
  oracle_dead=… (extras=0)` line, then exit `0`.

A failed run leaves the work directory in place. Useful next steps:

- Re-read the JSONL oracle: it lists every op in fsync order with
  base64-encoded key and payload. The last `seq` value tells you how
  far the workload got before the table flipped.
- Re-run the validator by hand against the on-disk store and the
  oracle. The same binary that ran inside the harness can be invoked
  from outside it.
- Inspect segments with `datawal scan` or `datawal report` from
  `datawal-cli` (shipped in `0.1.4`).
- Open the segment files directly with `xxd` and check the trailing
  bytes against the corpus layout described in `format.rs`.

## What this is not

- **Not a real power-loss test.** A real power-loss test pulls the
  power cord (or `echo b > /proc/sysrq-trigger`, or yanks a VM's
  virtio-blk). The host kernel survives this harness; only the
  device-mapper layer drops writes.
- **Not a substitute for `crash_injection.rs`.** The SIGKILL tests in
  `tests/crash_injection.rs` exercise the in-process write path and
  catch logic bugs that `dm-flakey` cannot reach (the workload never
  gets killed mid-`append`). Both layers are useful.
- **Not a substitute for `dm-log-writes` replay.** `dm-flakey` drops
  writes; it does not let you **replay** the I/O log and check
  every intermediate state. A future track will add a
  `dm-log-writes`-based harness; see the roadmap.
- **Not safe on a shared host.** It creates real loop devices and
  real dm devices in the global kernel namespaces. Run it on a
  scratch VM or a dedicated box.
- **Not safe on NFS, tmpfs, or any filesystem that does not honour
  `fsync`.** The whole harness assumes the underlying storage stack
  treats `fsync` as a hard durability barrier. The dm-flakey device
  is built on top of a loop device on top of a real local filesystem
  for that reason; do not move the backing file to NFS.
- **Not part of CI.** The example and the scripts only have to
  **compile** in the default workspace; they are not executed
  automatically. CI signals remain unchanged.
- **Not a benchmark.** Throughput numbers from this harness are not
  comparable across machines and do not reflect real workloads.

## Honest claim wording

If you want to cite this harness in release notes or external docs,
use language no stronger than:

> Exercised under a Linux device-mapper `dm-flakey` harness with
> ext4. After dropping every write that had not reached the
> device, the reopened store was a per-key prefix of the
> fsync-ordered oracle. This is a stricter test than `SIGKILL`-only
> crash injection; it is not a substitute for testing on a machine
> with a real power cord.

Anything stronger overclaims.
