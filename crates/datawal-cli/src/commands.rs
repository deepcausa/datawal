//! Subcommand dispatch and implementations.
//!
//! Subcommands fall into two groups:
//!
//! - **Inspection** (`scan`, `get`, `report`, `verify`, `dump`,
//!   `check`): never write to the source store.
//! - **Source-untouched mutations** (`export`, `compact`): write a new
//!   artefact at a caller-supplied output path; the source store on
//!   disk is never modified.
//!
//! All subcommands acquire `DataWal::open` / `RecordLog::open` and
//! therefore go through the cooperative single-writer lock. This is
//! by design — bypassing the lock would risk observing a
//! partially-written tail in flight. If the store is already opened
//! by a running writer, the CLI fails with exit code 1.

use std::process::ExitCode;

use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use datawal::{DataWal, Record, RecordLog};

use crate::bytes_render::{
    render_for_human, render_value_for_get, BytesMode, DEFAULT_TRUNCATE_BYTES,
};
use crate::cli::{Cli, Command, CompactArgs, DumpArgs, ExportArgs, GetArgs, ScanArgs, StoreArg};
use crate::output::{
    CheckObj, CompactObj, ExportObj, FrameLine, RecordLine, ReportObj, ValueHit, ValueMiss,
    VerifyObj, SCHEMA,
};

/// Exit codes:
/// - 0 success
/// - 1 user / configuration / store-locked / output-path error
///   (handled in `main`; also returned in-band when `export` /
///   `compact` would clobber an existing artefact)
/// - 2 recoverable storage state observed (truncated tail, missing
///   key on `get`)
/// - 3 hard storage error (CRC failure in a sealed segment, or a
///   per-record `get` failure during `check`)
pub fn dispatch(cli: Cli) -> Result<ExitCode> {
    let json = cli.json;
    match cli.command {
        Command::Scan(args) => cmd_scan(args, json),
        Command::Get(args) => cmd_get(args, json),
        Command::Report(args) => cmd_report(args, json),
        Command::Verify(args) => cmd_verify(args, json),
        Command::Dump(args) => cmd_dump(args, json),
        Command::Export(args) => cmd_export(args, json),
        Command::Compact(args) => cmd_compact(args, json),
        Command::Check(args) => cmd_check(args, json),
    }
}

fn truncate_for(no_truncate: bool) -> Option<usize> {
    if no_truncate {
        None
    } else {
        Some(DEFAULT_TRUNCATE_BYTES)
    }
}

// -----------------------------------------------------------------
// scan
// -----------------------------------------------------------------

fn cmd_scan(args: ScanArgs, json: bool) -> Result<ExitCode> {
    let log = RecordLog::open(&args.store)
        .with_context(|| format!("open record log {}", args.store.display()))?;

    let from_seg = args.from_segment;
    let from_off = args.from_offset.unwrap_or(0);
    let limit = args.limit;
    let mode: BytesMode = args.bytes.into();
    let truncate = truncate_for(args.no_truncate);

    let mut emitted: u64 = 0;
    {
        let iter = log.scan_iter()?;
        for item in iter {
            let rec = match item {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("datawal: scan: mid-stream error: {:#}", e);
                    return Ok(ExitCode::from(3));
                }
            };

            if let Some(seg) = from_seg {
                if rec.segment < seg {
                    continue;
                }
                if rec.segment == seg && rec.offset < from_off {
                    continue;
                }
            }

            if json {
                let line = RecordLine::from_record(&rec);
                println!("{}", serde_json::to_string(&line)?);
            } else {
                print_record_human(&rec, mode, truncate);
            }

            emitted += 1;
            if let Some(lim) = limit {
                if emitted >= lim {
                    break;
                }
            }
        }
    }

    let report = log.recovery_report()?;
    if report.tail_truncated > 0 {
        Ok(ExitCode::from(2))
    } else {
        Ok(ExitCode::SUCCESS)
    }
}

fn print_record_human(rec: &Record, mode: BytesMode, truncate: Option<usize>) {
    let rt = match rec.record_type {
        datawal::RecordType::Raw => "RAW",
        datawal::RecordType::Put => "PUT",
        datawal::RecordType::Delete => "DEL",
    };
    let key = render_for_human(&rec.key, mode, truncate);
    let payload = render_for_human(&rec.payload, mode, truncate);
    println!(
        "seg={:08} off={:>10} len={:>8} type={:<3} txid={:>10} key={} payload={}",
        rec.segment, rec.offset, rec.len, rt, rec.txid, key, payload,
    );
}

// -----------------------------------------------------------------
// get
// -----------------------------------------------------------------

fn cmd_get(args: GetArgs, json: bool) -> Result<ExitCode> {
    let key = decode_key(&args)?;
    let mode: BytesMode = args.bytes.into();
    let truncate = truncate_for(args.no_truncate);

    let mut store = DataWal::open(&args.store)
        .with_context(|| format!("open store {}", args.store.display()))?;
    match store.get(&key)? {
        Some(value) => {
            if json {
                let line = ValueHit::new(&key, &value);
                println!("{}", serde_json::to_string(&line)?);
            } else {
                match render_value_for_get(&value, mode, truncate) {
                    Ok(rendered) => println!("{}", rendered),
                    Err(hint) => {
                        // Binary value in Auto/Raw mode: don't dump
                        // control bytes; emit a hint on stderr and
                        // exit 0 (it was a hit). The user can re-run
                        // with `--bytes base64` / `--bytes hex` /
                        // `--json` to retrieve actual bytes.
                        eprintln!("{}", hint);
                    }
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        None => {
            if json {
                let line = ValueMiss::new(&key);
                println!("{}", serde_json::to_string(&line)?);
            } else {
                eprintln!("datawal: get: key not found");
            }
            Ok(ExitCode::from(2))
        }
    }
}

fn decode_key(args: &GetArgs) -> Result<Vec<u8>> {
    match (&args.key, &args.key_base64, &args.key_hex) {
        (Some(text), None, None) => Ok(text.as_bytes().to_vec()),
        (None, Some(b64), None) => B64
            .decode(b64.as_bytes())
            .map_err(|e| anyhow!("invalid --key-base64: {e}")),
        (None, None, Some(hex)) => decode_hex(hex),
        (None, None, None) => Err(anyhow!(
            "pass exactly one of --key, --key-base64, or --key-hex"
        )),
        // clap `group = "key"` should already prevent multi-set, but
        // we treat any unexpected combination defensively.
        _ => Err(anyhow!(
            "pass exactly one of --key, --key-base64, or --key-hex"
        )),
    }
}

fn decode_hex(s: &str) -> Result<Vec<u8>> {
    if s.len() % 2 != 0 {
        return Err(anyhow!("invalid --key-hex: odd length"));
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let bytes = s.as_bytes();
    for chunk in bytes.chunks_exact(2) {
        let hi = hex_digit(chunk[0])?;
        let lo = hex_digit(chunk[1])?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

fn hex_digit(b: u8) -> Result<u8> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err(anyhow!("invalid --key-hex: non-hex byte {:?}", b as char)),
    }
}

// -----------------------------------------------------------------
// report
// -----------------------------------------------------------------

fn cmd_report(args: StoreArg, json: bool) -> Result<ExitCode> {
    let log = RecordLog::open(&args.store)
        .with_context(|| format!("open record log {}", args.store.display()))?;
    let r = log.recovery_report()?;

    if json {
        let obj = ReportObj::from_report(&r);
        println!("{}", serde_json::to_string(&obj)?);
    } else {
        println!("schema:              {}", SCHEMA);
        println!("files_scanned:       {}", r.files_scanned);
        println!("records_replayed:    {}", r.records_replayed);
        println!("tail_truncated:      {}", r.tail_truncated);
        println!("tail_bytes_discarded:{}", r.tail_bytes_discarded);
        println!("mid_stream_errors:   {}", r.mid_stream_errors);
        println!("unsupported_versions:{}", r.unsupported_versions);
        println!("last_txid_seen:      {}", r.last_txid_seen);
    }

    if r.tail_truncated > 0 {
        Ok(ExitCode::from(2))
    } else {
        Ok(ExitCode::SUCCESS)
    }
}

// -----------------------------------------------------------------
// verify
// -----------------------------------------------------------------

fn cmd_verify(args: StoreArg, json: bool) -> Result<ExitCode> {
    let log = RecordLog::open(&args.store)
        .with_context(|| format!("open record log {}", args.store.display()))?;

    let mut frames_checked: u64 = 0;
    let mut last_segment: u32 = 0;
    let mut last_offset: u64 = 0;
    let final_report;

    {
        let mut iter = log.scan_iter()?;
        loop {
            match iter.next() {
                Some(Ok(rec)) => {
                    frames_checked += 1;
                    last_segment = rec.segment;
                    last_offset = rec.offset;
                }
                Some(Err(e)) => {
                    eprintln!("datawal: verify: CRC / decode failure: {:#}", e);
                    return Ok(ExitCode::from(3));
                }
                None => break,
            }
        }
        final_report = iter.recovery_report();
    }

    let obj = VerifyObj {
        schema: SCHEMA,
        kind: "verify",
        frames_checked,
        crc_failures: 0,
        tail_truncated: final_report.tail_truncated,
        tail_bytes_discarded: final_report.tail_bytes_discarded,
        last_segment,
        last_offset,
    };

    if json {
        println!("{}", serde_json::to_string(&obj)?);
    } else {
        println!("schema:              {}", obj.schema);
        println!("frames_checked:      {}", obj.frames_checked);
        println!("crc_failures:        {}", obj.crc_failures);
        println!("tail_truncated:      {}", obj.tail_truncated);
        println!("tail_bytes_discarded:{}", obj.tail_bytes_discarded);
        println!("last_segment:        {}", obj.last_segment);
        println!("last_offset:         {}", obj.last_offset);
    }

    if final_report.tail_truncated > 0 {
        Ok(ExitCode::from(2))
    } else {
        Ok(ExitCode::SUCCESS)
    }
}

// -----------------------------------------------------------------
// dump
// -----------------------------------------------------------------

fn cmd_dump(args: DumpArgs, json: bool) -> Result<ExitCode> {
    let log = RecordLog::open(&args.store)
        .with_context(|| format!("open record log {}", args.store.display()))?;
    let iter = log.scan_iter()?;
    let mode: BytesMode = args.bytes.into();
    let truncate = truncate_for(args.no_truncate);

    let mut emitted: u64 = 0;
    for item in iter {
        let rec = match item {
            Ok(r) => r,
            Err(e) => {
                eprintln!("datawal: dump: mid-stream error: {:#}", e);
                return Ok(ExitCode::from(3));
            }
        };

        if json {
            let line = FrameLine::from_record(&rec);
            println!("{}", serde_json::to_string(&line)?);
        } else {
            print_frame_human(&rec, mode, truncate);
        }

        emitted += 1;
        if let Some(lim) = args.limit {
            if emitted >= lim {
                break;
            }
        }
    }

    Ok(ExitCode::SUCCESS)
}

fn print_frame_human(rec: &Record, mode: BytesMode, truncate: Option<usize>) {
    let rt = match rec.record_type {
        datawal::RecordType::Raw => "RAW",
        datawal::RecordType::Put => "PUT",
        datawal::RecordType::Delete => "DEL",
    };
    // `dump` is header-only: never emit payload bytes, even when
    // they're printable. Always show payload_len numerically. Keys
    // are small (max 64 KiB) and inspecting them is the whole point,
    // so we do render the key.
    let key = render_for_human(&rec.key, mode, truncate);
    println!(
        "seg={:08} off={:>10} len={:>8} type={:<3} txid={:>10} key={} key_len={} payload_len={}",
        rec.segment,
        rec.offset,
        rec.len,
        rt,
        rec.txid,
        key,
        rec.key.len(),
        rec.payload.len(),
    );
}

// -----------------------------------------------------------------
// export
// -----------------------------------------------------------------

fn cmd_export(args: ExportArgs, json: bool) -> Result<ExitCode> {
    // Refuse to clobber an existing outfile. `DataWal::export_jsonl`
    // would itself fail on the open-for-write, but checking up front
    // gives a uniform error path and lets us emit a stable, scriptable
    // diagnostic (exit 1) before touching the source store at all.
    if args.outfile.exists() {
        eprintln!(
            "datawal: export: refuses to overwrite existing file: {}",
            args.outfile.display()
        );
        return Ok(ExitCode::from(1));
    }

    let mut store = DataWal::open(&args.store)
        .with_context(|| format!("open store {}", args.store.display()))?;

    // Live keys at the moment of open. `export_jsonl` walks the same
    // KV projection (last-write-wins, tombstones suppressed), so this
    // count matches the number of JSON lines written.
    let records_written = store.keys().len() as u64;

    store
        .export_jsonl(&args.outfile)
        .with_context(|| format!("export jsonl to {}", args.outfile.display()))?;

    let bytes_written = std::fs::metadata(&args.outfile)
        .with_context(|| format!("stat {}", args.outfile.display()))?
        .len();

    let obj = ExportObj::new(&args.outfile, records_written, bytes_written);
    if json {
        println!("{}", serde_json::to_string(&obj)?);
    } else {
        println!("schema:          {}", obj.schema);
        println!("kind:            {}", obj.kind);
        println!("outfile:         {}", obj.outfile);
        println!("records_written: {}", obj.records_written);
        println!("bytes_written:   {}", obj.bytes_written);
    }

    Ok(ExitCode::SUCCESS)
}

// -----------------------------------------------------------------
// compact
// -----------------------------------------------------------------

fn cmd_compact(args: CompactArgs, json: bool) -> Result<ExitCode> {
    // `DataWal::compact_to` refuses non-empty targets, but a pre-flight
    // check keeps the error code uniform (exit 1) and avoids opening
    // the source on a clearly broken invocation.
    if args.target.exists() {
        let mut entries = std::fs::read_dir(&args.target)
            .with_context(|| format!("read target dir {}", args.target.display()))?;
        if entries.next().is_some() {
            eprintln!(
                "datawal: compact: target directory is not empty: {}",
                args.target.display()
            );
            return Ok(ExitCode::from(1));
        }
    }

    let mut store = DataWal::open(&args.store)
        .with_context(|| format!("open store {}", args.store.display()))?;

    let stats = store
        .compact_to(&args.target)
        .with_context(|| format!("compact to {}", args.target.display()))?;

    let obj = CompactObj::new(&args.target, &stats);
    if json {
        println!("{}", serde_json::to_string(&obj)?);
    } else {
        println!("schema:          {}", obj.schema);
        println!("kind:            {}", obj.kind);
        println!("target:          {}", obj.target);
        println!("live_keys:       {}", obj.live_keys);
        println!("records_written: {}", obj.records_written);
        println!("bytes_written:   {}", obj.bytes_written);
    }

    Ok(ExitCode::SUCCESS)
}

// -----------------------------------------------------------------
// check
// -----------------------------------------------------------------

fn cmd_check(args: StoreArg, json: bool) -> Result<ExitCode> {
    let mut store = DataWal::open(&args.store)
        .with_context(|| format!("open store {}", args.store.display()))?;

    // Snapshot the live keyset before iterating, so a hypothetical
    // future concurrent reader on the same handle cannot change what
    // we check mid-loop. `keys()` already returns owned `Vec<Vec<u8>>`.
    let keys = store.keys();
    let keys_checked = keys.len() as u64;

    for k in &keys {
        match store.get(k) {
            Ok(Some(_)) => { /* live key resolves end-to-end */ }
            Ok(None) => {
                // `keys()` is the KV projection; a None here would
                // mean the projection and the keydir disagree, which
                // is a hard internal-consistency failure.
                eprintln!(
                    "datawal: check: key reported by keys() is missing on get: b64:{}",
                    B64.encode(k)
                );
                return Ok(ExitCode::from(3));
            }
            Err(e) => {
                eprintln!("datawal: check: get failed: {:#}", e);
                return Ok(ExitCode::from(3));
            }
        }
    }

    let report = store.log().recovery_report().context("recovery report")?;

    let obj = CheckObj::new(keys_checked, &report);
    if json {
        println!("{}", serde_json::to_string(&obj)?);
    } else {
        println!("schema:               {}", obj.schema);
        println!("kind:                 {}", obj.kind);
        println!("keys_checked:         {}", obj.keys_checked);
        println!("tail_truncated:       {}", obj.tail_truncated);
        println!("tail_bytes_discarded: {}", obj.tail_bytes_discarded);
        println!("mid_stream_errors:    {}", obj.mid_stream_errors);
        println!("unsupported_versions: {}", obj.unsupported_versions);
    }

    if report.tail_truncated > 0 || report.mid_stream_errors > 0 {
        Ok(ExitCode::from(2))
    } else {
        Ok(ExitCode::SUCCESS)
    }
}
