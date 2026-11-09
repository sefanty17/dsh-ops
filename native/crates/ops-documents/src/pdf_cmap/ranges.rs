//! The collected mapping table, validated against the codespace once the stream ends.
use super::{
    CMap, ENTRY_BYTES, MAX_BYTES, MAX_ENTRIES, MAX_RANGES, MAX_WIDTH, code, utf16::utf16,
};
use crate::pdf_syntax::{Value, bad, limit, unsupported};
use ops_protocol::Result;
use std::collections::BTreeMap;
/// One collected mapping: its code width, its code, and the Unicode destination text.
pub(super) struct Mapping {
    width: usize,
    code: u32,
    destination: Box<str>,
}
/// A CMap collected while the stream is read and validated after it ends; a mapping may precede
/// the codespace range that fixes its width, so collection applies budgets but compares no width.
#[derive(Default)]
pub(super) struct Table {
    width: usize,
    ranges: usize,
    pub(super) mappings: Vec<Mapping>,
    bytes: usize,
}
impl Table {
    pub(super) fn codespace(&mut self, low: &Value, high: &Value) -> Result<()> {
        let (Value::String(low), Value::String(high)) = (low, high) else {
            return Err(bad(
                "PDF ToUnicode codespace range must be two hexadecimal strings",
            ));
        };
        if low.len() != high.len() || !(1..=MAX_WIDTH).contains(&low.len()) {
            return Err(bad(
                "PDF ToUnicode codespace range must pair equal-width codes of 1-4 bytes",
            ));
        }
        if low > high {
            return Err(bad("PDF ToUnicode codespace range is inverted"));
        }
        if self.ranges >= MAX_RANGES {
            return Err(limit("PDF ToUnicode CMap exceeds 64 codespace ranges"));
        }
        if self.width == 0 {
            self.width = low.len();
        } else if self.width != low.len() {
            return Err(unsupported(
                "PDF ToUnicode codespace mixes code widths; mixed-width codespaces are \
                 unsupported because a code cannot be split into characters",
            ));
        }
        self.ranges += 1;
        Ok(())
    }
    pub(super) fn character(&mut self, source: &Value, target: &Value) -> Result<()> {
        let (Value::String(source), Value::String(target)) = (source, target) else {
            return Err(bad(
                "PDF ToUnicode bfchar entry must be two hexadecimal strings",
            ));
        };
        self.push(source.len(), code(source), utf16(target)?)
    }
    pub(super) fn range(&mut self, low: &Value, high: &Value, target: &Value) -> Result<()> {
        let (Value::String(low), Value::String(high)) = (low, high) else {
            return Err(bad("PDF ToUnicode bfrange must start with two bounds"));
        };
        // Both bounds are codes of the one codespace width, so their widths must agree.
        if low.len() != high.len() {
            return Err(bad(
                "PDF ToUnicode bfrange bounds must be codes of equal width",
            ));
        }
        let width = low.len();
        if low > high {
            return Err(bad("PDF ToUnicode bfrange is inverted"));
        }
        let start = code(low);
        let count = u64::from(code(high)) - u64::from(start) + 1;
        if self.mappings.len() as u64 + count > MAX_ENTRIES as u64 {
            return Err(limit("PDF ToUnicode CMap exceeds 65536 mappings"));
        }
        match target {
            // A scalar destination increments its last byte, which cannot carry.
            Value::String(target) => {
                if target.is_empty() {
                    return Err(bad("PDF ToUnicode bfrange destination is empty"));
                }
                let last = target.len() - 1;
                for offset in 0..count {
                    let value = u64::from(target[last]) + offset;
                    if value > 255 {
                        return Err(bad(
                            "PDF ToUnicode bfrange destination overflows its last byte",
                        ));
                    }
                    let mut destination = target.clone();
                    destination[last] = value as u8;
                    self.push(width, start + offset as u32, utf16(&destination)?)?;
                }
            }
            Value::Array(targets) => {
                if targets.len() as u64 != count {
                    return Err(bad(
                        "PDF ToUnicode bfrange destination array length does not match its code range",
                    ));
                }
                for (offset, target) in targets.iter().enumerate() {
                    let Value::String(target) = target else {
                        return Err(bad(
                            "PDF ToUnicode bfrange destination array must hold hexadecimal strings",
                        ));
                    };
                    self.push(width, start + offset as u32, utf16(target)?)?;
                }
            }
            _ => {
                return Err(bad(
                    "PDF ToUnicode bfrange destination must be a hexadecimal string or an array of them",
                ));
            }
        }
        Ok(())
    }
    /// Store one mapping under the entry and destination-byte budgets.
    pub(super) fn push(&mut self, width: usize, code: u32, destination: String) -> Result<()> {
        if self.mappings.len() >= MAX_ENTRIES {
            return Err(limit("PDF ToUnicode CMap exceeds 65536 mappings"));
        }
        let cost = destination.len() + ENTRY_BYTES;
        if self.bytes + cost > MAX_BYTES {
            return Err(limit("PDF ToUnicode CMap mappings exceed 1 MiB"));
        }
        self.mappings.push(Mapping {
            width,
            code,
            destination: destination.into_boxed_str(),
        });
        self.bytes += cost;
        Ok(())
    }
    /// Validate the collected codespace and mappings and build the CMap; this is the only place
    /// a mapping's width is compared with the codespace width.
    pub(super) fn finish(self) -> Result<CMap> {
        if self.ranges == 0 {
            return Err(bad("PDF ToUnicode CMap has no codespace range"));
        }
        let mut targets = BTreeMap::new();
        for mapping in self.mappings {
            if mapping.width != self.width {
                return Err(bad(format!(
                    "PDF ToUnicode code is {} bytes but its codespace is {} bytes",
                    mapping.width, self.width
                )));
            }
            if targets.insert(mapping.code, mapping.destination).is_some() {
                return Err(bad(format!(
                    "Duplicate PDF ToUnicode character code {:04X}",
                    mapping.code
                )));
            }
        }
        Ok(CMap {
            width: self.width,
            targets,
        })
    }
}
