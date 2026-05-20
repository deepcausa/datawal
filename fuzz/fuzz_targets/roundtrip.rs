//! Roundtrip fuzz target for `DataWal`.
//!
//! Splits fuzz input into a (key, payload) pair within the documented
//! limits (`MAX_KEY_LEN = 64 KiB`, `MAX_PAYLOAD_LEN = 64 MiB`), then:
//!
//!   1. Opens a fresh `DataWal` in a tempdir.
//!   2. `put(key, payload)`.
//!   3. Asserts `get(key) == Some(payload.clone())`.
//!
//! Goal: demonstrate that the core stores **arbitrary byte payloads
//! within those limits** without interpretation, and recovers them
//! byte-for-byte. This is the empirical complement to the `Limits`
//! section in the top-level `README.md`.
//!
//! Tier: integration. Slower than `decode_frame` because it touches
//! the filesystem; faster than `scan_log` because it does not
//! exercise random bytes through the recovery path.
//!
//! Out of scope: multi-key sequences, concurrent open, compaction,
//! durability semantics. Those belong in crash-injection / soak
//! tests (issue #8), not in libFuzzer.

#![no_main]

use libfuzzer_sys::fuzz_target;

use datawal::{
    format::{MAX_KEY_LEN, MAX_PAYLOAD_LEN},
    DataWal,
};

// Cap each fuzz input so exec/sec stays reasonable. The decoder
// already handles full-size keys/payloads in unit tests; the goal
// here is breadth across shapes, not stress-testing the maximums.
const MAX_FUZZ_INPUT: usize = 256 * 1024;

// Minimum input we will actually exercise: 2 bytes to encode the
// split point, plus at least one byte of content.
const MIN_FUZZ_INPUT: usize = 3;

fuzz_target!(|data: &[u8]| {
    if data.len() < MIN_FUZZ_INPUT || data.len() > MAX_FUZZ_INPUT {
        return;
    }

    // First 2 bytes pick the key length (LE) inside the body.
    // Clamp to MAX_KEY_LEN; the rest becomes the payload.
    let body = &data[2..];
    let key_len_raw = u16::from_le_bytes([data[0], data[1]]) as usize;
    let key_len = key_len_raw.min(body.len()).min(MAX_KEY_LEN as usize);

    let key = &body[..key_len];
    let payload = &body[key_len..];

    if payload.len() > MAX_PAYLOAD_LEN as usize {
        return;
    }

    let dir = match tempfile::tempdir() {
        Ok(d) => d,
        Err(_) => return,
    };

    let mut kv = match DataWal::open(dir.path()) {
        Ok(k) => k,
        Err(_) => return,
    };

    // The contract under test: put then get returns the same bytes.
    // Any panic here is a real bug.
    if kv.put(key, payload).is_err() {
        return;
    }
    match kv.get(key) {
        Ok(Some(got)) => {
            assert_eq!(
                got.as_slice(),
                payload,
                "roundtrip mismatch for key_len={} payload_len={}",
                key.len(),
                payload.len(),
            );
        }
        Ok(None) => {
            panic!(
                "get returned None after put: key_len={} payload_len={}",
                key.len(),
                payload.len(),
            );
        }
        Err(_) => return,
    }

    drop(kv);
});
