//! The next slice wires this module into font decoding; until then nothing
//! constructs it.
#![allow(dead_code)]

//! Bounded ToUnicode CMap decoding: character codes to Unicode. Glyph programs are never read.
mod parse;
mod ranges;
#[cfg(test)]
mod tests;
mod utf16;
use std::collections::BTreeMap;
pub use utf16::code;
/// Mappings accepted from one CMap.
pub const MAX_ENTRIES: usize = 65_536;
/// Codespace segments accepted from one CMap.
pub const MAX_RANGES: usize = 64;
/// Bytes accepted in one destination string.
pub const MAX_TARGET: usize = 512;
/// Destination bytes accepted across one CMap, charged with a per-mapping overhead.
pub const MAX_BYTES: usize = 1024 * 1024;
/// Code widths a codespace range may declare.
pub const MAX_WIDTH: usize = 4;
const MAX_OPERANDS: usize = 64;
/// Charged for every mapping in addition to its destination bytes.
const ENTRY_BYTES: usize = 8;
/// One ToUnicode CMap: character codes of one fixed width and their Unicode destinations; keys are
/// character codes, not CIDs, and a code absent from the table has no Unicode meaning here.
pub struct CMap {
    width: usize,
    targets: BTreeMap<u32, Box<str>>,
}
impl CMap {
    /// Code width in bytes; every code of this CMap has exactly this width.
    pub fn width(&self) -> usize {
        self.width
    }
    /// Unicode destination of one character code, when the CMap maps it.
    pub fn target(&self, code: u32) -> Option<&str> {
        self.targets.get(&code).map(Box::as_ref)
    }
    /// Mappings stored in this CMap; the decoder reads mappings, never their count.
    #[cfg(test)]
    pub fn entries(&self) -> usize {
        self.targets.len()
    }
}
