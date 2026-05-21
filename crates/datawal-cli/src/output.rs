//! JSON output types for the `datawal.cli.v1` schema.
//!
//! Every JSON object emitted by this binary carries a literal
//! `"schema":"datawal.cli.v1"` field. The schema is intentionally
//! conservative: payloads and keys are base64-encoded (no JSON-native
//! representation of arbitrary bytes), and field names use
//! `snake_case`.

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use datawal::{Record, RecoveryReport};
use serde::Serialize;

pub const SCHEMA: &str = "datawal.cli.v1";

fn record_type_str(r: datawal::RecordType) -> &'static str {
    match r {
        datawal::RecordType::Raw => "Raw",
        datawal::RecordType::Put => "Put",
        datawal::RecordType::Delete => "Delete",
    }
}

#[derive(Debug, Serialize)]
pub struct RecordLine<'a> {
    pub schema: &'a str,
    pub kind: &'a str,
    pub segment: u32,
    pub offset: u64,
    pub len: u32,
    pub record_type: &'a str,
    pub txid: u64,
    pub key_base64: String,
    pub payload_base64: String,
}

impl<'a> RecordLine<'a> {
    pub fn from_record(rec: &Record) -> Self {
        Self {
            schema: SCHEMA,
            kind: "record",
            segment: rec.segment,
            offset: rec.offset,
            len: rec.len,
            record_type: record_type_str(rec.record_type),
            txid: rec.txid,
            key_base64: B64.encode(&rec.key),
            payload_base64: B64.encode(&rec.payload),
        }
    }
}

/// Header-only frame line for `dump`. Excludes payload bytes so the
/// output stays useful even for stores with multi-MiB records.
#[derive(Debug, Serialize)]
pub struct FrameLine<'a> {
    pub schema: &'a str,
    pub kind: &'a str,
    pub segment: u32,
    pub offset: u64,
    pub len: u32,
    pub record_type: &'a str,
    pub txid: u64,
    pub key_len: usize,
    pub payload_len: usize,
}

impl<'a> FrameLine<'a> {
    pub fn from_record(rec: &Record) -> Self {
        Self {
            schema: SCHEMA,
            kind: "frame",
            segment: rec.segment,
            offset: rec.offset,
            len: rec.len,
            record_type: record_type_str(rec.record_type),
            txid: rec.txid,
            key_len: rec.key.len(),
            payload_len: rec.payload.len(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ReportObj<'a> {
    pub schema: &'a str,
    pub kind: &'a str,
    pub files_scanned: u32,
    pub records_replayed: u64,
    pub tail_truncated: u32,
    pub tail_bytes_discarded: u64,
    pub mid_stream_errors: u32,
    pub unsupported_versions: u32,
    pub last_txid_seen: u64,
}

impl<'a> ReportObj<'a> {
    pub fn from_report(r: &RecoveryReport) -> Self {
        Self {
            schema: SCHEMA,
            kind: "report",
            files_scanned: r.files_scanned,
            records_replayed: r.records_replayed,
            tail_truncated: r.tail_truncated,
            tail_bytes_discarded: r.tail_bytes_discarded,
            mid_stream_errors: r.mid_stream_errors,
            unsupported_versions: r.unsupported_versions,
            last_txid_seen: r.last_txid_seen,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct VerifyObj<'a> {
    pub schema: &'a str,
    pub kind: &'a str,
    pub frames_checked: u64,
    pub crc_failures: u64,
    pub tail_truncated: u32,
    pub tail_bytes_discarded: u64,
    pub last_segment: u32,
    pub last_offset: u64,
}

#[derive(Debug, Serialize)]
pub struct ValueHit<'a> {
    pub schema: &'a str,
    pub kind: &'a str,
    pub key_base64: String,
    pub value_base64: String,
    pub value_len: usize,
}

impl<'a> ValueHit<'a> {
    pub fn new(key: &[u8], value: &[u8]) -> Self {
        Self {
            schema: SCHEMA,
            kind: "value",
            key_base64: B64.encode(key),
            value_base64: B64.encode(value),
            value_len: value.len(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ValueMiss<'a> {
    pub schema: &'a str,
    pub kind: &'a str,
    pub key_base64: String,
}

impl<'a> ValueMiss<'a> {
    pub fn new(key: &[u8]) -> Self {
        Self {
            schema: SCHEMA,
            kind: "miss",
            key_base64: B64.encode(key),
        }
    }
}

/// Result of the `export` subcommand: a JSONL file was written at
/// `outfile`. Counts reflect the live KV projection visible at the
/// moment `DataWal::open` ran.
#[derive(Debug, Serialize)]
pub struct ExportObj<'a> {
    pub schema: &'a str,
    pub kind: &'a str,
    pub outfile: String,
    pub records_written: u64,
    pub bytes_written: u64,
}

impl<'a> ExportObj<'a> {
    pub fn new(outfile: &std::path::Path, records_written: u64, bytes_written: u64) -> Self {
        Self {
            schema: SCHEMA,
            kind: "export",
            outfile: outfile.display().to_string(),
            records_written,
            bytes_written,
        }
    }
}

/// Result of the `compact` subcommand: a snapshot was written at
/// `target`. Numeric fields mirror `datawal::CompactionStats`.
#[derive(Debug, Serialize)]
pub struct CompactObj<'a> {
    pub schema: &'a str,
    pub kind: &'a str,
    pub target: String,
    pub live_keys: u64,
    pub records_written: u64,
    pub bytes_written: u64,
}

impl<'a> CompactObj<'a> {
    pub fn new(target: &std::path::Path, stats: &datawal::CompactionStats) -> Self {
        Self {
            schema: SCHEMA,
            kind: "compact",
            target: target.display().to_string(),
            live_keys: stats.live_keys,
            records_written: stats.records_written,
            bytes_written: stats.bytes_written,
        }
    }
}

/// Result of the `check` subcommand: store-level health summary.
///
/// `keys_checked` is the number of live keys for which a `get`
/// succeeded end-to-end (decode + CRC32C revalidation). The remaining
/// fields are copied from `RecoveryReport` and tell the caller why
/// the run may still have surfaced a non-zero exit code.
#[derive(Debug, Serialize)]
pub struct CheckObj<'a> {
    pub schema: &'a str,
    pub kind: &'a str,
    pub keys_checked: u64,
    pub tail_truncated: u32,
    pub tail_bytes_discarded: u64,
    pub mid_stream_errors: u32,
    pub unsupported_versions: u32,
}

impl<'a> CheckObj<'a> {
    pub fn new(keys_checked: u64, r: &RecoveryReport) -> Self {
        Self {
            schema: SCHEMA,
            kind: "check",
            keys_checked,
            tail_truncated: r.tail_truncated,
            tail_bytes_discarded: r.tail_bytes_discarded,
            mid_stream_errors: r.mid_stream_errors,
            unsupported_versions: r.unsupported_versions,
        }
    }
}
