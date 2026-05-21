//! Subcommand dispatch and implementations.
//!
//! Each subcommand is read-only. `RecordLog::open` and `DataWal::open`
//! still acquire the cooperative single-writer lock; this is by
//! design — an inspector that bypassed the lock could observe a
//! partially-written tail in flight and would violate the
//! single-writer invariant. If the store is already opened by a
//! running writer, the inspector fails with exit code 1.

use std::process::ExitCode;

use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use datawal::{DataWal, Record, RecordLog};

use crate::bytes_render::{
    render_for_human, render_value_for_get, BytesMode, DEFAULT_TRUNCATE_BYTES,
};
use crate::cli::{Cli, Command, DumpArgs, GetArgs, ScanArgs, StoreArg};
use crate::output::{FrameLine, RecordLine, ReportObj, ValueHit, ValueMiss, VerifyObj, SCHEMA};

/// Exit codes:
/// - 0 success
/// - 1 user / configuration / store-locked error (handled in `main`)
/// - 2 recoverable storage state observed (truncated tail, missing
///   key on `get`)
/// - 3 hard storage error (CRC failure in a sealed segment)
pub fn dispatch(cli: Cli) -> Result<ExitCode> {
    let json = cli.json;
    match cli.command {
        Command::Scan(args) => cmd_scan(args, json),
        Command::Get(args) => cmd_get(args, json),
        Command::Report(args) => cmd_report(args, json),
        Command::Verify(args) => cmd_verify(args, json),
        Command::Dump(args) => cmd_dump(args, json),
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
