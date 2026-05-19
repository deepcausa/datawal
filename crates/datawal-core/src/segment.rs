//! Segment-file naming and discovery.
//!
//! Segment files live directly under the log directory:
//!
//! ```text
//! path/00000001.dwal
//! path/00000002.dwal
//! ```
//!
//! Naming: zero-padded 8-digit decimal id, suffix `.dwal`. Lexicographic
//! sort is therefore identical to numeric sort.
//!
//! v0.1-pre layout has **no MANIFEST**. The set of segments is whatever
//! matches the pattern on disk; the active segment is the one with the
//! highest id.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

/// File extension for segment files.
pub const SEGMENT_EXT: &str = "dwal";

/// Width of the zero-padded decimal id in segment filenames.
pub const SEGMENT_ID_WIDTH: usize = 8;

/// Build the path of a segment with the given numeric id in `dir`.
pub fn segment_path(dir: &Path, id: u32) -> PathBuf {
    dir.join(format!(
        "{:0width$}.{ext}",
        id,
        width = SEGMENT_ID_WIDTH,
        ext = SEGMENT_EXT,
    ))
}

/// Parse a filename like `"00000007.dwal"` into its numeric id.
///
/// Returns `None` for anything that does not match the exact pattern
/// (wrong width, wrong extension, non-decimal digits).
pub fn parse_segment_filename(name: &str) -> Option<u32> {
    let stripped = name.strip_suffix(&format!(".{SEGMENT_EXT}"))?;
    if stripped.len() != SEGMENT_ID_WIDTH {
        return None;
    }
    if !stripped.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    stripped.parse::<u32>().ok()
}

/// Discover all segment ids in `dir`, sorted ascending.
///
/// Ignores files that do not match `[0-9]{8}\.dwal` exactly. Subdirectories
/// are ignored.
pub fn list_segment_ids(dir: &Path) -> Result<Vec<u32>> {
    let mut ids: Vec<u32> = Vec::new();
    let rd = fs::read_dir(dir).with_context(|| format!("datawal: read_dir {}", dir.display()))?;
    for entry in rd {
        let entry = entry?;
        let ft = entry.file_type()?;
        if !ft.is_file() {
            continue;
        }
        let name_os = entry.file_name();
        let Some(name) = name_os.to_str() else {
            continue;
        };
        if let Some(id) = parse_segment_filename(name) {
            ids.push(id);
        }
    }
    ids.sort_unstable();
    Ok(ids)
}

/// Return the id of the active (highest) segment, or `None` if the directory
/// has no segments at all.
pub fn active_segment_id(dir: &Path) -> Result<Option<u32>> {
    Ok(list_segment_ids(dir)?.last().copied())
}

/// Compute the next segment id given an existing list. v0.1-pre starts at 1.
pub fn next_segment_id(existing: &[u32]) -> Result<u32> {
    match existing.last() {
        None => Ok(1),
        Some(&n) => n.checked_add(1).ok_or_else(|| {
            anyhow::anyhow!("datawal: segment id overflow at {} (u32 exhausted)", n)
        }),
    }
}

/// Strict size of a segment file on disk. Returns 0 if the file does not
/// exist yet (callers usually want that to mean "empty").
pub fn segment_size(dir: &Path, id: u32) -> Result<u64> {
    let p = segment_path(dir, id);
    match fs::metadata(&p) {
        Ok(m) => Ok(m.len()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(e) => bail!("datawal: stat segment {} failed: {}", p.display(), e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn segment_path_zero_pads() {
        let p = segment_path(Path::new("/tmp/log"), 7);
        assert_eq!(p, Path::new("/tmp/log/00000007.dwal"));
    }

    #[test]
    fn parse_round_trip() {
        assert_eq!(parse_segment_filename("00000001.dwal"), Some(1));
        assert_eq!(parse_segment_filename("12345678.dwal"), Some(12345678));
    }

    #[test]
    fn parse_rejects_bad() {
        assert_eq!(parse_segment_filename("1.dwal"), None);
        assert_eq!(parse_segment_filename("00000001.txt"), None);
        assert_eq!(parse_segment_filename("0000000a.dwal"), None);
        assert_eq!(parse_segment_filename("000000001.dwal"), None);
    }

    #[test]
    fn list_and_active() {
        let td = TempDir::new().unwrap();
        let d = td.path();
        fs::write(d.join("00000001.dwal"), b"").unwrap();
        fs::write(d.join("00000003.dwal"), b"").unwrap();
        fs::write(d.join("garbage.txt"), b"").unwrap();
        let ids = list_segment_ids(d).unwrap();
        assert_eq!(ids, vec![1, 3]);
        assert_eq!(active_segment_id(d).unwrap(), Some(3));
    }

    #[test]
    fn next_id_starts_at_one() {
        assert_eq!(next_segment_id(&[]).unwrap(), 1);
        assert_eq!(next_segment_id(&[1, 2, 5]).unwrap(), 6);
    }
}
