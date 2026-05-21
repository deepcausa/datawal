//! Power-loss simulation **workload generator** for `datawal`.
//!
//! **This is a manual validation tool, NOT a usage example.** It is the
//! producer half of the dm-flakey power-loss harness; the consumer half
//! is the sibling `power_loss_validate` example, and the two are wired
//! together by `scripts/power_loss_dm_flakey.sh`. None of this is part
//! of CI and none of it is part of the published artefact's documented
//! behaviour.
//!
//! # What the workload does
//!
//! Open (or create) a `DataWal` under `--work-dir`, then run a deterministic
//! loop of `put` / `delete` operations seeded by `--seed`. For every
//! operation whose effect we want to claim is durable, we:
//!
//! 1. Call `DataWal::put` / `DataWal::delete`.
//! 2. Call `DataWal::fsync`.
//! 3. **Only after `fsync` returns Ok**, append a single JSONL line to
//!    `--oracle` describing the operation and flush the oracle file
//!    (`write_all` + `flush` + `sync_data`).
//!
//! The oracle is the contract: every line in `--oracle` is an operation
//! the workload claims is durable. The validator reopens the store after
//! a fault injection and asserts that the on-disk state honours every
//! oracle line.
//!
//! The oracle file lives on a *separate filesystem* from the WAL under
//! test (the orchestrator script pins it to `/tmp` while the WAL is on
//! the dm-flakey-backed mount). This is deliberate: the dm-flakey device
//! drops writes to the WAL filesystem, not to the oracle path.
//!
//! # CLI
//!
//! ```text
//! cargo run --release -p datawal --example power_loss_workload -- \
//!   --work-dir /mnt/datawal-test/wal \
//!   --oracle   /tmp/datawal-powerloss-<id>/oracle.jsonl \
//!   --seed     42 \
//!   --ops      100000 \
//!   --duration 0
//! ```
//!
//! `--duration` is wall-clock seconds; `--ops` is an operation budget.
//! The workload stops as soon as **either** limit is reached. `--ops 0`
//! disables the op budget; `--duration 0` disables the wall-clock budget.
//! At least one must be non-zero.
//!
//! # Oracle line format
//!
//! Each oracle line is a JSON object on its own line, base64-encoding any
//! bytes:
//!
//! ```json
//! {"op":"put","seq":1,"key":"<base64>","payload":"<base64>"}
//! {"op":"del","seq":2,"key":"<base64>"}
//! ```
//!
//! `seq` is the op index of the **next** op that will be attempted; lines
//! are written in `seq` order. The validator treats the maximum recorded
//! `seq` as "the highest op the workload claims is durable".
//!
//! # Exit codes
//!
//! - `0` clean exit (budget reached, oracle flushed).
//! - `1` workload error (anything `DataWal` rejected before fsync).
//! - `2` setup error (missing arg, unwritable path, etc.).
//!
//! Exit codes mirror `soak.rs` so the orchestrator can treat the two
//! examples uniformly.
//!
//! # Honest claims
//!
//! - The workload is single-threaded and single-writer.
//! - The oracle is fsync-ordered: a line at `seq=N` means the workload
//!   **observed** `DataWal::fsync` return `Ok(())` for the op at `seq=N`.
//!   It does **not** mean the underlying block device fsync'd
//!   successfully; under dm-flakey that is exactly the property we are
//!   probing.
//! - This example does not inject faults. Fault injection is the
//!   orchestrator script's job (dm-flakey table reload).

use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use base64::Engine;
use datawal::DataWal;
use serde::Serialize;

const DEFAULT_KEY_LEN: usize = 16;
const DEFAULT_VALUE_LEN: usize = 96;
const DELETE_EVERY: u64 = 23;
const FSYNC_BATCH: u64 = 1;

struct Cli {
    work_dir: PathBuf,
    oracle: PathBuf,
    seed: u64,
    ops: u64,
    duration: Duration,
    key_len: usize,
    value_len: usize,
}

fn parse_args() -> Result<Cli> {
    let mut work_dir: Option<PathBuf> = None;
    let mut oracle: Option<PathBuf> = None;
    let mut seed: u64 = 1;
    let mut ops: u64 = 0;
    let mut duration_s: u64 = 0;
    let mut key_len: usize = DEFAULT_KEY_LEN;
    let mut value_len: usize = DEFAULT_VALUE_LEN;

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
            "--seed" => {
                seed = next()?.parse().context("parse --seed")?;
                i += 2;
            }
            "--ops" => {
                ops = next()?.parse().context("parse --ops")?;
                i += 2;
            }
            "--duration" => {
                duration_s = next()?.parse().context("parse --duration")?;
                i += 2;
            }
            "--key-len" => {
                key_len = next()?.parse().context("parse --key-len")?;
                i += 2;
            }
            "--value-len" => {
                value_len = next()?.parse().context("parse --value-len")?;
                i += 2;
            }
            other => anyhow::bail!("unknown arg `{}`", other),
        }
    }

    let work_dir = work_dir.context("--work-dir is required")?;
    let oracle = oracle.context("--oracle is required")?;
    if ops == 0 && duration_s == 0 {
        anyhow::bail!("at least one of --ops or --duration must be non-zero");
    }
    if !(1..=512).contains(&key_len) {
        anyhow::bail!("--key-len must be in 1..=512 (got {})", key_len);
    }
    if !(1..=65_536).contains(&value_len) {
        anyhow::bail!("--value-len must be in 1..=65536 (got {})", value_len);
    }

    Ok(Cli {
        work_dir,
        oracle,
        seed,
        ops,
        duration: Duration::from_secs(duration_s),
        key_len,
        value_len,
    })
}

/// Deterministic PRNG so a `(seed, op_idx)` pair always produces the
/// same key/payload bytes. We use `splitmix64` for the state advance
/// and slice the resulting bytes to fill key/value buffers; this is
/// not cryptographic and does not need to be.
#[derive(Clone, Copy)]
struct SplitMix64(u64);

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_add(0x9E3779B97F4A7C15))
    }
    fn next_u64(&mut self) -> u64 {
        let mut z = self.0.wrapping_add(0x9E3779B97F4A7C15);
        self.0 = z;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }
    fn fill(&mut self, out: &mut [u8]) {
        let mut i = 0;
        while i < out.len() {
            let chunk = self.next_u64().to_le_bytes();
            let take = (out.len() - i).min(8);
            out[i..i + take].copy_from_slice(&chunk[..take]);
            i += take;
        }
    }
}

fn key_for(seed: u64, op: u64, key_len: usize) -> Vec<u8> {
    // Universe of ~4096 distinct keys per seed so deletes hit existing
    // keys reasonably often without growing the keydir without bound.
    let key_id = op.wrapping_mul(0x100000001B3) & 0xFFF;
    let mut rng = SplitMix64::new(seed ^ key_id);
    let mut out = vec![0u8; key_len];
    rng.fill(&mut out);
    out
}

fn value_for(seed: u64, op: u64, value_len: usize) -> Vec<u8> {
    let mut rng = SplitMix64::new(seed ^ op ^ 0xDEADBEEFCAFEBABE);
    let mut out = vec![0u8; value_len];
    rng.fill(&mut out);
    out
}

#[derive(Serialize)]
#[serde(tag = "op")]
enum OracleLine<'a> {
    #[serde(rename = "put")]
    Put {
        seq: u64,
        key: &'a str,
        payload: &'a str,
    },
    #[serde(rename = "del")]
    Del { seq: u64, key: &'a str },
}

fn b64(b: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(b)
}

fn write_oracle_line(file: &mut fs::File, line: &OracleLine<'_>) -> Result<()> {
    let mut buf = serde_json::to_vec(line).context("encode oracle line")?;
    buf.push(b'\n');
    file.write_all(&buf).context("write oracle line")?;
    file.flush().context("flush oracle line")?;
    // sync_data on the oracle file — this is on a *separate* filesystem
    // from the WAL under test (orchestrator places it on /tmp); we want
    // the line itself durable so the validator can trust it.
    file.sync_data().context("fdatasync oracle file")?;
    Ok(())
}

fn ensure_parent_dir(p: &Path) -> Result<()> {
    if let Some(parent) = p.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create parent of {}", p.display()))?;
        }
    }
    Ok(())
}

fn main() -> ExitCode {
    let cli = match parse_args() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("power_loss_workload: setup error: {:#}", e);
            return ExitCode::from(2);
        }
    };
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("power_loss_workload: failed: {:#}", e);
            ExitCode::from(1)
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    fs::create_dir_all(&cli.work_dir)
        .with_context(|| format!("create work dir {}", cli.work_dir.display()))?;
    ensure_parent_dir(&cli.oracle)?;

    // Truncate the oracle on each run. The workload is the sole writer.
    let mut oracle = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&cli.oracle)
        .with_context(|| format!("open oracle {}", cli.oracle.display()))?;

    eprintln!(
        "power_loss_workload: work_dir={} oracle={} seed={} ops={} duration={}s key_len={} value_len={}",
        cli.work_dir.display(),
        cli.oracle.display(),
        cli.seed,
        cli.ops,
        cli.duration.as_secs(),
        cli.key_len,
        cli.value_len,
    );

    let mut log = DataWal::open(&cli.work_dir).context("DataWal::open work_dir")?;
    let start = Instant::now();
    let ops_budget = if cli.ops == 0 { u64::MAX } else { cli.ops };
    let duration_budget = cli.duration;

    let mut seq: u64 = 0;
    let mut puts: u64 = 0;
    let mut dels: u64 = 0;

    loop {
        if seq >= ops_budget {
            break;
        }
        if !duration_budget.is_zero() && start.elapsed() >= duration_budget {
            break;
        }

        let do_delete = seq % DELETE_EVERY == DELETE_EVERY - 1;
        let key = key_for(cli.seed, seq, cli.key_len);
        if do_delete {
            log.delete(&key).context("DataWal::delete")?;
            // We sync every op to make oracle truthful.
            if seq % FSYNC_BATCH == 0 {
                log.fsync().context("DataWal::fsync (delete)")?;
                let key_b64 = b64(&key);
                write_oracle_line(
                    &mut oracle,
                    &OracleLine::Del {
                        seq,
                        key: &key_b64,
                    },
                )?;
                dels += 1;
            }
        } else {
            let value = value_for(cli.seed, seq, cli.value_len);
            log.put(&key, &value).context("DataWal::put")?;
            if seq % FSYNC_BATCH == 0 {
                log.fsync().context("DataWal::fsync (put)")?;
                let key_b64 = b64(&key);
                let payload_b64 = b64(&value);
                write_oracle_line(
                    &mut oracle,
                    &OracleLine::Put {
                        seq,
                        key: &key_b64,
                        payload: &payload_b64,
                    },
                )?;
                puts += 1;
            }
        }

        seq = seq.wrapping_add(1);
    }

    // Final fsync on the WAL and the oracle. The orchestrator may still
    // pull the rug afterwards, but the workload's claim ends here.
    log.fsync().context("final fsync (wal)")?;
    oracle.sync_data().context("final fdatasync (oracle)")?;

    eprintln!(
        "power_loss_workload: clean exit ops={} puts={} dels={} elapsed={}s",
        seq,
        puts,
        dels,
        start.elapsed().as_secs()
    );
    Ok(())
}
