# datawal wire-format corpus

These fixtures freeze the v0.1-pre on-disk format. They are consumed by
[`tests/corpus_fixtures.rs`](../corpus_fixtures.rs).

Each subdirectory is a complete datawal log directory laid out exactly
as it would appear on disk (one or more `00000NNN.dwal` segments). The
fixture is copied into a temp dir by the test harness before being
opened, so the committed bytes never change.

## Fixtures

| Subdir              | Bytes describe                                                   | Expected behaviour on open                 |
| ------------------- | ---------------------------------------------------------------- | ------------------------------------------ |
| `valid_log/`        | 3 raw appends: `alpha`, `beta`, `gamma`                          | scan returns 3 records; no tail damage     |
| `truncated_tail/`   | 3 raw appends; active segment hard-truncated by 5 bytes          | scan returns 2 records; report.tail_truncated=1 |
| `bad_crc/`          | 3 records across 3 segments; payload byte flipped in segment 2   | open returns hard error (CRC mismatch in a closed segment) |
| `unknown_version/`  | 1 record; version field overwritten with `0xCAFE`                | open returns hard error (unknown version) |
| `delete_tombstone/` | `put a=1`, `put b=2`, `put a=3`, `del b`                         | DataWal projection: 1 live key, `a=3`; underlying log has 4 framed records |
| `compact_to_output/`| output of `compact_to` from a source with deleted/overwritten keys | only 2 Put records (`keep=final`, `other=value`); no tombstones |

## Regenerating

Re-run the generator only when the wire format changes intentionally:

```sh
cargo run -p datawal-core --example gen_corpus
```

This will overwrite the binaries above. After regeneration, **bump
`WIRE_VERSION`** in `crates/datawal-core/src/format.rs` and update the
documentation in `docs/canon.md`.

## Why these fixtures are committed

Generating fixtures only at runtime is fine for round-trip tests, but
it cannot detect a silent regression in the writer that breaks
backwards compatibility with bytes already on disk. Committing the
exact byte sequences of v0.1-pre captures the format precisely so that
any future change to encoding or decoding is forced to be deliberate.
