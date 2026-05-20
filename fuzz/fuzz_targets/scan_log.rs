//! Fuzz the full open + recovery path with arbitrary segment bytes.
//!
//! This is the *integration smoke* target. It is slower than
//! `decode_frame` because each iteration touches the filesystem:
//!
//! 1. write the fuzzer input to a fresh tempdir as `00000001.dwal`
//! 2. call `RecordLog::open(dir)` (which runs full recovery scan)
//! 3. ask for the `RecoveryReport`
//! 4. drop the log (releases the cooperative `fs2` lock)
//! 5. drop the tempdir (cleanup)
//!
//! The contract being exercised:
//!
//! - `RecordLog::open` must never panic on arbitrary segment bytes;
//!   it returns `Err` for hard structural problems (sealed-segment
//!   CRC mismatch, etc.) and `Ok` with a `RecoveryReport` describing
//!   any truncated tail otherwise.
//! - `recovery_report` must be consistent with the bytes consumed.
//!
//! libFuzzer can drive this at hundreds of execs/sec on a quiet
//! machine, which is enough to find allocation explosions, infinite
//! loops, or panics that the pure-decoder target would miss because
//! they live in the segment-walking glue rather than `decode_next`
//! itself.
//!
//! Inputs larger than 256 KiB are skipped: above that the iteration
//! cost dominates and libFuzzer's mutator no longer learns anything
//! useful. The static `MAX_PAYLOAD_LEN` (64 MiB) is enforced by the
//! decoder, so refusing huge inputs here does not weaken coverage of
//! the size-limit checks.

#![no_main]

use std::fs;

use libfuzzer_sys::fuzz_target;
use tempfile::TempDir;

use datawal::RecordLog;

/// Cap on a single fuzz input. Inputs beyond this are dropped so
/// libFuzzer stays in the regime where mutation matters.
const MAX_FUZZ_INPUT: usize = 256 * 1024;

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_FUZZ_INPUT {
        return;
    }

    // Fresh tempdir per iteration. Dropping it at the end of the
    // closure cleans up the directory; the lock is released when the
    // `RecordLog` is dropped before that.
    let Ok(dir) = TempDir::new() else {
        return;
    };

    let seg_path = dir.path().join("00000001.dwal");
    if fs::write(&seg_path, data).is_err() {
        return;
    }

    // The contract we are fuzzing: open() must not panic on arbitrary
    // bytes. Whether it succeeds or returns Err is fine.
    if let Ok(log) = RecordLog::open(dir.path()) {
        // recovery_report must succeed once open() succeeded, and the
        // tail-truncation byte count must not exceed the file size.
        if let Ok(report) = log.recovery_report() {
            let on_disk = data.len() as u64;
            assert!(
                report.tail_bytes_discarded <= on_disk,
                "RecoveryReport.tail_bytes_discarded={} > segment bytes={}",
                report.tail_bytes_discarded,
                on_disk
            );
        }
        // `log` drops here, releasing the cooperative lock.
    }
});
