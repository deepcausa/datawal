# AGENTS.md

A focused orientation file for any agent (LLM or human) opening this
repository for the first time. It complements `README.md`,
`docs/canon.md`, `docs/technical-decisions.md` and `docs/roadmap.md`
without duplicating them.

If you only read one file before touching anything, read this one.

## What this project is

`datawal` is an **append-only framed record log** plus a **bytes-based
last-write-wins KV projection**, written in Rust. It targets local
POSIX filesystems on Linux and macOS, single-process / single-writer.

The crate exposes two layers:

- `RecordLog` — segmented, framed, CRC32C-checked append-only log
  with valid-prefix recovery, explicit `fsync` and `rotate`. This is
  the substrate.
- `DataWal` — bytes-in-bytes-out KV layered on top of `RecordLog`,
  with `put` / `get` / `delete` / tombstone, manual `compact_to`,
  JSONL export.

Scope is _one process, one directory, framed records on disk_. It is
not a WAL for a transactional DB, not a distributed log, not a CAS,
not an event bus. See `docs/canon.md` § "Non-goals". For one-shot
atomic-write primitives see the sibling crate
[`safeatomic-rs`](https://github.com/deepcausa/safeatomic-rs).

## Hard invariants — do not break these

These are contractual. Violating any of them is a wire-format break
and requires a `WIRE_VERSION` bump + an entry in `CHANGELOG.md` and
`docs/technical-decisions.md`.

1. **Wire format is frozen at `WIRE_VERSION = 1`.** The frame layout
   (`MAGIC b"DWAL"`, 24-byte header + key + payload + 4-byte CRC32C,
   little-endian) is the public contract. Six corpus fixtures under
   `crates/datawal-core/tests/corpus/` lock the bytes against drift;
   the `corpus` CI job re-generates them on each run and compares
   SHA-256s. **Mutating an existing fixture is a wire-format break.**
2. **Recovery is defined as the longest valid prefix.** A truncated
   tail is reported via `RecoveryReport` but is **not** an error. A
   CRC mismatch in a sealed (non-active) segment **is** a hard
   error. A CRC mismatch in the active segment truncates back to the
   last valid record. This is model-checked in `formal/RecordLog.tla`.
3. **`RecordLog` is single-writer, single-process.** Concurrent
   processes are prevented at open time by a cooperative lock file
   (`fs2::FileExt::try_lock_exclusive`). Multi-writer support would
   be a different crate.
4. **Compaction is a snapshot-style rebuild into a target directory.**
   The source directory is read-only during compaction. There is no
   in-place mutation of segments. `Compaction.tla` is the reference.
5. **MSRV is Rust 1.75.0.** Bumping it is a minor-version event.
   Track via `rust-version` in `Cargo.toml` and the CI matrix.
6. **Public surface is the six re-exports in `lib.rs`:**
   `RecordLog`, `Record`, `RecordRef`, `RecoveryReport`, `DataWal`,
   `CompactionStats`, plus the `format::{RecordType, MAX_KEY_LEN,
   MAX_PAYLOAD_LEN, WIRE_VERSION}` constants. Adding to it is a
   minor-version event.

## What "honest" looks like here

The crate prefers **failing loudly** over silent best-effort:

- Lock not acquired? `Err`, no silent reentrancy.
- CRC mismatch mid-stream? Hard error in a sealed segment, truncate
  in the active segment. Either way the caller knows.
- Truncated tail? `RecoveryReport` reports byte-count and segment;
  recovery proceeds with the valid prefix.
- Compact into a non-empty target directory? Refused.
- Key or payload over the static limits (64 KiB / 64 MiB)?
  `format::DecodeError` before any I/O.

## Honest API surprises

Things that trip newcomers regardless of how good the rustdoc is:

1. **`DataWal::open` opens an existing log or creates one.** There is
   no `create_new` mode; existence is determined by whether any
   `[0-9]{8}\.dwal` file is present.
2. **`put` / `delete` are framed records, not in-memory mutations.**
   They write to the active segment and update the in-memory keydir.
   Crash-durability is per-segment, achieved by an explicit `fsync`.
3. **`compact_to` writes to a target directory, not in-place.** After
   a successful compaction the caller is responsible for swapping
   directories (e.g. rename + reopen). The crate does not do the
   swap because the right swap policy is application-specific.
4. **JSONL export is one record per line, base64-encoded payload.**
   It is intended for inspection and external migration, not for
   round-tripping; there is no `import_jsonl`.
5. **There is no MANIFEST file in v0.1-pre.** Segments are discovered
   by globbing `[0-9]{8}\.dwal`. Adding a MANIFEST is a v0.2 concern
   tracked in `docs/roadmap.md`.

## Layout

```
Cargo.toml                       # workspace root (single crate today)
crates/datawal-core/
  Cargo.toml                     # name = "datawal" (the published name)
  README.md                      # symlink -> ../../README.md
  src/
    lib.rs                       # public re-exports + module list
    format.rs                    # frame encode/decode + DecodeError
    segment.rs                   # one segment file, append/read primitives
    record_log.rs                # multi-segment log + recovery
    datawal.rs                   # KV projection + compact + export
    lock.rs                      # cooperative single-writer lock
  tests/
    record_log.rs                # RecordLog integration tests
    datawal.rs                   # DataWal integration tests
    integration.rs               # cross-cutting scenarios
    corpus_fixtures.rs           # asserts CRC/format on canned bytes
    corpus/                      # 6 canned wire-format fixtures
  examples/
    record_log_demo.rs
    datawal_kv_demo.rs
    tail_recovery_demo.rs
    gen_corpus.rs                # regenerates corpus fixtures
formal/                          # 3 TLA+ models + .cfg + reports
  RecordLog.tla                  # valid-prefix recovery
  KeydirProjection.tla           # KV projection correctness
  Compaction.tla                 # snapshot-style compaction
  README.md
docs/                            # public design notes
  canon.md
  technical-decisions.md
  roadmap.md
  related-work.md
.github/workflows/ci.yml         # rust matrix + dry-run + formal + corpus + release
LICENSE LICENSE-MIT LICENSE-APACHE
README.md AGENTS.md
```

There is also a **private** companion directory at `dev/` (gitignored)
containing dogfood call-site notes, migration pilots, decision drafts,
and references to non-public sibling work. It is **not** part of the
published artefact. See `dev/README.md` for navigation.

## Toolchain pinning

- Rust: MSRV `1.75.0` in `Cargo.toml`. CI matrix runs `stable` and
  `1.75.0`.
- `clippy`, `rustfmt`, `cargo doc`: run only on `stable`. Clippy
  diagnostics shift between Rust releases; gating MSRV on
  `clippy -D warnings` would create noise unrelated to the MSRV
  contract. The MSRV job runs `cargo check` and `cargo test`.
- TLA+: `tla2tools.jar` v1.8.0, fetched from the official
  `tlaplus/tlaplus` release URL pinned in `.github/workflows/ci.yml`
  (`TLA_TOOLS_URL` env). Java 21 (`actions/setup-java@v4` with
  `distribution: temurin`). Each TLC run greps for `Model checking
  completed. No error has been found.` and uploads its log as a
  `tlc-logs` artifact.
- `getrandom` is pinned in `Cargo.lock` to `0.3.4` to keep the MSRV
  1.75.0 job happy: the 0.4.x series declares `edition = "2024"` and
  `rust-version = "1.85"`, both unparseable by Cargo 1.75. Pulled in
  transitively via the `tempfile` dev-dependency only; no production
  code path uses it.

## The release flow

Short version:

1. Bump `version` in the workspace `Cargo.toml`. If MSRV changed,
   also bump `rust-version`.
2. Update `README.md` "MSRV" line if relevant. Add a `CHANGELOG.md`
   entry under the new version section.
3. Commit, push to `main`. Wait for CI to go green
   (`rust (stable)`, `rust (1.75.0)`, `formal`, `corpus`,
   `publish-dry-run` — five required signals).
4. Tag: `git tag vX.Y.Z && git push origin vX.Y.Z`. The tag name
   minus the `v` prefix **must** match `Cargo.toml`'s version; the
   `release` job verifies this and fails loudly otherwise.
5. The tag triggers a fresh CI run including the `release` job,
   which runs `cargo publish -p datawal` with
   `secrets.CARGO_REGISTRY_TOKEN`.
6. Verify: `cargo add datawal --version X.Y.Z` in a scratch crate,
   smoke-test the public API, check rendered docs on `docs.rs`
   (build queue is typically 5–10 min).

For pre-release versions (`X.Y.Z-alpha.N`, `-beta.N`, `-rc.N`) `cargo
add` requires the explicit version string; semver does not select
pre-releases by default. Document this in release notes.

To add manual approval before publish, create a `crates-io` GitHub
Environment and reference it from the `release` job. The token
continues to be a repo secret either way.

## Branch policy on `main`

There is **no enforced branch protection** on `main` (this is a
small one-author repo). Convention:

- Push regular work direct to `main`. CI runs on every push.
- Cut tags only from green commits.
- Force-pushes to `main` are not permitted by convention; recovery
  requires an explicit instruction from the owner.

If protection is added later (PR-required, status-checks gating),
update this section.

## Family of repos

`datawal` is part of a small family of local persistence primitives:

- [`safeatomic`](https://github.com/deepcausa/safeatomic) — Python
  one-shot atomic-write package with an eight-cell guarantee matrix
  and full ADR set. Primary reference for the design philosophy.
- [`safeatomic-rs`](https://github.com/deepcausa/safeatomic-rs) — Rust
  one-shot atomic-write crate (six free functions). `datawal` uses
  it internally for sidecar files. **Not** a binding, **not** a 1:1
  port of `safeatomic`.

Cross-linked from `README.md` § "Related projects", and from the
sibling repos' `AGENTS.md` files.

## Don'ts

- Do not mutate existing corpus fixtures under
  `crates/datawal-core/tests/corpus/`. They lock the wire format.
  Regenerate fresh ones via `cargo run -p datawal --example
  gen_corpus -- /tmp/scratch` into a tempdir for inspection.
- Do not bump `WIRE_VERSION` casually. It is the public contract.
  Any change there is a major-version event and requires updating
  every TLA+ model and corpus fixture.
- Do not introduce in-place segment mutation. Compaction stays
  snapshot-style.
- Do not add multi-writer / multi-process support to `RecordLog`.
  That is a different crate.
- Do not commit `dev/` or any local scratch directory. `.gitignore`
  already excludes it.
- Do not silently drop records during recovery. Always surface
  `RecoveryReport` to the caller.

## Where to ask "is this in scope?"

- For new behaviour or wire-format additions: open a GitHub issue
  with the rationale and the proposed encoding.
- For "I broke the build, how do I unbreak it?": the CI workflow at
  `.github/workflows/ci.yml` is the canonical local reproduction.
  The TLA+ models can be re-checked locally via:

  ```bash
  cd formal
  java -XX:+UseParallelGC -cp /path/to/tla2tools.jar tlc2.TLC \
    -workers 2 -config RecordLog.cfg RecordLog.tla
  ```
