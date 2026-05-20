//! Fuzz the wire-format decoder directly.
//!
//! Feeds arbitrary bytes to `datawal::format::decode_next` at offset 0
//! and asserts the decoder's contract holds for every input:
//!
//! - never panics (libFuzzer catches abort/UB regardless, this is the
//!   primary invariant)
//! - on `Ok(DecodeOutcome::Ok { bytes_consumed, .. })`, the consumed
//!   range stays within the buffer
//! - on `Ok(DecodeOutcome::Ok { key, payload, .. })`, both lengths
//!   respect the static caps `MAX_KEY_LEN` and `MAX_PAYLOAD_LEN`
//! - `DecodeOutcome::Truncated { available, needed }` reports
//!   `available <= needed`
//!
//! This is the *primary* target. It is cheap (no I/O) so libFuzzer can
//! reach high exec/s.

#![no_main]

use libfuzzer_sys::fuzz_target;

use datawal::format::{decode_next, DecodeOutcome, MAX_KEY_LEN, MAX_PAYLOAD_LEN};

fuzz_target!(|data: &[u8]| {
    match decode_next(data, 0) {
        Ok(DecodeOutcome::Ok {
            key,
            payload,
            bytes_consumed,
            ..
        }) => {
            // The decoder must not claim to have consumed more than it was given.
            assert!(
                (bytes_consumed as usize) <= data.len(),
                "decode_next claimed bytes_consumed={} > data.len()={}",
                bytes_consumed,
                data.len()
            );
            // Key / payload must respect the static limits.
            assert!(
                key.len() as u64 <= MAX_KEY_LEN as u64,
                "decoded key.len()={} exceeds MAX_KEY_LEN={}",
                key.len(),
                MAX_KEY_LEN
            );
            assert!(
                payload.len() as u64 <= MAX_PAYLOAD_LEN as u64,
                "decoded payload.len()={} exceeds MAX_PAYLOAD_LEN={}",
                payload.len(),
                MAX_PAYLOAD_LEN
            );
        }
        Ok(DecodeOutcome::Truncated { available, needed }) => {
            // Truncation is only meaningful if we asked for more than we had.
            assert!(
                available <= needed,
                "Truncated reports available={} > needed={}",
                available,
                needed
            );
        }
        Ok(DecodeOutcome::CrcMismatch { bytes_consumed }) => {
            assert!(
                (bytes_consumed as usize) <= data.len(),
                "CrcMismatch claimed bytes_consumed={} > data.len()={}",
                bytes_consumed,
                data.len()
            );
        }
        Err(_) => {
            // Hard structural errors are part of the contract; nothing to assert.
        }
    }
});
