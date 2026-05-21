//! Command-line argument parsing via clap derive.
//!
//! Subcommands fall into two groups:
//!
//! - **Inspection** (read-only): `scan`, `get`, `report`, `verify`, `dump`,
//!   `check`. They never write to the source store.
//! - **Source-untouched mutations**: `export`, `compact`. They produce a new
//!   artefact at a caller-supplied output path; the source store on disk is
//!   never modified.
//!
//! No subcommand performs `put` / `delete` / `rotate` in-place. Mutating
//! the source is intentionally out of scope for this binary in 0.1.x.

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

use crate::bytes_render::BytesMode;

/// CLI for datawal stores: read-only inspection plus source-untouched
/// export/compact operations.
#[derive(Debug, Parser)]
#[command(
    name = "datawal",
    version,
    about = "Inspect datawal stores and produce derived artefacts (scan, get, report, verify, dump, check, export, compact).",
    long_about = None,
)]
pub struct Cli {
    /// Emit machine-readable JSON on stdout (schema `datawal.cli.v1`).
    ///
    /// Without `--json`, output is a compact human-readable form on
    /// stdout. In human form, printable-ASCII keys and payloads are
    /// rendered literally (quoted as needed) and binary bytes are
    /// rendered with an explicit `b64:` or `hex:` prefix. The JSON
    /// stream is stable: it always carries base64-encoded bytes and
    /// is never affected by `--bytes`.
    ///
    /// Diagnostics and errors always go to stderr.
    #[arg(long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// List records in segment order.
    ///
    /// Walks the log via the same record-level lazy iterator used
    /// internally by `RecordLog::scan_iter`; does not materialise the
    /// whole log in memory.
    Scan(ScanArgs),

    /// Fetch the current value for a key from the KV projection.
    ///
    /// Opens the store as a `DataWal` (last-write-wins projection)
    /// and performs a single point lookup. Exits with code 2 when
    /// the key is absent.
    Get(GetArgs),

    /// Print the `RecoveryReport` for the store.
    ///
    /// Reports files scanned, records replayed, last-txid, tail
    /// truncation bytes and any unsupported-version frames.
    Report(StoreArg),

    /// Re-verify CRC32C on every frame in every segment.
    ///
    /// Exits with code 3 on the first CRC failure encountered in a
    /// sealed segment. A truncated active-segment tail is reported
    /// (exit code 2) but not treated as a hard error.
    Verify(StoreArg),

    /// Print raw frame headers (no payload bytes) for debugging.
    ///
    /// One line per frame; useful for inspecting wire layout without
    /// dumping potentially large payloads.
    Dump(DumpArgs),

    /// Export the live KV projection as JSONL to a new file.
    ///
    /// Opens the store as a `DataWal` and writes one JSON object per
    /// live key (base64-encoded payload) via `DataWal::export_jsonl`.
    /// The source store on disk is never modified. Refuses to
    /// overwrite an existing `OUTFILE` (exit 1).
    Export(ExportArgs),
    /// Snapshot-compact the store into a fresh target directory.
    ///
    /// Calls `DataWal::compact_to`, which rebuilds a minimal log
    /// containing only live keys into `TARGET`. `TARGET` must not
    /// exist or must be empty (exit 1 otherwise). The source store
    /// on disk is never modified; the caller decides when (and if)
    /// to swap the directories.
    Compact(CompactArgs),
    /// Open the store, validate every live key end-to-end, and
    /// report source health.
    ///
    /// Performs `DataWal::open` + a `get` for each `keys()` entry
    /// (forcing per-record CRC32C revalidation via the fd pool) and
    /// then reads the `RecoveryReport` to surface tail truncation
    /// or unsupported-version frames. Exits 2 on tail truncation,
    /// 3 on any mid-stream `get` failure.
    Check(StoreArg),
}

/// Common argument shared by subcommands that only need the store path.
#[derive(Debug, clap::Args)]
pub struct StoreArg {
    /// Path to the datawal store directory.
    pub store: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
#[value(rename_all = "lower")]
pub enum BytesModeArg {
    /// Printable ASCII -> literal text; otherwise `b64:` prefix.
    #[default]
    Auto,
    /// Same as auto, but falls back to base64 (never raw control bytes).
    Raw,
    /// Always render as `b64:<base64>` (or unprefixed base64 for `get`).
    Base64,
    /// Always render as `hex:<hex>` (or unprefixed hex for `get`).
    Hex,
}

impl From<BytesModeArg> for BytesMode {
    fn from(a: BytesModeArg) -> Self {
        match a {
            BytesModeArg::Auto => BytesMode::Auto,
            BytesModeArg::Raw => BytesMode::Raw,
            BytesModeArg::Base64 => BytesMode::Base64,
            BytesModeArg::Hex => BytesMode::Hex,
        }
    }
}

#[derive(Debug, clap::Args)]
pub struct ScanArgs {
    /// Path to the datawal store directory.
    pub store: PathBuf,

    /// Stop after emitting this many records.
    #[arg(long)]
    pub limit: Option<u64>,

    /// Skip records until this segment id (inclusive) is reached.
    #[arg(long)]
    pub from_segment: Option<u32>,

    /// Together with `--from-segment`, also skip records whose offset
    /// within that segment is less than this value.
    #[arg(long, requires = "from_segment")]
    pub from_offset: Option<u64>,

    /// How to render bytes in human form. JSON output is unaffected.
    #[arg(long, value_enum, default_value_t = BytesModeArg::Auto)]
    pub bytes: BytesModeArg,

    /// Do not truncate long keys / payloads in human form.
    #[arg(long)]
    pub no_truncate: bool,
}

#[derive(Debug, clap::Args)]
pub struct GetArgs {
    /// Path to the datawal store directory.
    pub store: PathBuf,

    /// Key as UTF-8 text (most ergonomic; pass `--key alpha`).
    #[arg(long, group = "key_input", value_name = "TEXT")]
    pub key: Option<String>,

    /// Key as base64 (standard alphabet, with padding).
    #[arg(long, group = "key_input", value_name = "B64")]
    pub key_base64: Option<String>,

    /// Key as hex (lowercase or uppercase, no `0x` prefix).
    #[arg(long, group = "key_input", value_name = "HEX")]
    pub key_hex: Option<String>,

    /// How to render the value in human form. JSON output is unaffected.
    #[arg(long, value_enum, default_value_t = BytesModeArg::Auto)]
    pub bytes: BytesModeArg,

    /// Do not truncate long values in human form.
    #[arg(long)]
    pub no_truncate: bool,
}

#[derive(Debug, clap::Args)]
pub struct DumpArgs {
    /// Path to the datawal store directory.
    pub store: PathBuf,

    /// Stop after emitting this many frames.
    #[arg(long)]
    pub limit: Option<u64>,

    /// How to render bytes in human form. JSON output is unaffected.
    #[arg(long, value_enum, default_value_t = BytesModeArg::Auto)]
    pub bytes: BytesModeArg,

    /// Do not truncate long keys / payloads in human form.
    #[arg(long)]
    pub no_truncate: bool,
}

#[derive(Debug, clap::Args)]
pub struct ExportArgs {
    /// Path to the datawal store directory (read-only).
    pub store: PathBuf,

    /// Destination JSONL file. Must not already exist; the CLI refuses
    /// to overwrite to avoid clobbering caller artefacts.
    pub outfile: PathBuf,
}

#[derive(Debug, clap::Args)]
pub struct CompactArgs {
    /// Path to the datawal store directory (read-only).
    pub store: PathBuf,

    /// Target directory for the compacted snapshot. Must either not
    /// exist or be an empty directory; `DataWal::compact_to` refuses
    /// to write into a non-empty target.
    pub target: PathBuf,
}
