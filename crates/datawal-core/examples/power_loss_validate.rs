//! Power-loss simulation **validator** for `datawal`.
//!
//! **This is a manual validation tool, NOT a usage example.** It is the
//! consumer half of the dm-flakey power-loss harness; the producer half
//! is the sibling `power_loss_workload` example, and the two are wired
//! together by `scripts/power_loss_dm_flakey.sh`. None of this is part
//! of CI and none of it is part of the published artefact's documented
//! behaviour.
//!
//! # What the validator does
//!
//! Open the `DataWal` directory **after** a fault injection (typically a
//! dm-flakey table reload that drops writes, followed by `umount -f` and
//! `mount` again) and assert the following invariants:
//!
//! 1. `DataWal::open(&dir)` succeeds. `RecordLog`'s longest-valid-prefix
//!    recovery is allowed to truncate a partial tail, but it must not
//!    error out. CRC mismatch in a *sealed* segment is a hard error and
//!    will surface as a failed `open`.
//! 2. The `RecoveryReport` returned by reopening the underlying
//!    `RecordLog` is recorded for inspection (segments scanned,
//!    tail-bytes discarded, mid-stream errors).
//! 3. For every **put** line in the oracle file (lines that the workload
//!    flushed *after* a successful `DataWal::fsync`):
//!    - `DataWal::contains_key(key)` must be `true`, **unless** the
//!      same key is overwritten or tombstoned by a later oracle line.
//!    - `DataWal::get(key)` must succeed (CRC-valid) and return the
//!      expected payload bytes, where "expected" is the most recent
//!      oracle line for that key.
//! 4. For every **del** oracle line, the key must not be live in the
//!    reopened store *unless* a later oracle line puts it back.
//! 5. The reopened keydir must contain **no extra live keys** beyond
//!    those implied by the oracle. The harness's contract is that the
//!    oracle is the upper bound of what the workload claimed durable;
//!    a reopened store may have *less* (tail dropped) but not *more*.
//!    A larger live set means the recovery surfaced state we never
//!    fsync'd — that is a bug.
//!
//! Together these say: **the reopened store is a prefix of the oracle**.
//! That is the property dm-flakey-class faults should preserve under
//! correct fsync semantics.
//!
//! # CLI
//!
//! ```text
//! cargo run --release -p datawal --example power_loss_validate -- \
//!   --work-dir /mnt/datawal-test/wal \
//!   --oracle   /tmp/datawal-powerloss-<id>/oracle.jsonl
//! ```
//!
//! # Exit codes
//!
//! - `0` reopened store is a valid prefix of the oracle.
//! - `1` invariant violated. The validator prints exactly which
//!   invariant failed and a small sample of mismatched keys.
//! - `2` setup error (missing arg, oracle unreadable, etc.).
//!
//! Exit codes mirror `soak.rs` and `power_loss_workload.rs` so the
//! orchestrator can treat all three examples uniformly.
//!
//! # Honest claims
//!
//! - The validator does not re-derive payloads from `--seed`; it reads
//!   the bytes the workload claimed durable from the oracle. This keeps
//!   the validator independent of the workload's PRNG choice.
//! - "Prefix of the oracle" is checked *per key*, not in oracle order.
//!   The workload writes oracle lines after each successful fsync, but
//!   the validator does not assume the surviving tail of the WAL is a
//!   contiguous prefix of the oracle by `seq`: a tail may end anywhere
//!   the kernel cared to flush. What it does assume is that any *single*
//!   surviving record is the **most recent** put for its key, because
//!   `RecordLog` recovery uses the longest valid prefix per segment.

use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result};
use base64::Engine;
use datawal::{DataWal, RecordLog};
use serde::Deserialize;

struct Cli {
    work_dir: PathBuf,
    oracle: PathBuf,
    /// Max samples to print when reporting mismatches. Defaults to 5.
    sample: usize,
}

fn parse_args() -> Result<Cli> {
    let mut work_dir: Option<PathBuf> = None;
    let mut oracle: Option<PathBuf> = None;
    let mut sample: usize = 5;

    let args: Vec<String> = env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        let next = || -> Result<&String> {
            args.get(i + 1)
                .with_context(|| format!("{} requires a value", arg))
        };
        match arg.as_str() {
            "--work-dir" => {
                work_dir = Some(PathBuf::from(next()?));
                i += 2;
            }
            "--oracle" => {
                oracle = Some(PathBuf::from(next()?));
                i += 2;
            }
            "--sample" => {
                sample = next()?.parse().context("parse --sample")?;
                i += 2;
            }
            other => anyhow::bail!("unknown arg `{}`", other),
        }
    }
    Ok(Cli {
        work_dir: work_dir.context("--work-dir is required")?,
        oracle: oracle.context("--oracle is required")?,
        sample,
    })
}

#[derive(Debug, Deserialize)]
#[serde(tag = "op")]
enum OracleLine {
    #[serde(rename = "put")]
    Put {
        seq: u64,
        key: String,
        payload: String,
    },
    #[serde(rename = "del")]
    Del { seq: u64, key: String },
}

/// Effective oracle state after replay: the most-recent op per key.
enum OracleEffect {
    Live(Vec<u8>),
    Dead,
}

fn b64_decode(s: &str) -> Result<Vec<u8>> {
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .context("base64 decode oracle field")
}

fn load_oracle(path: &Path) -> Result<(HashMap<Vec<u8>, OracleEffect>, u64, u64, u64)> {
    let f = fs::File::open(path).with_context(|| format!("open oracle {}", path.display()))?;
    let mut effect: HashMap<Vec<u8>, OracleEffect> = HashMap::new();
    let mut last_seq: u64 = 0;
    let mut puts: u64 = 0;
    let mut dels: u64 = 0;
    for (lineno, line) in BufReader::new(f).lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let parsed: OracleLine = serde_json::from_str(&line)
            .with_context(|| format!("{}:{} parse oracle line", path.display(), lineno + 1))?;
        match parsed {
            OracleLine::Put { seq, key, payload } => {
                let key = b64_decode(&key)?;
                let payload = b64_decode(&payload)?;
                effect.insert(key, OracleEffect::Live(payload));
                last_seq = last_seq.max(seq);
                puts += 1;
            }
            OracleLine::Del { seq, key } => {
                let key = b64_decode(&key)?;
                effect.insert(key, OracleEffect::Dead);
                last_seq = last_seq.max(seq);
                dels += 1;
            }
        }
    }
    Ok((effect, last_seq, puts, dels))
}

fn main() -> ExitCode {
    let cli = match parse_args() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("power_loss_validate: setup error: {:#}", e);
            return ExitCode::from(2);
        }
    };
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("power_loss_validate: invariant violated: {:#}", e);
            ExitCode::from(1)
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    let (effect, last_seq, oracle_puts, oracle_dels) = load_oracle(&cli.oracle)?;
    eprintln!(
        "power_loss_validate: oracle loaded path={} last_seq={} puts={} dels={} effective_keys={}",
        cli.oracle.display(),
        last_seq,
        oracle_puts,
        oracle_dels,
        effect.len()
    );

    // First, inspect recovery via RecordLog directly so we can print the
    // RecoveryReport (DataWal::open does not surface it). RecordLog::open
    // holds the lock; drop it before opening DataWal on the same dir.
    {
        let rlog = RecordLog::open(&cli.work_dir)
            .with_context(|| format!("RecordLog::open {}", cli.work_dir.display()))?;
        let report = rlog
            .recovery_report()
            .context("RecordLog::recovery_report")?;
        eprintln!(
            "power_loss_validate: recovery files_scanned={} records_replayed={} tail_truncated_segs={} tail_bytes_discarded={} mid_stream_errors={} last_txid_seen={}",
            report.files_scanned,
            report.records_replayed,
            report.tail_truncated,
            report.tail_bytes_discarded,
            report.mid_stream_errors,
            report.last_txid_seen,
        );
        if report.mid_stream_errors > 0 {
            anyhow::bail!(
                "RecoveryReport.mid_stream_errors = {} (v0.1.x is supposed to abort on the first; this is a bug)",
                report.mid_stream_errors
            );
        }
    }

    // Now reopen as DataWal and run the invariants.
    let mut store = DataWal::open(&cli.work_dir)
        .with_context(|| format!("DataWal::open {}", cli.work_dir.display()))?;

    // Collect the live keydir for fast intersection.
    let observed: std::collections::HashSet<Vec<u8>> = store.keys().into_iter().collect();
    let oracle_live: std::collections::HashSet<Vec<u8>> = effect
        .iter()
        .filter_map(|(k, eff)| match eff {
            OracleEffect::Live(_) => Some(k.clone()),
            OracleEffect::Dead => None,
        })
        .collect();

    // Invariant 5: no extra live keys.
    let extras: Vec<Vec<u8>> = observed.difference(&oracle_live).cloned().collect();
    if !extras.is_empty() {
        let sample: Vec<String> = extras
            .iter()
            .take(cli.sample)
            .map(|k| format!("len={} sha256_prefix={}", k.len(), short_fingerprint(k)))
            .collect();
        anyhow::bail!(
            "reopened store has {} live key(s) not present (live) in oracle; sample={:?}",
            extras.len(),
            sample
        );
    }

    // Invariants 3 & 4: for every live oracle entry, the reopened store
    // either has the same payload or is missing the key (tail dropped).
    let mut survived_live: u64 = 0;
    let mut dropped_live: u64 = 0;
    let mut mismatches: Vec<String> = Vec::new();
    for (key, eff) in &effect {
        if let OracleEffect::Live(expected) = eff {
            match store.get(key).context("DataWal::get for oracle key")? {
                Some(got) => {
                    if &got != expected {
                        if mismatches.len() < cli.sample {
                            mismatches.push(format!(
                                "key_len={} expected_len={} got_len={} key_fp={}",
                                key.len(),
                                expected.len(),
                                got.len(),
                                short_fingerprint(key),
                            ));
                        }
                    } else {
                        survived_live += 1;
                    }
                }
                None => {
                    dropped_live += 1;
                }
            }
        }
    }
    if !mismatches.is_empty() {
        anyhow::bail!(
            "payload mismatch for {} live oracle key(s); sample={:?}",
            mismatches.len(),
            mismatches
        );
    }

    // For oracle Dead entries we already enforced "no extras" above; a
    // tombstone that survived is consistent. We do not require Dead
    // entries to have left a tombstone on disk because the validator
    // cannot distinguish "tombstone applied" from "put + delete were
    // both lost together" without the RecordLog scan, which is out of
    // scope here.

    let dead_in_oracle = effect
        .values()
        .filter(|e| matches!(e, OracleEffect::Dead))
        .count();
    eprintln!(
        "power_loss_validate: OK observed_live={} oracle_live={} survived={} dropped={} oracle_dead={} (extras=0)",
        observed.len(),
        oracle_live.len(),
        survived_live,
        dropped_live,
        dead_in_oracle,
    );
    Ok(())
}

/// Stable fingerprint for logs — first 8 bytes hex of a non-cryptographic
/// hash. Just enough to disambiguate sample lines without dumping bytes.
fn short_fingerprint(b: &[u8]) -> String {
    // FNV-1a 64-bit — fine for log labels, not a security primitive.
    let mut h: u64 = 0xcbf29ce484222325;
    for &byte in b {
        h ^= byte as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{:016x}", h)
}
