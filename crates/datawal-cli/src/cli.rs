//! Command-line argument parsing via clap derive.
//!
//! All subcommands are read-only inspection over a datawal store.
//! No subcommand performs `put` / `delete` / `rotate` / `compact`;
//! mutating operations are out of scope for this binary in 0.1.x.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// Read-only inspector for datawal stores.
#[derive(Debug, Parser)]
#[command(
    name = "datawal",
    version,
    about = "Read-only inspector for datawal stores (scan, get, report, verify, dump).",
    long_about = None,
)]
pub struct Cli {
    /// Emit machine-readable JSON on stdout (schema `datawal.cli.v1`).
    ///
    /// Without `--json`, output is a compact human-readable form on
    /// stdout. Either form goes only to stdout; diagnostics and errors
    /// always go to stderr.
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
}

/// Common argument shared by subcommands that only need the store path.
#[derive(Debug, clap::Args)]
pub struct StoreArg {
    /// Path to the datawal store directory.
    pub store: PathBuf,
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
}

#[derive(Debug, clap::Args)]
pub struct GetArgs {
    /// Path to the datawal store directory.
    pub store: PathBuf,

    /// Key as base64 (standard alphabet, with padding).
    #[arg(long, group = "key", value_name = "B64")]
    pub key_base64: Option<String>,

    /// Key as hex (lowercase or uppercase, no `0x` prefix).
    #[arg(long, group = "key", value_name = "HEX")]
    pub key_hex: Option<String>,
}

#[derive(Debug, clap::Args)]
pub struct DumpArgs {
    /// Path to the datawal store directory.
    pub store: PathBuf,

    /// Stop after emitting this many frames.
    #[arg(long)]
    pub limit: Option<u64>,
}
