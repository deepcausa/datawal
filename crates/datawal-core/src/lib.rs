//! `datawal` — append-only framed record log and bytes-based KV (v0.1-pre).
//!
//! ## What this crate is
//!
//! - [`RecordLog`]: append-only segmented record log with per-record CRC,
//!   valid-prefix recovery, and explicit `fsync`/`rotate`. The substrate.
//! - [`DataWal`]: last-write-wins bytes-based key/value projection on top of
//!   [`RecordLog`], with `put` / `get` / `delete` / tombstone semantics and
//!   manual `compact_to` plus JSONL export.
//!
//! ## What this crate is not
//!
//! v0.1-pre intentionally has **no**:
//!
//! - content-addressed storage / blob CAS
//! - Python bindings (PyO3)
//! - compression
//! - server / RPC / network exposure
//! - query / analytics engine
//! - multi-writer or multi-process safety
//! - JSON awareness — payloads are opaque bytes
//! - schema validation
//! - encryption
//! - application-specific event kinds, manifest schemas, or hashing semantics
//!
//! ## Layout on disk
//!
//! ```text
//! dir/
//!   .lock                   # cooperative sentinel file
//!   00000001.dwal           # segment 1
//!   00000002.dwal           # segment 2 (created by `rotate`)
//!   ...
//! ```
//!
//! There is no MANIFEST file in v0.1-pre: the set of segments is whatever
//! matches `[0-9]{8}\.dwal` on disk. Adding a MANIFEST is a v0.2 concern.
//!
//! ## Wire format
//!
//! See [`mod@format`] for the byte-exact specification. Each record is a 24-byte
//! header + key + payload + 4-byte CRC trailer. Multi-byte integers are
//! little-endian.
//!
//! ## Filesystem primitives
//!
//! Atomic FS operations (`write_atomic`, `fsync_dir`, `rename_atomic`, etc.)
//! live in the sibling crate [`safeatomic_rs`]. `datawal` consumes them;
//! it does not redefine them.

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms)]

pub mod format;
pub mod lock;
pub mod segment;

mod datawal;
mod record_log;

pub use datawal::{CompactionStats, DataWal};
pub use format::{RecordType, MAX_KEY_LEN, MAX_PAYLOAD_LEN, WIRE_VERSION};
pub use record_log::{Record, RecordLog, RecordRef, RecoveryReport};
