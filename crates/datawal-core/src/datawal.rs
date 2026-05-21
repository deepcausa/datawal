//! Last-write-wins bytes-based KV projection over [`RecordLog`].
//!
//! v0.1.4 semantics:
//! - `put(key, value)` appends a `Put` record and updates the in-memory keydir.
//! - `delete(key)` appends a `Delete` (tombstone) record and removes the key
//!   from the keydir.
//! - `get(key)` looks up the keydir, then performs a positional read against
//!   the segment file (`pread` on Unix), validates the per-record CRC32C, and
//!   returns the payload bytes. Takes `&mut self` because it maintains an
//!   internal LRU of file descriptors (see [`fd_pool`](crate::fd_pool)).
//! - `open(dir)` rebuilds the keydir by scanning the log lazily via
//!   [`RecordLog::scan_iter`]; payloads are **not** materialised in memory.
//! - `compact_to(out_dir)` writes a new datawal log into `out_dir` containing
//!   exactly one `Put` per live key. Payloads are read from the source via
//!   the same `get` path used by callers (CRC re-validated). The original
//!   log is untouched.
//! - `export_jsonl(out)` writes one JSON object per live key to `out` using
//!   `safeatomic_rs::write_atomic` (base64-encoded bytes).
//!
//! ## Why `get` takes `&mut self`
//!
//! Pre-0.1.4 the keydir stored the full payload alongside the key, so
//! `get` was a pure HashMap lookup that fit `&self`. In 0.1.4 the keydir
//! stores a [`RecordRef`] (segment + offset + length) instead, and `get`
//! performs disk I/O plus CRC validation. To avoid re-opening the same
//! segment file on every call, `DataWal` keeps a small LRU of read-only
//! file descriptors keyed by segment id; touching that cache requires
//! `&mut`. The trade-off is a smaller `DataWal` footprint in RAM (only
//! key bytes and 16-byte refs are kept) at the cost of one read syscall
//! and one CRC check per `get`.
//!
//! Out of scope for v0.1.4:
//! - In-place `compact()` that swaps segments atomically.
//! - Transactions / atomic multi-record commits.
//! - Concurrent writers.
//! - Iteration over historical record versions.
//! - Asynchronous I/O.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use serde::Serialize;

use crate::fd_pool::{FdPool, DEFAULT_CAPACITY};
use crate::format::{decode_next, DecodeOutcome, RecordType, HEADER_LEN};
use crate::record_log::{RecordLog, RecordRef};

/// Stats reported by a successful compaction.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompactionStats {
    /// Number of live keys written to the output log.
    pub live_keys: u64,
    /// Number of records written. Equal to `live_keys` in v0.1.4.
    pub records_written: u64,
    /// Total framed bytes written. Useful for sanity checks.
    pub bytes_written: u64,
}

/// Bytes-based key/value store backed by a [`RecordLog`].
///
/// See the [module docs](crate) for the semantic contract and the rationale
/// for the `&mut self` signatures on read paths.
#[derive(Debug)]
pub struct DataWal {
    log: RecordLog,
    /// In-memory live keydir. Stores only on-disk references; payloads
    /// are read on demand through `fd_pool`.
    map: HashMap<Vec<u8>, RecordRef>,
    /// Bounded LRU of read-only segment file descriptors.
    fd_pool: FdPool,
}

impl DataWal {
    /// Open (or create) a `DataWal` rooted at `dir`. Rebuilds the keydir
    /// by scanning the underlying record log lazily; payloads are not
    /// materialised in memory.
    pub fn open(dir: &Path) -> Result<Self> {
        let log = RecordLog::open(dir)?;
        let map = rebuild_keydir_lazy(&log)?;
        Ok(Self {
            log,
            map,
            fd_pool: FdPool::with_capacity(DEFAULT_CAPACITY),
        })
    }

    /// Path of the directory backing this store.
    pub fn dir(&self) -> &Path {
        self.log.dir()
    }

    /// Number of live keys.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// `true` if there are no live keys.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Returns `true` if `key` is currently live. Does not read from disk.
    pub fn contains_key(&self, key: &[u8]) -> bool {
        self.map.contains_key(key)
    }

    /// Returns the on-disk reference for `key`, if live. Does not read
    /// the payload from disk.
    pub fn ref_of(&self, key: &[u8]) -> Option<RecordRef> {
        self.map.get(key).copied()
    }

    /// Reads the live value for `key`, validating the per-record CRC32C.
    ///
    /// Performs a positional read (`pread` on Unix) against the segment
    /// holding the most recent `Put` for `key`. Takes `&mut self` because
    /// it maintains an internal LRU of read-only segment file descriptors;
    /// see the [module docs](crate) for the rationale.
    ///
    /// Returns `Err` if the on-disk frame is structurally invalid, the
    /// CRC does not match, or the segment file cannot be opened.
    pub fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let rref = match self.map.get(key).copied() {
            Some(r) => r,
            None => return Ok(None),
        };
        let bytes =
            self.fd_pool
                .read_at(self.log.dir(), rref.segment, rref.offset, rref.len as usize)?;
        decode_payload_for_get(&bytes, rref).map(Some)
    }

    /// All live keys, unordered (HashMap iteration order). Does not read
    /// from disk.
    pub fn keys(&self) -> Vec<Vec<u8>> {
        self.map.keys().cloned().collect()
    }

    /// All live `(key, value)` pairs, unordered. Performs one disk read
    /// per key (CRC validated) and takes `&mut self` for the same reason
    /// `get` does. Returns the first I/O or CRC error if any.
    pub fn items(&mut self) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let keys: Vec<Vec<u8>> = self.map.keys().cloned().collect();
        let mut out = Vec::with_capacity(keys.len());
        for k in keys {
            let v = self.get(&k)?.ok_or_else(|| {
                anyhow::anyhow!("datawal: keydir desync for key (len={})", k.len())
            })?;
            out.push((k, v));
        }
        Ok(out)
    }

    /// Append a `Put` record and update the keydir with the returned
    /// on-disk reference.
    pub fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        let rref = self.log.append_record(RecordType::Put, key, value)?;
        self.map.insert(key.to_vec(), rref);
        Ok(())
    }

    /// Append a `Delete` tombstone and remove the key from the keydir.
    ///
    /// Deleting a non-existent key is a no-op at the keydir level, but the
    /// tombstone is still written so the log accurately reflects the
    /// intent.
    pub fn delete(&mut self, key: &[u8]) -> Result<()> {
        self.log.append_record(RecordType::Delete, key, b"")?;
        self.map.remove(key);
        Ok(())
    }

    /// Force durability of the underlying record log.
    pub fn fsync(&mut self) -> Result<()> {
        self.log.fsync()
    }

    /// Underlying record log (read-only handle).
    pub fn log(&self) -> &RecordLog {
        &self.log
    }

    /// Write a clean compacted copy of this DataWal into `out_dir`.
    ///
    /// `out_dir` must be empty (or non-existent). Tombstones are not
    /// carried over: deleted keys simply disappear. The original
    /// `self` is **not** modified.
    ///
    /// Each live value is read from the source via the same disk-read /
    /// CRC-validate path used by [`Self::get`]; this re-validates every
    /// record copied to the compacted target, at the cost of one read
    /// syscall per key.
    pub fn compact_to(&mut self, out_dir: &Path) -> Result<CompactionStats> {
        // Refuse to clobber an existing non-empty directory or an existing
        // datawal log directory. v0.1.4 keeps this strict on purpose.
        if out_dir.exists() {
            let is_empty = std::fs::read_dir(out_dir)
                .with_context(|| {
                    format!(
                        "datawal: read_dir on compact_to target {}",
                        out_dir.display()
                    )
                })?
                .next()
                .is_none();
            if !is_empty {
                anyhow::bail!(
                    "datawal: compact_to target {} is not empty; refusing to overwrite",
                    out_dir.display()
                );
            }
        }

        // Sort keys for deterministic output. Useful for diffing snapshots.
        let mut sorted_keys: Vec<Vec<u8>> = self.map.keys().cloned().collect();
        sorted_keys.sort();

        let mut out_log = RecordLog::open(out_dir)?;
        let mut stats = CompactionStats::default();
        for k in sorted_keys {
            let v = self
                .get(&k)?
                .ok_or_else(|| anyhow::anyhow!("datawal: keydir desync during compact_to"))?;
            let r = out_log.append_record(RecordType::Put, &k, &v)?;
            stats.records_written += 1;
            stats.bytes_written += r.len as u64;
        }
        stats.live_keys = stats.records_written;
        out_log.fsync()?;
        out_log.close()?;
        Ok(stats)
    }

    /// Write one JSONL line per live key to `out_path` using
    /// [`safeatomic_rs::write_atomic`]. Each line has the shape:
    ///
    /// ```json
    /// {"key_b64": "...", "value_b64": "..."}
    /// ```
    ///
    /// Keys are emitted in sorted byte order for deterministic output.
    /// This is **not** an analytics format: keys and values are opaque
    /// bytes; consumers must base64-decode them.
    ///
    /// Performs one disk read per live key (CRC validated) and therefore
    /// takes `&mut self`.
    pub fn export_jsonl(&mut self, out_path: &Path) -> Result<()> {
        #[derive(Serialize)]
        struct Row<'a> {
            key_b64: &'a str,
            value_b64: &'a str,
        }

        let mut sorted_keys: Vec<Vec<u8>> = self.map.keys().cloned().collect();
        sorted_keys.sort();

        let mut buf = String::new();
        for k in sorted_keys {
            let v = self
                .get(&k)?
                .ok_or_else(|| anyhow::anyhow!("datawal: keydir desync during export_jsonl"))?;
            let k_b64 = B64.encode(&k);
            let v_b64 = B64.encode(&v);
            let line = serde_json::to_string(&Row {
                key_b64: &k_b64,
                value_b64: &v_b64,
            })
            .context("datawal: serialize JSONL row")?;
            buf.push_str(&line);
            buf.push('\n');
        }

        // Ensure the parent directory exists; `write_atomic` does not mkdir.
        if let Some(parent) = out_path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("datawal: create_dir_all {}", parent.display()))?;
            }
        }
        safeatomic_rs::write_atomic(out_path, buf.as_bytes())
            .with_context(|| format!("datawal: write_atomic {}", out_path.display()))?;
        Ok(())
    }

    /// Stable path representation, useful for logging/diagnostics.
    pub fn dir_path_buf(&self) -> PathBuf {
        self.log.dir().to_path_buf()
    }
}

/// Replay the log lazily through [`RecordLog::scan_iter`] to build a
/// `key -> RecordRef` map. Last-write-wins by physical order.
///
/// Payloads are not materialised: only `RecordRef`s are inserted into
/// the map. Tombstones (`Delete`) remove the prior entry, mirroring the
/// pre-0.1.4 behaviour.
fn rebuild_keydir_lazy(log: &RecordLog) -> Result<HashMap<Vec<u8>, RecordRef>> {
    let mut map: HashMap<Vec<u8>, RecordRef> = HashMap::new();
    let iter = log.scan_iter()?;
    for rec in iter {
        let rec = rec?;
        match rec.record_type {
            RecordType::Put => {
                map.insert(
                    rec.key,
                    RecordRef {
                        segment: rec.segment,
                        offset: rec.offset,
                        len: rec.len,
                    },
                );
            }
            RecordType::Delete => {
                map.remove(&rec.key);
            }
            RecordType::Raw => {
                // Raw records are not part of the KV projection. They are
                // valid in the log but invisible to DataWal.
            }
        }
    }
    Ok(map)
}

/// Decode the payload at a positionally-read frame, sanity-checking the
/// frame against the `RecordRef` that pointed at it.
///
/// Returns the payload bytes on success, or an `anyhow` error otherwise.
fn decode_payload_for_get(bytes: &[u8], rref: RecordRef) -> Result<Vec<u8>> {
    if bytes.len() < HEADER_LEN {
        anyhow::bail!(
            "datawal: short read for segment {:08} offset {} (got {}, need at least {})",
            rref.segment,
            rref.offset,
            bytes.len(),
            HEADER_LEN
        );
    }
    match decode_next(bytes, 0) {
        Ok(DecodeOutcome::Ok {
            record_type,
            key: _key,
            payload,
            bytes_consumed,
            ..
        }) => {
            if bytes_consumed != rref.len {
                anyhow::bail!(
                    "datawal: frame length mismatch at segment {:08} offset {} (decoded {} vs ref {})",
                    rref.segment,
                    rref.offset,
                    bytes_consumed,
                    rref.len
                );
            }
            // The keydir only points at Put records, so anything else here
            // is corruption — surface it explicitly instead of silently
            // returning bytes.
            if !matches!(record_type, RecordType::Put) {
                anyhow::bail!(
                    "datawal: record at segment {:08} offset {} is not a Put (found {:?})",
                    rref.segment,
                    rref.offset,
                    record_type
                );
            }
            Ok(payload)
        }
        Ok(DecodeOutcome::Truncated { available, needed }) => {
            anyhow::bail!(
                "datawal: truncated frame at segment {:08} offset {} (available {}, needed {})",
                rref.segment,
                rref.offset,
                available,
                needed
            );
        }
        Ok(DecodeOutcome::CrcMismatch { bytes_consumed }) => {
            anyhow::bail!(
                "datawal: CRC mismatch at segment {:08} offset {} (size {})",
                rref.segment,
                rref.offset,
                bytes_consumed
            );
        }
        Err(e) => Err(anyhow::Error::new(e).context(format!(
            "datawal: structural decode error at segment {:08} offset {}",
            rref.segment, rref.offset
        ))),
    }
}
