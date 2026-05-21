//! Populate a small datawal store at the path given on argv.
//!
//! Used by `examples/cli_read_smoke.sh` to seed a deterministic store
//! the inspector can be exercised against. Writes a handful of `Put`
//! records, fsyncs and exits.

use std::path::PathBuf;

use anyhow::{anyhow, Result};
use datawal::DataWal;

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let dir: PathBuf = args
        .next()
        .ok_or_else(|| anyhow!("usage: populate_smoke_store <store-dir>"))?
        .into();

    let mut wal = DataWal::open(&dir)?;
    wal.put(b"alpha", b"1")?;
    wal.put(b"beta", b"22")?;
    wal.put(b"gamma", b"333")?;
    wal.put(b"delta", b"4444")?;
    wal.fsync()?;

    println!("populated {} with 4 Put records", dir.display());
    Ok(())
}
