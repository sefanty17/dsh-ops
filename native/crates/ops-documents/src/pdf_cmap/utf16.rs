//! Character-code packing and UTF-16BE destination decoding for one CMap.
use super::MAX_TARGET;
use crate::pdf_syntax::{bad, limit};
use ops_protocol::Result;
/// Big-endian character code of at most four bytes; the split and the CMap keys share it.
pub fn code(bytes: &[u8]) -> u32 {
    bytes
        .iter()
        .fold(0, |value, byte| value << 8 | u32::from(*byte))
}
/// Decode one destination string: UTF-16BE code units with surrogate pairs combined; U+0000 is a
/// legal destination, which producers use for unused codes.
pub(super) fn utf16(bytes: &[u8]) -> Result<String> {
    if bytes.is_empty() {
        return Err(bad("PDF ToUnicode destination is empty"));
    }
    if bytes.len() > MAX_TARGET {
        return Err(limit("PDF ToUnicode destination exceeds 512 bytes"));
    }
    if !bytes.len().is_multiple_of(2) {
        return Err(bad(
            "PDF ToUnicode destination has an odd UTF-16BE length",
        ));
    }
    let mut output = String::new();
    let mut at = 0;
    while at < bytes.len() {
        let unit = u16::from_be_bytes([bytes[at], bytes[at + 1]]);
        at += 2;
        let scalar = if (0xd800..=0xdbff).contains(&unit) {
            if at >= bytes.len() {
                return Err(bad("PDF ToUnicode destination ends with a high surrogate"));
            }
            let low = u16::from_be_bytes([bytes[at], bytes[at + 1]]);
            at += 2;
            if !(0xdc00..=0xdfff).contains(&low) {
                return Err(bad("PDF ToUnicode destination has an invalid surrogate pair"));
            }
            char::from_u32(0x10000 + ((u32::from(unit) - 0xd800) << 10) + u32::from(low) - 0xdc00)
        } else {
            char::from_u32(u32::from(unit))
        };
        output.push(
            scalar.ok_or_else(|| bad("PDF ToUnicode destination has an unpaired surrogate"))?,
        );
    }
    Ok(output)
}
