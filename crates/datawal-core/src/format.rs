//! Wire format for datawal records (v0.1-pre).
//!
//! Each record is a self-contained framed unit:
//!
//! ```text
//! offset  size  field
//! ------  ----  ------------------------------------------
//!     0    4    magic        = b"DWAL"
//!     4    2    version      u16 LE = 1
//!     6    1    record_type  u8  (0=Raw, 1=Put, 2=Delete)
//!     7    1    flags        u8  reserved, must be 0
//!     8    8    txid         u64 LE
//!    16    4    key_len      u32 LE
//!    20    4    payload_len  u32 LE
//!    24    K    key          K = key_len bytes
//!  24+K    P    payload      P = payload_len bytes
//! 24+K+P   4    crc32c       u32 LE = CRC32C(bytes[0 .. 24+K+P])
//! ```
//!
//! Total wire size: `HEADER_LEN (24) + key_len + payload_len + CRC_LEN (4)`.
//!
//! Notes:
//! - CRC covers the 24-byte header AND key AND payload (everything before the
//!   CRC itself).
//! - `crc32fast` exposes IEEE/Castagnoli; v0.1-pre uses **`crc32fast::Hasher`**
//!   which is the standard CRC32 (IEEE/zlib polynomial). The on-disk field is
//!   named `crc32c` for forward compatibility but is computed by `crc32fast`
//!   for now. This is documented as a known v0.1-pre quirk; switching to true
//!   CRC32C is a follow-up that requires a wire version bump.
//! - All multi-byte integers are little-endian.

use std::io::Write;

use anyhow::{anyhow, bail, Result};

/// Magic bytes identifying a datawal record header.
pub const MAGIC: [u8; 4] = *b"DWAL";

/// On-disk wire format version. Bumped on any incompatible header change.
pub const WIRE_VERSION: u16 = 1;

/// Fixed-size record header (everything before key/payload/crc).
pub const HEADER_LEN: usize = 24;

/// CRC trailer length.
pub const CRC_LEN: usize = 4;

/// Hard cap on key length. v0.1-pre rejects anything larger before allocation.
pub const MAX_KEY_LEN: u32 = 64 * 1024; // 64 KiB

/// Hard cap on payload length. v0.1-pre rejects anything larger before allocation.
pub const MAX_PAYLOAD_LEN: u32 = 64 * 1024 * 1024; // 64 MiB

/// Type of a record. Encoded as a single byte in the header.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RecordType {
    /// Opaque record. Used by [`crate::RecordLog::append`].
    Raw = 0,
    /// Key/value PUT. Used by [`crate::DataWal::put`].
    Put = 1,
    /// Key DELETE (tombstone). Used by [`crate::DataWal::delete`].
    Delete = 2,
}

impl RecordType {
    /// Decode a record-type byte. Unknown bytes are an error (not silently skipped).
    pub fn from_byte(b: u8) -> Result<Self> {
        match b {
            0 => Ok(RecordType::Raw),
            1 => Ok(RecordType::Put),
            2 => Ok(RecordType::Delete),
            other => Err(anyhow!("datawal: unknown record_type byte {}", other)),
        }
    }

    /// Encode as the on-disk byte.
    pub fn as_byte(self) -> u8 {
        self as u8
    }
}

/// Total wire size of a record with the given key/payload lengths.
pub fn record_wire_size(key_len: u32, payload_len: u32) -> u64 {
    HEADER_LEN as u64 + key_len as u64 + payload_len as u64 + CRC_LEN as u64
}

/// Encode a single record into a freshly-allocated `Vec<u8>` ready to be
/// written to disk in one append.
///
/// Validates length limits and `flags` before allocating.
pub fn encode_record(
    record_type: RecordType,
    txid: u64,
    key: &[u8],
    payload: &[u8],
) -> Result<Vec<u8>> {
    if key.len() as u64 > MAX_KEY_LEN as u64 {
        bail!(
            "datawal: key_len {} exceeds MAX_KEY_LEN {}",
            key.len(),
            MAX_KEY_LEN
        );
    }
    if payload.len() as u64 > MAX_PAYLOAD_LEN as u64 {
        bail!(
            "datawal: payload_len {} exceeds MAX_PAYLOAD_LEN {}",
            payload.len(),
            MAX_PAYLOAD_LEN
        );
    }

    let key_len = key.len() as u32;
    let payload_len = payload.len() as u32;
    let total = record_wire_size(key_len, payload_len) as usize;
    let mut buf = Vec::with_capacity(total);

    // Header (24 bytes).
    buf.write_all(&MAGIC)?;
    buf.write_all(&WIRE_VERSION.to_le_bytes())?;
    buf.write_all(&[record_type.as_byte()])?;
    buf.write_all(&[0u8])?; // flags reserved
    buf.write_all(&txid.to_le_bytes())?;
    buf.write_all(&key_len.to_le_bytes())?;
    buf.write_all(&payload_len.to_le_bytes())?;

    // Body.
    buf.write_all(key)?;
    buf.write_all(payload)?;

    // CRC over everything we just wrote.
    let mut h = crc32fast::Hasher::new();
    h.update(&buf);
    let crc = h.finalize();
    buf.write_all(&crc.to_le_bytes())?;

    debug_assert_eq!(buf.len(), total);
    Ok(buf)
}

/// Outcome of attempting to decode the next record from a byte slice
/// starting at the given offset.
#[derive(Debug)]
pub enum DecodeOutcome {
    /// A complete, CRC-valid record was decoded. `bytes_consumed` is the
    /// total wire size of the record (header + key + payload + crc).
    Ok {
        record_type: RecordType,
        txid: u64,
        key: Vec<u8>,
        payload: Vec<u8>,
        bytes_consumed: u32,
    },
    /// The slice ended mid-record. Caller should treat as truncated tail
    /// if and only if this is the last segment, otherwise as corruption.
    Truncated {
        /// How many bytes from `offset` were available.
        available: u64,
        /// How many bytes were needed.
        needed: u64,
    },
    /// CRC mismatch on a fully-readable record. Caller decides whether to
    /// treat as truncated-tail (last segment) or hard corruption (mid-stream).
    CrcMismatch {
        /// Wire size of the bad record, so the caller can skip it if desired.
        bytes_consumed: u32,
    },
}

/// Hard errors that abort scanning regardless of position in the file.
#[derive(Debug)]
pub enum DecodeError {
    BadMagic { found: [u8; 4] },
    UnknownVersion { found: u16 },
    UnknownRecordType { found: u8 },
    ReservedFlagsSet { found: u8 },
    KeyTooLarge { found: u32 },
    PayloadTooLarge { found: u32 },
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::BadMagic { found } => {
                write!(f, "datawal: bad magic, expected DWAL, got {:?}", found)
            }
            DecodeError::UnknownVersion { found } => {
                write!(
                    f,
                    "datawal: unknown wire version {} (this build supports {})",
                    found, WIRE_VERSION
                )
            }
            DecodeError::UnknownRecordType { found } => {
                write!(f, "datawal: unknown record_type byte {}", found)
            }
            DecodeError::ReservedFlagsSet { found } => {
                write!(f, "datawal: reserved flags byte must be 0, got {}", found)
            }
            DecodeError::KeyTooLarge { found } => {
                write!(
                    f,
                    "datawal: key_len {} exceeds MAX_KEY_LEN {}",
                    found, MAX_KEY_LEN
                )
            }
            DecodeError::PayloadTooLarge { found } => {
                write!(
                    f,
                    "datawal: payload_len {} exceeds MAX_PAYLOAD_LEN {}",
                    found, MAX_PAYLOAD_LEN
                )
            }
        }
    }
}

impl std::error::Error for DecodeError {}

/// Try to decode one record starting at `buf[offset..]`.
///
/// Returns:
/// - `Ok(DecodeOutcome::Ok { .. })` on a complete CRC-valid record.
/// - `Ok(DecodeOutcome::Truncated { .. })` if `buf[offset..]` ended early.
/// - `Ok(DecodeOutcome::CrcMismatch { .. })` if the record was fully readable
///   but its CRC did not match.
/// - `Err(DecodeError::*)` for structural problems that should never be treated
///   as a recoverable tail (bad magic, unknown version, unknown type, oversize).
pub fn decode_next(buf: &[u8], offset: u64) -> std::result::Result<DecodeOutcome, DecodeError> {
    let off = offset as usize;
    if off + HEADER_LEN > buf.len() {
        return Ok(DecodeOutcome::Truncated {
            available: (buf.len().saturating_sub(off)) as u64,
            needed: HEADER_LEN as u64,
        });
    }

    let header = &buf[off..off + HEADER_LEN];

    // Magic.
    let mut magic = [0u8; 4];
    magic.copy_from_slice(&header[0..4]);
    if magic != MAGIC {
        return Err(DecodeError::BadMagic { found: magic });
    }

    // Version.
    let version = u16::from_le_bytes([header[4], header[5]]);
    if version != WIRE_VERSION {
        return Err(DecodeError::UnknownVersion { found: version });
    }

    // Record type.
    let rt_byte = header[6];
    let record_type = match rt_byte {
        0 | 1 | 2 => rt_byte,
        other => return Err(DecodeError::UnknownRecordType { found: other }),
    };

    // Flags.
    let flags = header[7];
    if flags != 0 {
        return Err(DecodeError::ReservedFlagsSet { found: flags });
    }

    // Txid.
    let txid = u64::from_le_bytes([
        header[8], header[9], header[10], header[11], header[12], header[13], header[14],
        header[15],
    ]);

    // Lengths.
    let key_len = u32::from_le_bytes([header[16], header[17], header[18], header[19]]);
    let payload_len = u32::from_le_bytes([header[20], header[21], header[22], header[23]]);
    if key_len > MAX_KEY_LEN {
        return Err(DecodeError::KeyTooLarge { found: key_len });
    }
    if payload_len > MAX_PAYLOAD_LEN {
        return Err(DecodeError::PayloadTooLarge { found: payload_len });
    }

    let total = record_wire_size(key_len, payload_len) as usize;
    if off + total > buf.len() {
        return Ok(DecodeOutcome::Truncated {
            available: (buf.len() - off) as u64,
            needed: total as u64,
        });
    }

    let key_start = off + HEADER_LEN;
    let payload_start = key_start + key_len as usize;
    let crc_start = payload_start + payload_len as usize;

    let crc_expected = u32::from_le_bytes([
        buf[crc_start],
        buf[crc_start + 1],
        buf[crc_start + 2],
        buf[crc_start + 3],
    ]);

    let mut h = crc32fast::Hasher::new();
    h.update(&buf[off..crc_start]);
    let crc_actual = h.finalize();
    if crc_actual != crc_expected {
        return Ok(DecodeOutcome::CrcMismatch {
            bytes_consumed: total as u32,
        });
    }

    Ok(DecodeOutcome::Ok {
        record_type: RecordType::from_byte(record_type).expect("validated above"),
        txid,
        key: buf[key_start..payload_start].to_vec(),
        payload: buf[payload_start..crc_start].to_vec(),
        bytes_consumed: total as u32,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_raw_empty_key() {
        let buf = encode_record(RecordType::Raw, 7, b"", b"hello").unwrap();
        let out = decode_next(&buf, 0).unwrap();
        match out {
            DecodeOutcome::Ok {
                record_type,
                txid,
                key,
                payload,
                bytes_consumed,
            } => {
                assert_eq!(record_type, RecordType::Raw);
                assert_eq!(txid, 7);
                assert!(key.is_empty());
                assert_eq!(payload, b"hello");
                assert_eq!(bytes_consumed as usize, buf.len());
            }
            _ => panic!("expected Ok, got {:?}", out),
        }
    }

    #[test]
    fn roundtrip_put_with_key() {
        let buf = encode_record(RecordType::Put, 42, b"alpha", b"value-1").unwrap();
        let out = decode_next(&buf, 0).unwrap();
        match out {
            DecodeOutcome::Ok {
                record_type,
                txid,
                key,
                payload,
                ..
            } => {
                assert_eq!(record_type, RecordType::Put);
                assert_eq!(txid, 42);
                assert_eq!(key, b"alpha");
                assert_eq!(payload, b"value-1");
            }
            _ => panic!("expected Ok"),
        }
    }

    #[test]
    fn truncated_header_reports_needed() {
        let buf = encode_record(RecordType::Raw, 1, b"", b"x").unwrap();
        let short = &buf[..10];
        match decode_next(short, 0).unwrap() {
            DecodeOutcome::Truncated { available, needed } => {
                assert_eq!(available, 10);
                assert_eq!(needed, HEADER_LEN as u64);
            }
            other => panic!("expected Truncated, got {:?}", other),
        }
    }

    #[test]
    fn truncated_body_reports_total() {
        let buf = encode_record(RecordType::Raw, 1, b"", b"hello").unwrap();
        let short = &buf[..HEADER_LEN + 2];
        match decode_next(short, 0).unwrap() {
            DecodeOutcome::Truncated { needed, .. } => {
                assert_eq!(needed, buf.len() as u64);
            }
            other => panic!("expected Truncated, got {:?}", other),
        }
    }

    #[test]
    fn crc_mismatch_detected() {
        let mut buf = encode_record(RecordType::Raw, 1, b"", b"hello").unwrap();
        // Corrupt one byte of the payload, leaving lengths intact.
        let n = buf.len();
        buf[n - 5] ^= 0xff;
        match decode_next(&buf, 0).unwrap() {
            DecodeOutcome::CrcMismatch { bytes_consumed } => {
                assert_eq!(bytes_consumed as usize, n);
            }
            other => panic!("expected CrcMismatch, got {:?}", other),
        }
    }

    #[test]
    fn bad_magic_is_hard_error() {
        let mut buf = encode_record(RecordType::Raw, 1, b"", b"x").unwrap();
        buf[0] = b'X';
        match decode_next(&buf, 0) {
            Err(DecodeError::BadMagic { .. }) => {}
            other => panic!("expected BadMagic, got {:?}", other),
        }
    }

    #[test]
    fn unknown_version_is_hard_error() {
        let mut buf = encode_record(RecordType::Raw, 1, b"", b"x").unwrap();
        buf[4] = 99;
        buf[5] = 0;
        match decode_next(&buf, 0) {
            Err(DecodeError::UnknownVersion { found: 99 }) => {}
            other => panic!("expected UnknownVersion, got {:?}", other),
        }
    }

    #[test]
    fn unknown_record_type_is_hard_error() {
        let mut buf = encode_record(RecordType::Raw, 1, b"", b"x").unwrap();
        buf[6] = 200;
        match decode_next(&buf, 0) {
            Err(DecodeError::UnknownRecordType { found: 200 }) => {}
            other => panic!("expected UnknownRecordType, got {:?}", other),
        }
    }

    #[test]
    fn reserved_flags_must_be_zero() {
        let mut buf = encode_record(RecordType::Raw, 1, b"", b"x").unwrap();
        buf[7] = 1;
        match decode_next(&buf, 0) {
            Err(DecodeError::ReservedFlagsSet { found: 1 }) => {}
            other => panic!("expected ReservedFlagsSet, got {:?}", other),
        }
    }

    #[test]
    fn encode_rejects_oversize_key() {
        let big = vec![0u8; (MAX_KEY_LEN as usize) + 1];
        let err = encode_record(RecordType::Put, 1, &big, b"").unwrap_err();
        assert!(format!("{err}").contains("MAX_KEY_LEN"));
    }

    #[test]
    fn encode_rejects_oversize_payload() {
        // Allocate carefully — keep it small to avoid OOM in CI: build a fake
        // buffer of MAX_PAYLOAD_LEN+1 length via Vec::with_capacity is fine.
        let big = vec![0u8; (MAX_PAYLOAD_LEN as usize) + 1];
        let err = encode_record(RecordType::Raw, 1, b"", &big).unwrap_err();
        assert!(format!("{err}").contains("MAX_PAYLOAD_LEN"));
    }
}
