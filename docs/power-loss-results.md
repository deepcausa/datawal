# Power-loss harness: verified runs

Sanitized snapshot of `dm-flakey` power-loss simulation harness runs. See
[`power-loss-testing.md`](power-loss-testing.md) for the harness contract,
what it does, and what it explicitly does not show.

The numbers below are reproducible from `scripts/power_loss_dm_flakey.sh`
with the same seed and ops on any host that satisfies the prerequisites in
`power-loss-testing.md`.

## Run 1 — 2026-05-21

### Host shape, sanitized

| Field | Value |
|---|---|
| OS family | Linux x86_64, apt-based distribution |
| Kernel | 6.12 series |
| device-mapper | flakey target available |
| Backing | loopback file under temporary local filesystem |
| Filesystem under test | ext4 |

No hostname, user, paths, kernel patchlevel, mount table, partition UUID,
device IDs, container IDs, environment, or raw `uname`/`mount`/`env`
output is published.

### Workload

| Field | Value |
|---|---|
| Ops | 50000 |
| Key length | 16 bytes |
| Value length | 96 bytes |
| Seed | 42 (deterministic) |
| Fsync policy | every op |
| Effective key space | ~4096 (deletes target live keys by construction) |
| Wall time | ~9 s |
| Puts | 47827 |
| Deletes | 2173 |

### Fault injection

| Phase | Action |
|---|---|
| 1 | `dm-flakey` table `up=600 down=0` (healthy) |
| 2 | Workload writes to ext4 on top of `dm-flakey`, fsync per op; oracle written on a separate local filesystem |
| 3 | `dm-flakey` reloaded to `error_writes` (every subsequent write is rejected by the device) |
| 4 | `umount -f` (errors expected and tolerated) |
| 5 | `dm-flakey` reloaded to healthy table |
| 6 | Remount, reopen `DataWal`, run validator |

### Result

#### Recovery report on reopen

| Metric | Value |
|---|---|
| Files scanned | 1 |
| Records replayed | 50000 |
| Tail segments truncated | 0 |
| Tail bytes discarded | 0 |
| Mid-stream errors | 0 |
| Last txid seen | 50000 |

#### Validator output

| Metric | Value |
|---|---|
| Observed live keys (on disk) | 3918 |
| Oracle live keys (expected) | 3918 |
| Survived (live in both) | 3918 |
| Dropped (oracle live missing on disk) | 0 |
| Oracle dead (tombstoned, correctly absent) | 178 |
| Extras (on disk but not in oracle) | 0 |
| Exit code | 0 |

### Reading

- Per-key prefix invariant held. Every key the oracle marked live after a
  successful `fsync` was present on disk with the exact expected payload.
  Every tombstoned key was correctly absent. No on-disk key appeared that
  was not produced by the workload.
- No tail truncation was required: every record whose `fsync` returned
  before the fault was durable past the `umount -f` and remount cycle, as
  expected when `fsync` is honored end-to-end by the storage stack under
  test.
- This run exercises the "happy" path of `dm-flakey error_writes` with
  `fsync` per op. To exercise the prefix property more aggressively, a
  future workload variant will buffer ops between fsyncs so the fault can
  catch in-flight writes that have not yet reached the device. That is
  expected to drive `dropped > 0` and `tail_bytes_discarded > 0` while
  still keeping `extras = 0`.

### Scope: what this run shows and does not show

This run shows: under a controlled `dm-flakey` `error_writes` fault on
ext4, the reopened `DataWal` matches the fsync-ordered oracle, with no
extras and no corrupted payloads, for a 50k-operation workload.

This run does not show:

- behaviour under a real power cut on physical hardware,
- behaviour on lying storage (drives that ack `fsync` before persisting),
- behaviour on networked filesystems,
- behaviour on filesystems other than ext4,
- the `dm-log-writes` intermediate-state replay property (see roadmap).

This is stricter than process-level crash testing but is not a substitute
for real power-cut testing on real hardware. DataWal trusts the storage
stack below it to honor `fsync`.

### How to reproduce

On a Linux host with the prerequisites listed in `power-loss-testing.md`:

```sh
sudo \
  DATAWAL_POWERLOSS_OPS=50000 \
  DATAWAL_POWERLOSS_SEED=42 \
  DATAWAL_POWERLOSS_KEY_LEN=16 \
  DATAWAL_POWERLOSS_VALUE_LEN=96 \
  ./scripts/power_loss_dm_flakey.sh
```

Exit code 0 with the validator line

```text
power_loss_validate: OK observed_live=3918 oracle_live=3918 survived=3918 dropped=0 oracle_dead=178 (extras=0)
```

is the expected outcome with these inputs.
