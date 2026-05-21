//! Long-running soak validator for `datawal`.
//!
//! **This is a manual validation tool, NOT a usage example.** It exercises
//! the public `DataWal` API in a long loop, tracks process metrics over
//! time, and verifies final-state consistency after restart. It is not
//! part of CI and is not part of the published artefact's documented
//! behaviour.
//!
//! # Modes
//!
//! Synthetic (default):
//!
//! ```text
//! DATAWAL_SOAK_DURATION=600 \
//!   cargo run --release -p datawal --example soak -- --mode synthetic
//! ```
//!
//! Reads the three JSONL fixtures committed under
//! `crates/datawal-core/tests/fixtures/soak/` and replays them in a
//! weighted round-robin until the duration elapses.
//!
//! Real (operator-supplied data):
//!
//! ```text
//! DATAWAL_SOAK_DURATION=3600 \
//! DATAWAL_SOAK_INPUT_SMALL=/path/to/small.jsonl \
//! DATAWAL_SOAK_INPUT_MEDIUM=/path/to/medium.jsonl \
//! DATAWAL_SOAK_INPUT_LARGE=/path/to/large.jsonl \
//!   cargo run --release -p datawal --example soak -- --mode real
//! ```
//!
//! All three env vars are required in real mode; the soak refuses to
//! start otherwise. The expected JSONL schema is one object per line with
//! `{"key": "<base64>", "payload": "<base64>"}`.
//!
//! # Output
//!
//! - stdout: human-readable progress every minute (synthetic) or every
//!   `DATAWAL_SOAK_PROGRESS_SECS` (default 60).
//! - `${DATAWAL_SOAK_LOG_DIR:-/tmp}/soak.csv`: time-series CSV of RSS,
//!   fd count, segment count, live keys.
//! - exit code: 0 clean, 1 invariant violated, 2 setup error.
//!
//! # Honest naming
//!
//! Soak does NOT prove correctness, does NOT prove production-readiness,
//! and does NOT replace fuzz, proptest, or model checking. It is a smoke
//! test for the long-running case: file descriptors stay bounded, RSS
//! stays bounded, the keydir replayed after restart equals the keydir
//! built in memory during the run.

use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use base64::Engine;
use datawal::DataWal;
use serde::Deserialize;

#[derive(Deserialize)]
struct SoakLine {
    key: String,
    payload: String,
}

#[derive(Clone)]
struct Record {
    key: Vec<u8>,
    payload: Vec<u8>,
}

#[derive(Default)]
struct Counters {
    puts: u64,
    deletes: u64,
    rotates: u64,
    compacts: u64,
    bytes_written: u64,
    fsyncs: u64,
}

fn b64_decode(s: &str) -> Result<Vec<u8>> {
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .context("base64 decode")
}

fn load_jsonl(path: &Path) -> Result<Vec<Record>> {
    let f = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut out = Vec::new();
    for (i, line) in BufReader::new(f).lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let parsed: SoakLine = serde_json::from_str(&line)
            .with_context(|| format!("{}:{} parse JSONL", path.display(), i + 1))?;
        out.push(Record {
            key: b64_decode(&parsed.key)?,
            payload: b64_decode(&parsed.payload)?,
        });
    }
    if out.is_empty() {
        anyhow::bail!("{} is empty", path.display());
    }
    Ok(out)
}

#[cfg(target_os = "linux")]
fn read_rss_kb() -> Option<u64> {
    let s = fs::read_to_string("/proc/self/status").ok()?;
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let v = rest.split_whitespace().next()?;
            return v.parse().ok();
        }
    }
    None
}

#[cfg(not(target_os = "linux"))]
fn read_rss_kb() -> Option<u64> {
    None
}

#[cfg(target_os = "linux")]
fn read_fd_count() -> Option<u64> {
    let entries = fs::read_dir("/proc/self/fd").ok()?;
    Some(entries.count() as u64)
}

#[cfg(not(target_os = "linux"))]
fn read_fd_count() -> Option<u64> {
    None
}

fn count_segments(dir: &Path) -> Result<u64> {
    let mut n: u64 = 0;
    for entry in fs::read_dir(dir).with_context(|| format!("readdir {}", dir.display()))? {
        let entry = entry?;
        let name = entry.file_name();
        let name = match name.to_str() {
            Some(s) => s,
            None => continue,
        };
        if name.len() == 13
            && name.ends_with(".dwal")
            && name[..8].chars().all(|c| c.is_ascii_digit())
        {
            n += 1;
        }
    }
    Ok(n)
}

struct Cli {
    mode: Mode,
    duration: Duration,
    rotate_every: u64,
    compact_every_rotations: u64,
    progress_secs: u64,
    log_dir: PathBuf,
    work_dir: PathBuf,
    real_small: Option<PathBuf>,
    real_medium: Option<PathBuf>,
    real_large: Option<PathBuf>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Synthetic,
    Real,
}

fn parse_args() -> Result<Cli> {
    let mut mode = Mode::Synthetic;
    let args: Vec<String> = env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--mode" => {
                i += 1;
                let v = args.get(i).context("--mode requires value")?;
                mode = match v.as_str() {
                    "synthetic" => Mode::Synthetic,
                    "real" => Mode::Real,
                    other => anyhow::bail!("--mode must be synthetic or real, got `{}`", other),
                };
            }
            other => anyhow::bail!("unknown arg `{}`", other),
        }
        i += 1;
    }
    let duration_s: u64 = env::var("DATAWAL_SOAK_DURATION")
        .ok()
        .map(|s| s.parse())
        .transpose()
        .context("parse DATAWAL_SOAK_DURATION")?
        .unwrap_or(1800);
    let rotate_every: u64 = env::var("DATAWAL_SOAK_ROTATE_EVERY")
        .ok()
        .map(|s| s.parse())
        .transpose()
        .context("parse DATAWAL_SOAK_ROTATE_EVERY")?
        .unwrap_or(5_000);
    let compact_every_rotations: u64 = env::var("DATAWAL_SOAK_COMPACT_EVERY_ROTATIONS")
        .ok()
        .map(|s| s.parse())
        .transpose()
        .context("parse DATAWAL_SOAK_COMPACT_EVERY_ROTATIONS")?
        .unwrap_or(4);
    let progress_secs: u64 = env::var("DATAWAL_SOAK_PROGRESS_SECS")
        .ok()
        .map(|s| s.parse())
        .transpose()
        .context("parse DATAWAL_SOAK_PROGRESS_SECS")?
        .unwrap_or(60);
    let log_dir: PathBuf = env::var("DATAWAL_SOAK_LOG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"));
    let work_dir: PathBuf = env::var("DATAWAL_SOAK_WORK_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| env::temp_dir().join("datawal-soak"));
    let (small, medium, large) = match mode {
        Mode::Synthetic => (None, None, None),
        Mode::Real => {
            let s = env::var("DATAWAL_SOAK_INPUT_SMALL")
                .context("DATAWAL_SOAK_INPUT_SMALL required in --mode real")?;
            let m = env::var("DATAWAL_SOAK_INPUT_MEDIUM")
                .context("DATAWAL_SOAK_INPUT_MEDIUM required in --mode real")?;
            let l = env::var("DATAWAL_SOAK_INPUT_LARGE")
                .context("DATAWAL_SOAK_INPUT_LARGE required in --mode real")?;
            (
                Some(PathBuf::from(s)),
                Some(PathBuf::from(m)),
                Some(PathBuf::from(l)),
            )
        }
    };
    Ok(Cli {
        mode,
        duration: Duration::from_secs(duration_s),
        rotate_every,
        compact_every_rotations,
        progress_secs,
        log_dir,
        work_dir,
        real_small: small,
        real_medium: medium,
        real_large: large,
    })
}

fn fixture_path(name: &str) -> PathBuf {
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    crate_dir
        .join("tests")
        .join("fixtures")
        .join("soak")
        .join(name)
}

fn load_streams(cli: &Cli) -> Result<(Vec<Record>, Vec<Record>, Vec<Record>)> {
    match cli.mode {
        Mode::Synthetic => {
            let small = load_jsonl(&fixture_path("small_records.jsonl"))?;
            let medium = load_jsonl(&fixture_path("medium_records.jsonl"))?;
            let large = load_jsonl(&fixture_path("large_payloads.jsonl"))?;
            Ok((small, medium, large))
        }
        Mode::Real => {
            let small = load_jsonl(cli.real_small.as_ref().unwrap())?;
            let medium = load_jsonl(cli.real_medium.as_ref().unwrap())?;
            let large = load_jsonl(cli.real_large.as_ref().unwrap())?;
            Ok((small, medium, large))
        }
    }
}

fn pick_stream<'a>(
    op_idx: u64,
    small: &'a [Record],
    medium: &'a [Record],
    large: &'a [Record],
) -> &'a Record {
    // Weight 70 / 25 / 5 small / medium / large via the low byte mod 100.
    let bucket = op_idx.wrapping_mul(2654435761) % 100;
    let stream: &[Record] = if bucket < 70 {
        small
    } else if bucket < 95 {
        medium
    } else {
        large
    };
    let i = (op_idx as usize) % stream.len();
    &stream[i]
}

fn main() -> ExitCode {
    let cli = match parse_args() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("soak: setup error: {:#}", e);
            return ExitCode::from(2);
        }
    };
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("soak: failed: {:#}", e);
            ExitCode::from(1)
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    // Wipe + create the work dir cleanly.
    let _ = fs::remove_dir_all(&cli.work_dir);
    fs::create_dir_all(&cli.work_dir)
        .with_context(|| format!("create work dir {}", cli.work_dir.display()))?;
    fs::create_dir_all(&cli.log_dir)
        .with_context(|| format!("create log dir {}", cli.log_dir.display()))?;

    let csv_path = cli.log_dir.join("soak.csv");
    let mut csv =
        fs::File::create(&csv_path).with_context(|| format!("create {}", csv_path.display()))?;
    writeln!(
        csv,
        "elapsed_s,rss_kb,fds,segments,live_keys,puts,deletes,rotates,compacts,bytes_written"
    )?;

    let (small, medium, large) = load_streams(&cli)?;
    println!(
        "soak: mode={} duration={}s work_dir={} log={}",
        match cli.mode {
            Mode::Synthetic => "synthetic",
            Mode::Real => "real",
        },
        cli.duration.as_secs(),
        cli.work_dir.display(),
        csv_path.display(),
    );
    println!(
        "soak: streams loaded small={} medium={} large={}",
        small.len(),
        medium.len(),
        large.len()
    );

    let mut log = DataWal::open(&cli.work_dir).context("DataWal::open work_dir")?;
    let mut counters = Counters::default();
    let mut expected: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();
    let mut op_idx: u64 = 0;
    let mut rotations_since_compact: u64 = 0;

    let start = Instant::now();
    let mut last_progress = start;

    while start.elapsed() < cli.duration {
        let rec = pick_stream(op_idx, &small, &medium, &large);
        // Mostly puts; ~5% deletes against an existing key.
        let do_delete = (op_idx % 20 == 19) && !expected.is_empty();
        if do_delete {
            // Pick a deterministic existing key.
            let target_idx = (op_idx as usize) % expected.len();
            let target_key = expected.keys().nth(target_idx).cloned().unwrap();
            log.delete(&target_key).context("delete")?;
            expected.remove(&target_key);
            counters.deletes += 1;
            counters.bytes_written += target_key.len() as u64;
        } else {
            log.put(&rec.key, &rec.payload).context("put")?;
            expected.insert(rec.key.clone(), rec.payload.clone());
            counters.puts += 1;
            counters.bytes_written += (rec.key.len() + rec.payload.len()) as u64;
        }

        // Fsync periodically (every 1000 ops) so the durability path is exercised.
        if op_idx % 1000 == 999 {
            log.fsync().context("fsync")?;
            counters.fsyncs += 1;
        }

        // Compact every N ops. Compaction here implicitly serves the
        // "segment rotation" role because the swap-and-reopen rebuilds the
        // log from scratch into a single fresh segment. A finer-grained
        // rotation cadence would require `RecordLog::rotate` directly,
        // which lives one layer below `DataWal` and is exercised by the
        // proptest suite already (#21).
        if op_idx % cli.rotate_every == cli.rotate_every - 1 {
            counters.rotates += 1;
            rotations_since_compact += 1;
            if rotations_since_compact >= cli.compact_every_rotations {
                let target = cli.work_dir.with_extension("compact-tmp");
                let _ = fs::remove_dir_all(&target);
                log.compact_to(&target).context("compact_to")?;
                drop(log);
                fs::remove_dir_all(&cli.work_dir).context("remove old work_dir")?;
                fs::rename(&target, &cli.work_dir).context("rename compact target")?;
                log = DataWal::open(&cli.work_dir).context("reopen after compact")?;
                counters.compacts += 1;
                rotations_since_compact = 0;
            }
        }

        op_idx = op_idx.wrapping_add(1);

        // Progress + CSV row.
        if last_progress.elapsed() >= Duration::from_secs(cli.progress_secs) {
            let elapsed = start.elapsed().as_secs();
            let rss = read_rss_kb().unwrap_or(0);
            let fds = read_fd_count().unwrap_or(0);
            let segs = count_segments(&cli.work_dir).unwrap_or(0);
            let live = expected.len() as u64;
            writeln!(
                csv,
                "{},{},{},{},{},{},{},{},{},{}",
                elapsed,
                rss,
                fds,
                segs,
                live,
                counters.puts,
                counters.deletes,
                counters.rotates,
                counters.compacts,
                counters.bytes_written
            )?;
            csv.flush()?;
            println!(
                "soak: t={:>5}s rss={}kb fds={} segs={} live={} puts={} dels={} rot={} cmp={}",
                elapsed,
                rss,
                fds,
                segs,
                live,
                counters.puts,
                counters.deletes,
                counters.rotates,
                counters.compacts
            );
            last_progress = Instant::now();
        }
    }

    log.fsync().context("final fsync")?;
    drop(log);

    // Final-state consistency: reopen and compare keydir to `expected`.
    let reopen = DataWal::open(&cli.work_dir).context("final reopen")?;
    let observed_keys: std::collections::HashSet<Vec<u8>> = reopen.keys().into_iter().collect();
    let expected_keys: std::collections::HashSet<Vec<u8>> = expected.keys().cloned().collect();

    if observed_keys != expected_keys {
        let only_obs: Vec<_> = observed_keys.difference(&expected_keys).take(3).collect();
        let only_exp: Vec<_> = expected_keys.difference(&observed_keys).take(3).collect();
        anyhow::bail!(
            "keydir mismatch after reopen: observed={} expected={} only_in_observed_sample={} only_in_expected_sample={}",
            observed_keys.len(),
            expected_keys.len(),
            only_obs.len(),
            only_exp.len()
        );
    }
    for k in &expected_keys {
        let got = reopen.get(k).context("get for consistency check")?;
        let want = expected.get(k);
        if got.as_deref() != want.map(|v| v.as_slice()) {
            anyhow::bail!(
                "value mismatch for key of len {}: got len {:?}, want len {:?}",
                k.len(),
                got.as_ref().map(|v| v.len()),
                want.map(|v| v.len())
            );
        }
    }

    let final_segs = count_segments(&cli.work_dir).unwrap_or(0);
    println!(
        "soak: clean exit; final_keys={} final_segments={} puts={} deletes={} rotates={} compacts={} bytes_written={}",
        expected_keys.len(),
        final_segs,
        counters.puts,
        counters.deletes,
        counters.rotates,
        counters.compacts,
        counters.bytes_written
    );
    Ok(())
}
