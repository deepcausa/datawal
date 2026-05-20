//! Shared helpers for benches.
//!
//! Compiled per-bench (each bench file `include!`s this module via
//! `mod common;`). Keep this small and dependency-free — anything
//! interesting belongs in the crate itself.

#![allow(dead_code)]

use std::env;
use std::path::PathBuf;

use tempfile::TempDir;

/// Honours `DATAWAL_BENCH_DIR` if set, otherwise falls back to the
/// system temp dir.
///
/// fsync-sensitive benches should set `DATAWAL_BENCH_DIR=/mnt/nvme/...`
/// to point at a real local disk. The default location is typically
/// `/tmp`, which on Linux is often tmpfs — fsync there is essentially
/// a no-op and numbers will be misleadingly fast.
pub fn bench_tempdir() -> TempDir {
    match env::var_os("DATAWAL_BENCH_DIR") {
        Some(parent) => {
            let parent = PathBuf::from(parent);
            std::fs::create_dir_all(&parent).expect("create DATAWAL_BENCH_DIR");
            TempDir::new_in(parent).expect("tempdir in DATAWAL_BENCH_DIR")
        }
        None => TempDir::new().expect("tempdir"),
    }
}

/// A deterministic byte payload of the requested size. Cheap to build;
/// not constant so callers always get a fresh allocation when needed.
pub fn payload(size: usize) -> Vec<u8> {
    vec![0xABu8; size]
}
