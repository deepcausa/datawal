//! Human-readable rendering of opaque byte strings.
//!
//! `datawal` keys and payloads are `Vec<u8>`. The wire format makes no
//! claims about their encoding. For machine output (`--json`), the CLI
//! emits base64 unconditionally — that is the contract of the
//! `datawal.cli.v1` schema.
//!
//! For human output, base64 in the common case (UTF-8 / ASCII text) is
//! hostile. This module renders bytes with a small auto-detect:
//!
//! * Printable ASCII (`0x20..=0x7E`) + tab (`0x09`) → literal text,
//!   quoted if it contains whitespace, quotes, or backslashes that
//!   would otherwise blur the field boundary.
//! * Anything else → an explicit prefixed encoding (`b64:` or `hex:`)
//!   so the reader sees that it is encoded and which encoding.
//!
//! The caller selects the encoding explicitly with [`BytesMode`]. With
//! [`BytesMode::Auto`] (the default) the renderer picks `literal` when
//! the bytes are printable and `b64:` otherwise.
//!
//! Long payloads are truncated by default to keep output legible; pass
//! `truncate = None` to emit in full.

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};

/// How to render bytes in human form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BytesMode {
    /// Auto-detect: literal if printable ASCII, `b64:` otherwise.
    #[default]
    Auto,
    /// Always print the literal bytes (quoted as needed). Falls back
    /// to `b64:` when the bytes are not printable, since printing raw
    /// control bytes to a terminal is hostile.
    Raw,
    /// Always render as `b64:<standard base64>`.
    Base64,
    /// Always render as `hex:<lowercase hex>`.
    Hex,
}

impl std::str::FromStr for BytesMode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "auto" => Ok(BytesMode::Auto),
            "raw" => Ok(BytesMode::Raw),
            "base64" | "b64" => Ok(BytesMode::Base64),
            "hex" => Ok(BytesMode::Hex),
            other => Err(format!(
                "unknown --bytes mode `{other}` (want auto|raw|base64|hex)"
            )),
        }
    }
}

/// Default truncation point in bytes (only applies to the rendered
/// string for `scan` / `dump`; `get` honours `--no-truncate` directly).
pub const DEFAULT_TRUNCATE_BYTES: usize = 64;

/// Render `bytes` for human display.
///
/// `truncate = Some(n)` clips the rendered output and appends `...`
/// (the byte budget is approximate: `n` source bytes, not output
/// characters). `truncate = None` emits in full.
pub fn render_for_human(bytes: &[u8], mode: BytesMode, truncate: Option<usize>) -> String {
    match mode {
        BytesMode::Auto => {
            if is_printable_ascii(bytes) {
                literal(bytes, truncate)
            } else {
                prefixed_b64(bytes, truncate)
            }
        }
        BytesMode::Raw => {
            if is_printable_ascii(bytes) {
                literal(bytes, truncate)
            } else {
                // Hostile to terminals: don't actually emit control bytes.
                prefixed_b64(bytes, truncate)
            }
        }
        BytesMode::Base64 => prefixed_b64(bytes, truncate),
        BytesMode::Hex => prefixed_hex(bytes, truncate),
    }
}

/// Same as [`render_for_human`] but for the `get` subcommand on a hit:
/// when the value is printable ASCII (auto mode), emit raw bytes with
/// no quoting and no prefix — the caller is consuming the value as
/// shell data, not a debugging line. Binary values fall back to a
/// human hint rather than printing garbage to the terminal.
///
/// Returns `Ok(rendered)` when the value can be printed, or
/// `Err(hint)` when binary in `Auto` mode (caller should print the
/// hint to stderr and the byte-length / encoding suggestion).
pub fn render_value_for_get(
    bytes: &[u8],
    mode: BytesMode,
    truncate: Option<usize>,
) -> Result<String, String> {
    match mode {
        BytesMode::Auto => {
            if is_printable_ascii(bytes) {
                Ok(String::from_utf8_lossy(slice_truncated(bytes, truncate)).into_owned())
            } else {
                Err(format!(
                    "<binary value, {} bytes; use --bytes base64 or --bytes hex>",
                    bytes.len()
                ))
            }
        }
        BytesMode::Raw => {
            if is_printable_ascii(bytes) {
                Ok(String::from_utf8_lossy(slice_truncated(bytes, truncate)).into_owned())
            } else {
                Err(format!(
                    "<binary value, {} bytes; use --bytes base64 or --bytes hex>",
                    bytes.len()
                ))
            }
        }
        BytesMode::Base64 => Ok(B64.encode(slice_truncated(bytes, truncate))),
        BytesMode::Hex => Ok(to_hex(slice_truncated(bytes, truncate))),
    }
}

/// True iff every byte is ASCII printable (`0x20..=0x7E`) or `\t`.
/// Empty slices are considered printable (literal "" is a fine
/// rendering of the empty key/payload).
pub fn is_printable_ascii(bytes: &[u8]) -> bool {
    bytes
        .iter()
        .all(|&b| (0x20..=0x7E).contains(&b) || b == b'\t')
}

fn slice_truncated(bytes: &[u8], truncate: Option<usize>) -> &[u8] {
    match truncate {
        Some(n) if bytes.len() > n => &bytes[..n],
        _ => bytes,
    }
}

fn literal(bytes: &[u8], truncate: Option<usize>) -> String {
    let head = slice_truncated(bytes, truncate);
    let s = String::from_utf8_lossy(head);
    let needs_quoting = s
        .chars()
        .any(|c| c == ' ' || c == '\t' || c == '"' || c == '\\')
        || s.is_empty();
    let body = if needs_quoting {
        let mut out = String::with_capacity(head.len() + 2);
        out.push('"');
        for c in s.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\t' => out.push_str("\\t"),
                other => out.push(other),
            }
        }
        out.push('"');
        out
    } else {
        s.into_owned()
    };
    if truncate.is_some_and(|n| bytes.len() > n) {
        format!("{body}...")
    } else {
        body
    }
}

fn prefixed_b64(bytes: &[u8], truncate: Option<usize>) -> String {
    let head = slice_truncated(bytes, truncate);
    let s = B64.encode(head);
    if truncate.is_some_and(|n| bytes.len() > n) {
        format!("b64:{s}...")
    } else {
        format!("b64:{s}")
    }
}

fn prefixed_hex(bytes: &[u8], truncate: Option<usize>) -> String {
    let head = slice_truncated(bytes, truncate);
    let s = to_hex(head);
    if truncate.is_some_and(|n| bytes.len() > n) {
        format!("hex:{s}...")
    } else {
        format!("hex:{s}")
    }
}

fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0F) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_literal_for_ascii() {
        assert_eq!(render_for_human(b"alpha", BytesMode::Auto, None), "alpha");
        assert_eq!(
            render_for_human(b"a/b-c.d", BytesMode::Auto, None),
            "a/b-c.d"
        );
    }

    #[test]
    fn auto_quotes_when_space_or_quote() {
        assert_eq!(
            render_for_human(b"foo bar", BytesMode::Auto, None),
            "\"foo bar\""
        );
        assert_eq!(
            render_for_human(b"a\"b", BytesMode::Auto, None),
            "\"a\\\"b\""
        );
        assert_eq!(
            render_for_human(b"x\\y", BytesMode::Auto, None),
            "\"x\\\\y\""
        );
        assert_eq!(render_for_human(b"\tt", BytesMode::Auto, None), "\"\\tt\"");
    }

    #[test]
    fn auto_quotes_empty_as_quoted_empty_string() {
        assert_eq!(render_for_human(b"", BytesMode::Auto, None), "\"\"");
    }

    #[test]
    fn auto_falls_back_to_b64_for_binary() {
        let bytes = &[0u8, 1, 2, 3, 0xFF];
        let s = render_for_human(bytes, BytesMode::Auto, None);
        assert!(s.starts_with("b64:"), "expected b64 prefix, got `{s}`");
    }

    #[test]
    fn forced_base64_always_emits_b64_prefix() {
        let s = render_for_human(b"alpha", BytesMode::Base64, None);
        assert_eq!(s, "b64:YWxwaGE=");
    }

    #[test]
    fn forced_hex_emits_hex_prefix() {
        let s = render_for_human(b"\xDE\xAD\xBE\xEF", BytesMode::Hex, None);
        assert_eq!(s, "hex:deadbeef");
    }

    #[test]
    fn raw_falls_back_to_b64_for_binary() {
        let bytes = &[0u8, 1, 2];
        let s = render_for_human(bytes, BytesMode::Raw, None);
        assert!(s.starts_with("b64:"));
    }

    #[test]
    fn truncate_appends_ellipsis() {
        let bytes = b"abcdefghij"; // 10 bytes
        let s = render_for_human(bytes, BytesMode::Auto, Some(4));
        assert_eq!(s, "abcd...");
    }

    #[test]
    fn truncate_appends_ellipsis_for_b64() {
        let bytes = &[0u8; 10];
        let s = render_for_human(bytes, BytesMode::Auto, Some(3));
        // 3 bytes of zero == "AAAA" in base64
        assert_eq!(s, "b64:AAAA...");
    }

    #[test]
    fn get_value_auto_emits_literal_for_text() {
        let r = render_value_for_get(b"hello", BytesMode::Auto, None).unwrap();
        assert_eq!(r, "hello");
    }

    #[test]
    fn get_value_auto_hints_for_binary() {
        let err = render_value_for_get(b"\x00\x01\x02", BytesMode::Auto, None).unwrap_err();
        assert!(err.contains("binary value, 3 bytes"));
        assert!(err.contains("--bytes base64"));
    }

    #[test]
    fn get_value_forced_base64_emits_unprefixed_b64() {
        // For `get` we emit unprefixed bytes (shell-friendly).
        let r = render_value_for_get(b"hello", BytesMode::Base64, None).unwrap();
        assert_eq!(r, "aGVsbG8=");
    }

    #[test]
    fn get_value_forced_hex_emits_unprefixed_hex() {
        let r = render_value_for_get(b"\xDE\xAD", BytesMode::Hex, None).unwrap();
        assert_eq!(r, "dead");
    }

    #[test]
    fn mode_parses_from_str() {
        use std::str::FromStr;
        assert_eq!(BytesMode::from_str("auto").unwrap(), BytesMode::Auto);
        assert_eq!(BytesMode::from_str("raw").unwrap(), BytesMode::Raw);
        assert_eq!(BytesMode::from_str("base64").unwrap(), BytesMode::Base64);
        assert_eq!(BytesMode::from_str("b64").unwrap(), BytesMode::Base64);
        assert_eq!(BytesMode::from_str("hex").unwrap(), BytesMode::Hex);
        assert!(BytesMode::from_str("garbage").is_err());
    }
}
