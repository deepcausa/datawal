# datawal-cli

Read-only command-line inspector for [`datawal`](https://crates.io/crates/datawal)
stores. Ships a single binary named `datawal` with five subcommands:
`scan`, `get`, `report`, `verify`, `dump`.

Mutating operations (`put` / `delete` / `rotate` / `compact`) are
intentionally **not** in this crate during the 0.1.x line. Use the
library API for those, or a future `datawal-cli` release once the
mutate surface is design-reviewed.

## Install

```bash
# from the workspace (development):
cargo install --path crates/datawal-cli

# from crates.io (once published):
cargo install datawal-cli
```

The binary is named `datawal`. Once installed, `which datawal` should
print the path of the inspector.

## Subcommands

All subcommands take a store directory as a positional argument and
acquire the same cooperative single-writer lock the library uses:
running them against a store with a live writer fails with exit
code 1.

### `scan` — list records in segment order

```bash
datawal scan ./my-store
datawal scan ./my-store --limit 100
datawal scan ./my-store --from-segment 3 --from-offset 0
datawal --json scan ./my-store > records.ndjson
```

Walks every segment via `RecordLog::scan_iter`, the record-level lazy
iterator (segment-buffered, not zero-copy). Does not materialise the
whole log in memory.

### `get` — fetch the current value for a key

```bash
datawal get ./my-store --key alpha            # UTF-8 text (most ergonomic)
datawal get ./my-store --key-base64 aGVsbG8=  # base64
datawal get ./my-store --key-hex deadbeef     # hex
datawal --json get ./my-store --key alpha
```

Opens the store as a `DataWal` (last-write-wins KV projection) and
returns the live value. Exits with code 2 when the key is absent.

In human form, printable-ASCII values are printed literally on
stdout. Binary values fall back to a hint on stderr (`<binary
value, N bytes; use --bytes base64 or --bytes hex>`) so terminals
don't get garbled. Force a specific encoding with `--bytes
base64|hex`:

```bash
datawal get ./my-store --key alpha --bytes base64
datawal get ./my-store --key alpha --bytes hex
```

### `report` — print the `RecoveryReport`

```bash
datawal report ./my-store
datawal --json report ./my-store
```

Reports files scanned, records replayed, last-txid, tail-truncation
bytes, and any unsupported-version frames. Tail truncation is
reported via exit code 2; the data itself is recoverable.

### `verify` — re-CRC every frame

```bash
datawal verify ./my-store
datawal --json verify ./my-store
```

Walks every frame and re-verifies CRC32C. CRC failure in a sealed
segment exits 3; a truncated active-segment tail exits 2.

### `dump` — print raw frame headers

```bash
datawal dump ./my-store --limit 10
datawal --json dump ./my-store
```

Header-only output (no payload bytes). Useful for inspecting wire
layout on stores with large records.

## Human vs JSON output

The two output modes are deliberately different in shape and
guarantees.

**Human form (default)** — designed to be skimmable in a terminal.
Printable-ASCII keys and payloads are rendered literally (quoted as
`"foo bar"` when whitespace, quotes, or backslashes would otherwise
blur field boundaries). Binary bytes are rendered with an explicit
prefix — `b64:<base64>` by default, `hex:<hex>` when `--bytes hex`
is set — so the reader sees that the field is encoded and which
encoding is in use. Long keys and payloads are truncated to 64 bytes
with a trailing `...`; pass `--no-truncate` to disable truncation.
Override the heuristic with `--bytes auto|raw|base64|hex` on `scan`,
`get`, and `dump`.

**JSON form (`--json`)** — designed to be parsed. Always emits
base64-encoded bytes regardless of `--bytes`; never truncates; never
introduces alternative encoded fields. The `datawal.cli.v1` schema
is the source of truth for tooling and is not affected by any
human-rendering flag.

## JSON output schema

Every JSON object emitted with `--json` carries a literal
`"schema":"datawal.cli.v1"` field. The schema is conservative on
purpose: keys and payloads are base64-encoded (standard alphabet
with padding); field names use `snake_case`.

| Subcommand | `kind`     | Object fields                                                                                       |
|------------|------------|------------------------------------------------------------------------------------------------------|
| `scan`     | `record`   | `segment u32`, `offset u64`, `len u32`, `record_type str`, `txid u64`, `key_base64`, `payload_base64` |
| `get` hit  | `value`    | `key_base64`, `value_base64`, `value_len usize`                                                      |
| `get` miss | `miss`     | `key_base64`                                                                                         |
| `report`   | `report`   | `files_scanned u32`, `records_replayed u64`, `tail_truncated u32`, `tail_bytes_discarded u64`, `mid_stream_errors u32`, `unsupported_versions u32`, `last_txid_seen u64` |
| `verify`   | `verify`   | `frames_checked u64`, `crc_failures u64`, `tail_truncated u32`, `tail_bytes_discarded u64`, `last_segment u32`, `last_offset u64` |
| `dump`     | `frame`    | `segment u32`, `offset u64`, `len u32`, `record_type str`, `txid u64`, `key_len usize`, `payload_len usize` |

`record_type` is the string form: `Raw`, `Put`, or `Delete`.

The schema string `datawal.cli.v1` will be bumped to `v2` only on a
breaking change to field names, kinds, or value encodings. Adding
new optional fields is non-breaking.

## Exit codes

| Code | Meaning                                                                  |
|------|---------------------------------------------------------------------------|
| 0    | Success.                                                                 |
| 1    | User error (bad args, unparseable encoding), or store locked by another process. |
| 2    | Recoverable storage state: truncated active-segment tail, or `get` miss. |
| 3    | Hard storage error: CRC failure in a sealed segment, or decode error.    |

## Concurrency

Every subcommand acquires the same cooperative single-writer lock
(`<store>/.lock`) the library uses. Skipping the lock would risk
observing a partially-written tail and would break the single-writer
invariant of `RecordLog`. As a consequence:

- Only one inspector or writer at a time, per store directory.
- Killing the inspector releases the lock immediately (the OS drops
  the file descriptor).

## What this binary is not

- It does **not** mutate the store.
- It does **not** compact or export.
- It does **not** bypass the cooperative lock.
- It is **not** a query / analytics engine. There is no `select`,
  `where`, `index`, or `server` subcommand, and these names are
  reserved against future confusion.

## Shell smoke test

An end-to-end shell smoke test lives at
`examples/cli_read_smoke.sh`. It builds the binary, seeds a store
via the bundled `populate_smoke_store` example, then exercises every
subcommand in both human and `--json` forms and asserts on exit
codes and key invariants via `jq`. Requires `jq`.

```bash
crates/datawal-cli/examples/cli_read_smoke.sh
```

The script is not wired into CI; it is for downstream users to
sanity-check the binary against their own datawal stores. The
`tests/integration.rs` test suite covers the same surface from
inside `cargo test`.

## License

MIT OR Apache-2.0, mirroring the `datawal` library crate.
