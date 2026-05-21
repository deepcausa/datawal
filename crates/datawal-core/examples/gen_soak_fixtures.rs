//! Regenerate the synthetic JSONL fixtures used by the `soak` example.
//!
//! Run from the workspace root:
//!
//! ```text
//! cargo run -p datawal --example gen_soak_fixtures
//! cargo run -p datawal --example gen_soak_fixtures -- /tmp/soak-fixtures-check
//! ```
//!
//! Without arguments, fixtures are written to the in-tree location:
//! `crates/datawal-core/tests/fixtures/soak/`. With a single positional
//! argument, fixtures are written there instead (useful for spot-checking).
//!
//! The fixtures are committed to the repository and consumed by the `soak`
//! example in synthetic mode. The generator is deterministic (fixed seed),
//! so regenerating produces byte-identical files until the schema changes.
//!
//! This generator is **not** part of `cargo test` and **not** part of CI.
//! It exists so a maintainer can regenerate the fixtures if the schema
//! evolves. It does NOT touch the wire-format corpus under `tests/corpus/`.

use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use base64::Engine;
use serde::Serialize;

/// JSONL line schema for soak fixtures.
///
/// Both fields are base64-encoded byte strings, so binary payloads round-trip
/// cleanly and the file is still text-grep-able.
#[derive(Serialize)]
struct SoakLine<'a> {
    key: &'a str,
    payload: &'a str,
}

/// Deterministic 64-bit splitmix-style PRNG. Sufficient for generating
/// pseudo-random fixtures; not used for any cryptographic purpose.
struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn fill(&mut self, buf: &mut [u8]) {
        for chunk in buf.chunks_mut(8) {
            let n = self.next_u64().to_le_bytes();
            for (dst, src) in chunk.iter_mut().zip(n.iter()) {
                *dst = *src;
            }
        }
    }
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn write_jsonl(path: &Path, lines: &[(Vec<u8>, Vec<u8>)]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("mkdir -p {}", parent.display()))?;
    }
    let mut f = fs::File::create(path).with_context(|| format!("create {}", path.display()))?;
    for (key, payload) in lines {
        let line = SoakLine {
            key: &b64(key),
            payload: &b64(payload),
        };
        let s = serde_json::to_string(&line)?;
        f.write_all(s.as_bytes())?;
        f.write_all(b"\n")?;
    }
    f.sync_all()?;
    Ok(())
}

/// Generate `count` records with payload of `payload_size` bytes and key of
/// `key_size` bytes, using `rng`.
fn gen_records(
    rng: &mut SplitMix64,
    count: usize,
    key_size: usize,
    payload_size: usize,
) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        let mut k = vec![0u8; key_size];
        rng.fill(&mut k);
        let mut p = vec![0u8; payload_size];
        rng.fill(&mut p);
        out.push((k, p));
    }
    out
}

fn fixtures_root_default() -> PathBuf {
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    crate_dir.join("tests").join("fixtures").join("soak")
}

fn resolve_root() -> Result<PathBuf> {
    let mut args = env::args().skip(1);
    match args.next() {
        Some(arg) => {
            if args.next().is_some() {
                anyhow::bail!(
                    "gen_soak_fixtures accepts at most one positional argument (output dir)"
                );
            }
            let p = PathBuf::from(arg);
            let abs = if p.is_absolute() {
                p
            } else {
                env::current_dir()?.join(p)
            };
            Ok(abs)
        }
        None => Ok(fixtures_root_default()),
    }
}

fn main() -> Result<()> {
    let root = resolve_root()?;
    println!(
        "gen_soak_fixtures: writing synthetic JSONL fixtures to {}",
        root.display()
    );

    // Three streams with independent deterministic seeds. Splitting the seed
    // per stream keeps each file stable even if another stream's counts
    // change later.
    let mut rng_small = SplitMix64::new(0x5050_5050_0000_0001);
    let mut rng_medium = SplitMix64::new(0x5050_5050_0000_0002);
    let mut rng_large = SplitMix64::new(0x5050_5050_0000_0003);

    // Small: 100 lines, 32-byte key, 512-byte payload.
    let small = gen_records(&mut rng_small, 100, 32, 512);
    write_jsonl(&root.join("small_records.jsonl"), &small)?;

    // Medium: 100 lines, 32-byte key, ~3 KB payload.
    let medium = gen_records(&mut rng_medium, 100, 32, 3_072);
    write_jsonl(&root.join("medium_records.jsonl"), &medium)?;

    // Large: 20 lines, 32-byte key, 64 KB payload.
    let large = gen_records(&mut rng_large, 20, 32, 65_536);
    write_jsonl(&root.join("large_payloads.jsonl"), &large)?;

    println!(
        "  small_records.jsonl   {:>5} lines, payload {:>6} B",
        small.len(),
        512
    );
    println!(
        "  medium_records.jsonl  {:>5} lines, payload {:>6} B",
        medium.len(),
        3_072
    );
    println!(
        "  large_payloads.jsonl  {:>5} lines, payload {:>6} B",
        large.len(),
        65_536
    );
    println!("done.");
    Ok(())
}
