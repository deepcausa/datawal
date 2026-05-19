# datawal — related work

A non-exhaustive map of nearby Rust crates and external systems, plus where
datawal is meant to land relative to them. Entries are sketches, not reviews.

## External Rust crates in nearby design space

- **sled** — embedded KV with crash-safe log structured storage. Opaque on
  disk, no clean export, undergoing a rewrite as `bloodstone`. datawal's
  point of differentiation: explicit canonical log, clean export, formal
  spec.
- **redb** — single-file MVCC KV. Strong durability story, opaque format.
- **rusqlite** in WAL mode — battle-tested, but SQL-shaped.
- **rocksdb** bindings — heavyweight LSM; not in datawal's scope.
- **fjall** — LSM in Rust; closer to a "real" KV engine. Different niche.
- **bitcasky** — Bitcask-style; dormant. Closest in spirit to `DataWal`'s
  projection model.
- **okaywal** — WAL-only crate, no KV projection, no compaction. Closest in
  spirit to `RecordLog`.

## External non-Rust systems for context

- **Kafka log segments** — segment + index pattern.
- **LevelDB / RocksDB WAL** — framed records with checksums.
- **Bitcask** (Riak) — KV projection of an append-only log with manual
  compaction. This is the closest mental model for `DataWal`.
- **Git** — content-addressed object store. The mental model for an eventual
  `datawal-cas`.

## Where datawal aims to sit

```
                   transparent on-disk format
                              ▲
                              │
                              │
        bitcasky  ─────────── │ ─────── datawal (target)
                              │
                              │
                              │
      sled  ─────             │
              \\               │
               \\              │
                \\             │
                 rocksdb ──────┴── opaque on-disk format
```

Differentiators datawal aims for:

1. Canonical, documented wire format.
2. Clean JSON/JSONL export at any time.
3. TLA+ specs for log invariants and compaction.
4. Bytes-only core; codecs and value types live above.
5. Small surface area, small dependency tree.
