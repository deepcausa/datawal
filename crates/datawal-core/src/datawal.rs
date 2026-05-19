//! Last-write-wins bytes-based KV projection over [`RecordLog`].
//!
//! v0.1-pre semantics:
//! - `put(key, value)` appends a `Put` record and updates the in-memory keydir.
//! - `delete(key)` appends a `Delete` (tombstone) record and removes the key
//!   from the keydir.
//! - `get(key)` reads from the in-memory keydir (payload is held in memory).
//! - `open(dir)` rebuilds the keydir by scanning the log from segment 1 in
//!   order; the on-disk order is authoritative and **last-write-wins**.
//! - `compact_to(out_dir)` writes a new datawal log into `out_dir` containing
//!   exactly one `Put` per live key. The original log is untouched.
//! - `export_jsonl(out)` writes one JSON object per live key to `out` using
//!   `safeatomic_rs::write_atomic` (base64-encoded bytes).
//!
//! Out of scope for v0.1-pre:
//! - In-place `compact()` that swaps segments atomically.
//! - Transactions / atomic multi-record commits.
//! - Concurrent writers.
//! - Iteration over historical record versions.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use serde::Serialize;

use crate::format::RecordType;
use crate::record_log::{Record, RecordLog};

/// Stats reported by a successful compaction.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompactionStats {
    /// Number of live keys written to the output log.
    pub live_keys: u64,
    /// Number of records written. Equal to `live_keys` in v0.1-pre.
    pub records_written: u64,
    /// Total framed bytes written. Useful for sanity checks.
    pub bytes_written: u64,
}

/// Bytes-based key/value store backed by a [`RecordLog`].
#[derive(Debug)]
pub struct DataWal {
    log: RecordLog,
    /// In-memory live keydir. v0.1-pre stores the full payload alongside
    /// the key for simplicity; a future variant will switch to
    /// `HashMap<Vec<u8>, RecordRef>` and read by offset.
    map: HashMap<Vec<u8>, Vec<u8>>,
}

impl DataWal {
    /// Open (or create) a `DataWal` rooted at `dir`. Rebuilds the keydir
    /// by scanning the underlying record log in physical order.
    pub fn open(dir: &Path) -> Result<Self> {
        let mut log = RecordLog::open(dir)?;
        let records = log.scan()?;
        let map = rebuild_keydir(&records);
        Ok(Self { log, map })
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

    /// Returns `true` if `key` is currently live.
    pub fn contains_key(&self, key: &[u8]) -> bool {
        self.map.contains_key(key)
    }

    /// Returns a copy of the value for `key`, if live.
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self.map.get(key).cloned())
    }

    /// All live keys, unordered (HashMap iteration order).
    pub fn keys(&self) -> Vec<Vec<u8>> {
        self.map.keys().cloned().collect()
    }

    /// All live `(key, value)` pairs, unordered.
    pub fn items(&self) -> Vec<(Vec<u8>, Vec<u8>)> {
        self.map
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// Append a `Put` record and update the keydir.
    pub fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        self.log.append_record(RecordType::Put, key, value)?;
        self.map.insert(key.to_vec(), value.to_vec());
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
    pub fn compact_to(&self, out_dir: &Path) -> Result<CompactionStats> {
        // Refuse to clobber an existing non-empty directory or an existing
        // datawal log directory. v0.1-pre keeps this strict on purpose.
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

        let mut out_log = RecordLog::open(out_dir)?;
        let mut stats = CompactionStats::default();
        // Sort keys for deterministic output. Useful for diffing snapshots.
        let mut sorted: Vec<(&Vec<u8>, &Vec<u8>)> = self.map.iter().collect();
        sorted.sort_by(|a, b| a.0.cmp(b.0));
        for (k, v) in sorted {
            let r = out_log.append_record(RecordType::Put, k, v)?;
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
    pub fn export_jsonl(&self, out_path: &Path) -> Result<()> {
        #[derive(Serialize)]
        struct Row<'a> {
            key_b64: &'a str,
            value_b64: &'a str,
        }

        let mut sorted: Vec<(&Vec<u8>, &Vec<u8>)> = self.map.iter().collect();
        sorted.sort_by(|a, b| a.0.cmp(b.0));

        let mut buf = String::new();
        for (k, v) in sorted {
            let k_b64 = B64.encode(k);
            let v_b64 = B64.encode(v);
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

/// Replay a scanned record list into a live keydir.
///
/// Last-write-wins by physical order: later records (later segment, later
/// offset within the same segment) overwrite earlier ones.
fn rebuild_keydir(records: &[Record]) -> HashMap<Vec<u8>, Vec<u8>> {
    let mut map: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();
    for r in records {
        match r.record_type {
            RecordType::Put => {
                map.insert(r.key.clone(), r.payload.clone());
            }
            RecordType::Delete => {
                map.remove(&r.key);
            }
            RecordType::Raw => {
                // Raw records are not part of the KV projection. They are
                // valid in the log but invisible to DataWal.
            }
        }
    }
    map
}
