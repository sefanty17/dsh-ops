//! Bounded `ToUnicode` CMap decoding: character codes to Unicode. Glyph programs are never read.
use crate::pdf_syntax::{Lexer, Value, bad, limit, unsupported};
use ops_protocol::Result;
use std::collections::BTreeMap;
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
/// One `ToUnicode` CMap: character codes of one fixed width and their Unicode destinations.
///
/// Keys are character codes, not CIDs: under Identity-H/V the two coincide, and a simple font
/// uses one-byte codes. A code absent from the table has no Unicode meaning in this document.
pub struct CMap {
    width: usize,
    targets: BTreeMap<u32, Box<str>>,
}
impl CMap {
    /// Read a decoded `ToUnicode` stream.
    ///
    /// Only the mappings are interpreted; every other operator is skipped with its operands.
    /// `usecmap`, a `/UseCMap` operand, a missing `begincmap`/`endcmap` marker, mixed codespace
    /// widths and mappings that disagree with the codespace are all rejected.
    pub fn parse(data: &[u8]) -> Result<Self> {
        let mut lexer = Lexer::new(data);
        let mut table = Table::default();
        let mut block = Block::None;
        let mut args: Vec<Value> = Vec::new();
        let (mut opened, mut closed) = (false, false);
        loop {
            lexer.skip();
            if lexer.at == data.len() {
                break;
            }
            let byte = data[lexer.at];
            if byte.is_ascii_digit() || b"+-./(<[".contains(&byte) {
                if args.len() >= MAX_OPERANDS {
                    return Err(limit("PDF ToUnicode CMap operator exceeds 64 operands"));
                }
                args.push(lexer.value(0)?);
                if block.arity() == args.len() {
                    match block {
                        Block::Space(left) => {
                            table.codespace(&args[0], &args[1])?;
                            block = Block::Space(step(left)?);
                        }
                        Block::Chars(left) => {
                            table.character(&args[0], &args[1])?;
                            block = Block::Chars(step(left)?);
                        }
                        Block::Ranges(left) => {
                            table.range(&args[0], &args[1], &args[2])?;
                            block = Block::Ranges(step(left)?);
                        }
                        Block::None => {}
                    }
                    args.clear();
                }
                continue;
            }
            let word = lexer.word()?;
            // `/UseCMap` states mappings this CMap inherits, as an operand or as a dictionary key.
            if inherits(&args) {
                return Err(unsupported(
                    "PDF ToUnicode CMaps that inherit mappings through /UseCMap are unsupported",
                ));
            }
            match block {
                Block::None => match word {
                    b"begincmap" => {
                        if opened {
                            return Err(bad("Nested PDF ToUnicode begincmap"));
                        }
                        opened = true;
                    }
                    b"endcmap" => {
                        if !opened {
                            return Err(bad("PDF ToUnicode CMap has no begincmap"));
                        }
                        closed = true;
                        // The PostScript packaging after endcmap is not part of the mapping.
                        break;
                    }
                    b"usecmap" => {
                        return Err(unsupported(
                            "PDF ToUnicode CMaps that inherit mappings through usecmap are unsupported",
                        ));
                    }
                    b"endcodespacerange" | b"endbfchar" | b"endbfrange" => {
                        return Err(bad(format!(
                            "PDF ToUnicode {} ends a block that was never begun",
                            String::from_utf8_lossy(word)
                        )));
                    }
                    b"begincodespacerange" | b"beginbfchar" | b"beginbfrange" => {
                        if !opened {
                            return Err(bad("PDF ToUnicode block starts before begincmap"));
                        }
                        block = begin(word, &args)?;
                    }
                    // `def`, `dict`, `findresource` and every other operator only carry operands
                    // this reader never interprets.
                    _ => {}
                },
                open => {
                    if word != open.end() {
                        return Err(bad(format!(
                            "Unexpected PDF ToUnicode operator {} inside a {} block",
                            String::from_utf8_lossy(word),
                            open.name()
                        )));
                    }
                    if open.remaining() != 0 {
                        return Err(bad(format!(
                            "PDF ToUnicode {} block declares {} more entries than it contains",
                            open.name(),
                            open.remaining()
                        )));
                    }
                    if !args.is_empty() {
                        return Err(bad("Incomplete PDF ToUnicode entry at the end of a block"));
                    }
                    block = Block::None;
                }
            }
            args.clear();
        }
        if !matches!(block, Block::None) {
            return Err(bad("PDF ToUnicode CMap ends inside a block without its end operator"));
        }
        if !opened {
            return Err(bad("PDF ToUnicode stream is not a CMap: it has no begincmap"));
        }
        if !closed {
            return Err(bad("PDF ToUnicode CMap is missing endcmap"));
        }
        if table.ranges == 0 {
            return Err(bad("PDF ToUnicode CMap has no codespace range"));
        }
        Ok(Self {
            width: table.width,
            targets: table.targets,
        })
    }
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
#[derive(Clone, Copy)]
/// The `begin…end` construct currently open, with the entries it still declares.
enum Block {
    None,
    Space(usize),
    Chars(usize),
    Ranges(usize),
}
impl Block {
    /// Operands one complete entry consumes.
    fn arity(self) -> usize {
        match self {
            Self::None => 0,
            Self::Space(_) | Self::Chars(_) => 2,
            Self::Ranges(_) => 3,
        }
    }
    fn end(self) -> &'static [u8] {
        match self {
            Self::None => b"",
            Self::Space(_) => b"endcodespacerange",
            Self::Chars(_) => b"endbfchar",
            Self::Ranges(_) => b"endbfrange",
        }
    }
    fn name(self) -> &'static str {
        match self {
            Self::None => "mapping",
            Self::Space(_) => "begincodespacerange",
            Self::Chars(_) => "beginbfchar",
            Self::Ranges(_) => "beginbfrange",
        }
    }
    fn remaining(self) -> usize {
        match self {
            Self::None => 0,
            Self::Space(left) | Self::Chars(left) | Self::Ranges(left) => left,
        }
    }
}
/// Whether these operands state inherited mappings through `/UseCMap`.
fn inherits(args: &[Value]) -> bool {
    args.iter().any(|arg| match arg {
        Value::Name(name) => name == b"UseCMap",
        Value::Dict(dict) => dict.contains_key(b"UseCMap".as_slice()),
        _ => false,
    })
}
/// Open the block named by `word`, which declares `args[0]` entries.
fn begin(word: &[u8], args: &[Value]) -> Result<Block> {
    let [Value::Integer(count)] = args else {
        return Err(bad(
            "PDF ToUnicode block must be introduced by one nonnegative entry count",
        ));
    };
    let count = usize::try_from(*count)
        .map_err(|_| bad("PDF ToUnicode block declares a negative entry count"))?;
    match word {
        b"begincodespacerange" => {
            if count > MAX_RANGES {
                return Err(limit("PDF ToUnicode CMap exceeds 64 codespace ranges"));
            }
            Ok(Block::Space(count))
        }
        b"beginbfchar" => {
            if count > MAX_ENTRIES {
                return Err(limit("PDF ToUnicode CMap exceeds 65536 mappings"));
            }
            Ok(Block::Chars(count))
        }
        _ => {
            if count > MAX_ENTRIES {
                return Err(limit("PDF ToUnicode CMap exceeds 65536 mappings"));
            }
            Ok(Block::Ranges(count))
        }
    }
}
/// Consume one declared entry, refusing a block that holds more entries than it declares.
fn step(left: usize) -> Result<usize> {
    left.checked_sub(1)
        .ok_or_else(|| bad("PDF ToUnicode block contains more entries than it declares"))
}
#[derive(Default)]
struct Table {
    width: usize,
    ranges: usize,
    targets: BTreeMap<u32, Box<str>>,
    bytes: usize,
}
impl Table {
    fn codespace(&mut self, low: &Value, high: &Value) -> Result<()> {
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
            return Err(bad(
                "PDF ToUnicode codespace mixes code widths, so a code cannot be split",
            ));
        }
        self.ranges += 1;
        Ok(())
    }
    fn character(&mut self, source: &Value, target: &Value) -> Result<()> {
        let (Value::String(source), Value::String(target)) = (source, target) else {
            return Err(bad(
                "PDF ToUnicode bfchar entry must be two hexadecimal strings",
            ));
        };
        self.width_of(source.len())?;
        let destination = utf16(target)?;
        self.insert(code(source), destination)
    }
    fn range(&mut self, low: &Value, high: &Value, target: &Value) -> Result<()> {
        let (Value::String(low), Value::String(high)) = (low, high) else {
            return Err(bad("PDF ToUnicode bfrange must start with two bounds"));
        };
        self.width_of(low.len())?;
        self.width_of(high.len())?;
        if low > high {
            return Err(bad("PDF ToUnicode bfrange is inverted"));
        }
        let start = code(low);
        let count = u64::from(code(high)) - u64::from(start) + 1;
        if self.targets.len() as u64 + count > MAX_ENTRIES as u64 {
            return Err(limit("PDF ToUnicode CMap exceeds 65536 mappings"));
        }
        match target {
            // A scalar destination covers the range by incrementing its last byte, which cannot
            // carry: the specification defines no destination for the overflowed codes.
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
                    self.insert(start + offset as u32, utf16(&destination)?)?;
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
                    self.insert(start + offset as u32, utf16(target)?)?;
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
    /// Refuse a code whose width rules out the codespace width established for this CMap.
    fn width_of(&self, width: usize) -> Result<()> {
        if self.width == 0 {
            return Err(bad(
                "PDF ToUnicode mapping appears before its codespace range",
            ));
        }
        if width != self.width {
            return Err(bad(format!(
                "PDF ToUnicode code is {width} bytes but its codespace is {} bytes",
                self.width
            )));
        }
        Ok(())
    }
    fn insert(&mut self, code: u32, destination: String) -> Result<()> {
        if self.targets.len() >= MAX_ENTRIES {
            return Err(limit("PDF ToUnicode CMap exceeds 65536 mappings"));
        }
        let cost = destination.len() + ENTRY_BYTES;
        if self.bytes + cost > MAX_BYTES {
            return Err(limit("PDF ToUnicode CMap mappings exceed 1 MiB"));
        }
        if self.targets.contains_key(&code) {
            return Err(bad(format!(
                "Duplicate PDF ToUnicode character code {code:04X}"
            )));
        }
        self.targets.insert(code, destination.into_boxed_str());
        self.bytes += cost;
        Ok(())
    }
}
/// Big-endian character code of at most four bytes; the split and the CMap keys share it.
pub fn code(bytes: &[u8]) -> u32 {
    bytes
        .iter()
        .fold(0, |value, byte| value << 8 | u32::from(*byte))
}
/// Decode one destination string: UTF-16BE code units with surrogate pairs combined.
///
/// U+0000 is a legal destination; producers map unused codes to it.
fn utf16(bytes: &[u8]) -> Result<String> {
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

#[cfg(test)]
mod tests {
    use super::*;
    const TWO: &str = "<0000> <FFFF>";
    const ONE: &str = "<00> <FF>";
    /// One CMap whose body is `body`, without a codespace range.
    fn raw(body: &str) -> Vec<u8> {
        format!(
            "/CIDInit /ProcSet findresource begin\n12 dict begin\nbegincmap\n{body}\n\
             endcmap CMapName currentdict /CMap defineresource pop end end\n"
        )
        .into_bytes()
    }
    /// A CMap with one codespace range and `body` after it.
    fn cmap(codespace: &str, body: &str) -> Vec<u8> {
        raw(&format!(
            "/CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def\n\
             /CMapName /Adobe-Identity-UCS def\n/CMapType 2 def\n\
             1 begincodespacerange\n{codespace}\nendcodespacerange\n{body}"
        ))
    }
    fn parse(codespace: &str, body: &str) -> Result<CMap> {
        CMap::parse(&cmap(codespace, body))
    }
    /// Assert the failure code and return the message for a secondary assertion.
    fn rejected(codespace: &str, body: &str, code: &str) -> String {
        let error = parse(codespace, body).err().expect("CMap must be rejected");
        assert_eq!(error.code, code, "{}", error.message);
        error.message
    }
    fn rejected_raw(body: &str, code: &str) -> String {
        let error = CMap::parse(&raw(body)).err().expect("CMap must be rejected");
        assert_eq!(error.code, code, "{}", error.message);
        error.message
    }
    #[test]
    fn bfchar_maps_codes_and_combines_surrogate_pairs() {
        let table = parse(
            TWO,
            "3 beginbfchar\n<0001> <0059>\n<0002> <D83DDE00>\n<0003> <00660066>\nendbfchar",
        )
        .unwrap();
        assert_eq!(table.width(), 2);
        assert_eq!(table.entries(), 3);
        assert_eq!(table.target(1), Some("Y"));
        assert_eq!(table.target(2), Some("😀"));
        // One code may map to several characters, as a ligature does.
        assert_eq!(table.target(3), Some("ff"));
        assert_eq!(table.target(4), None);
    }
    #[test]
    fn bfchar_reads_one_byte_codes_without_widening_them() {
        let table = parse(ONE, "2 beginbfchar\n<01> <0051>\n<0A> <4E2D>\nendbfchar").unwrap();
        assert_eq!(table.width(), 1);
        assert_eq!(table.entries(), 2);
        assert_eq!(table.target(0x01), Some("Q"));
        assert_eq!(table.target(0x0a), Some("中"));
    }
    #[test]
    fn bfrange_increments_the_last_byte_of_a_scalar_destination() {
        let table = parse(TWO, "1 beginbfrange\n<0034> <0036> <0051>\nendbfrange").unwrap();
        assert_eq!(
            (0x34..=0x36)
                .map(|code| table.target(code))
                .collect::<Vec<_>>(),
            [Some("Q"), Some("R"), Some("S")]
        );
        assert_eq!(table.entries(), 3);
    }
    #[test]
    fn bfrange_array_destination_maps_one_target_per_code() {
        let table = parse(
            TWO,
            "1 beginbfrange\n<0010> <0012> [<4E2D> <D83DDE00> <0066>]\nendbfrange",
        )
        .unwrap();
        assert_eq!(table.target(0x10), Some("中"));
        assert_eq!(table.target(0x11), Some("😀"));
        assert_eq!(table.target(0x12), Some("f"));
    }
    #[test]
    fn bfrange_destinations_must_cover_the_whole_range() {
        let message = rejected(
            TWO,
            "1 beginbfrange\n<0010> <0012> [<4E2D> <0066>]\nendbfrange",
            "DOCUMENT_PARSE_ERROR",
        );
        assert!(message.contains("length does not match"), "{message}");
        // A scalar destination cannot carry past its last byte.
        let message = rejected(
            TWO,
            "1 beginbfrange\n<0001> <0002> <00FF>\nendbfrange",
            "DOCUMENT_PARSE_ERROR",
        );
        assert!(message.contains("overflows its last byte"), "{message}");
        rejected(
            TWO,
            "1 beginbfrange\n<0010> <0012> 7\nendbfrange",
            "DOCUMENT_PARSE_ERROR",
        );
        rejected(
            TWO,
            "1 beginbfrange\n<0010> <0012> [<4E2D> 7 7]\nendbfrange",
            "DOCUMENT_PARSE_ERROR",
        );
    }
    #[test]
    fn every_mapping_must_match_the_codespace_width() {
        // Two-byte codes in a one-byte codespace cannot be split into characters.
        let message = rejected(
            ONE,
            "1 beginbfchar\n<0041> <0059>\nendbfchar",
            "DOCUMENT_PARSE_ERROR",
        );
        assert!(message.contains("2 bytes but its codespace is 1 bytes"), "{message}");
        let message = rejected(
            TWO,
            "1 beginbfchar\n<41> <0059>\nendbfchar",
            "DOCUMENT_PARSE_ERROR",
        );
        assert!(message.contains("1 bytes but its codespace is 2 bytes"), "{message}");
        let message = rejected_raw(
            "1 beginbfchar\n<0001> <0041>\nendbfchar",
            "DOCUMENT_PARSE_ERROR",
        );
        assert!(message.contains("before its codespace range"), "{message}");
    }
    #[test]
    fn malformed_destinations_are_rejected() {
        for body in [
            "1 beginbfchar\n<0001> <000000>\nendbfchar",
            "1 beginbfchar\n<0001> <D83D>\nendbfchar",
            "1 beginbfchar\n<0001> <D83D0041>\nendbfchar",
            "1 beginbfchar\n<0001> <DC00>\nendbfchar",
            "1 beginbfchar\n<0001> <>\nendbfchar",
        ] {
            rejected(TWO, body, "DOCUMENT_PARSE_ERROR");
        }
        let long = "00".repeat(MAX_TARGET + 2);
        let message = rejected(
            TWO,
            &format!("1 beginbfchar\n<0001> <{long}>\nendbfchar"),
            "OUTPUT_LIMIT",
        );
        assert!(message.contains("512 bytes"), "{message}");
        // U+0000 is a legal destination: WPS maps unused codes to it.
        let table = parse(TWO, "1 beginbfchar\n<0001> <0000>\nendbfchar").unwrap();
        assert_eq!(table.target(1), Some("\0"));
    }
    #[test]
    fn duplicate_character_codes_are_ambiguous() {
        let message = rejected(
            TWO,
            "2 beginbfchar\n<0001> <0041>\n<0001> <0042>\nendbfchar",
            "DOCUMENT_PARSE_ERROR",
        );
        assert!(message.contains("Duplicate"), "{message}");
        rejected(
            TWO,
            "1 beginbfchar\n<0034> <0041>\nendbfchar\n1 beginbfrange\n<0034> <0035> <0041>\nendbfrange",
            "DOCUMENT_PARSE_ERROR",
        );
    }
    #[test]
    fn inherited_mappings_are_unsupported() {
        let message = rejected(TWO, "usecmap", "UNSUPPORTED_DOCUMENT");
        assert!(message.contains("usecmap"), "{message}");
        let message = rejected(TWO, "<< /UseCMap /Identity-H >>", "UNSUPPORTED_DOCUMENT");
        assert!(message.contains("/UseCMap"), "{message}");
        let message = rejected_raw("12 dict dup /UseCMap 5 0 R put", "UNSUPPORTED_DOCUMENT");
        assert!(message.contains("/UseCMap"), "{message}");
    }
    #[test]
    fn other_constructs_are_skipped_with_their_operands() {
        let table = parse(
            TWO,
            "1 begincidrange\n<0000> <0003> 7\nendcidrange\n\
             /CIDSystemInfo <</Registry (Adobe) /Ordering (UCS)>> def\n\
             /CMapName /Identity-UCS def\n\
             1 beginbfchar\n<0041> <0059>\nendbfchar",
        )
        .unwrap();
        assert_eq!(table.entries(), 1);
        assert_eq!(table.target(0x41), Some("Y"));
    }
    #[test]
    fn declared_entries_must_match_the_block() {
        let message = rejected(
            TWO,
            "2 beginbfchar\n<0001> <0041>\nendbfchar",
            "DOCUMENT_PARSE_ERROR",
        );
        assert!(
            message.contains("more entries than it contains"),
            "{message}"
        );
        let message = rejected(
            TWO,
            "1 beginbfchar\n<0001> <0041>\n<0002> <0042>\nendbfchar",
            "DOCUMENT_PARSE_ERROR",
        );
        assert!(
            message.contains("more entries than it declares"),
            "{message}"
        );
        // A block introduced without its entry count is ambiguous.
        rejected_raw("begincodespacerange <00> <FF> endcodespacerange", "DOCUMENT_PARSE_ERROR");
        rejected(
            TWO,
            "1 beginbfchar\n<0001> <0041> endbfchar endbfchar",
            "DOCUMENT_PARSE_ERROR",
        );
    }
    #[test]
    fn a_cmap_needs_its_markers_its_codespace_and_its_block_ends() {
        let message =
            CMap::parse(b"begincmap 1 begincodespacerange <0000> <FFFF> endcodespacerange")
                .err().expect("a CMap without endcmap is truncated")
                .message;
        assert!(message.contains("endcmap"), "{message}");
        let message = CMap::parse(b"1 beginbfchar <0001> <0041> endbfchar endcmap")
            .err().expect("a CMap needs begincmap")
            .message;
        assert!(message.contains("begincmap"), "{message}");
        let message =
            CMap::parse(b"begincmap 1 beginbfchar <0001> <0041> endbfchar endcmap")
                .err().expect("a CMap needs a codespace range")
                .message;
        assert!(message.contains("codespace"), "{message}");
        // A stream that stops inside a block is truncated, not a complete CMap.
        let message = CMap::parse(
            b"begincmap 1 begincodespacerange <0000> <FFFF> endcodespacerange \
              1 beginbfchar <0001> <0041>",
        )
        .err().expect("an unterminated bfchar block is truncated")
        .message;
        assert!(message.contains("ends inside a block"), "{message}");
        let message = rejected(TWO, "1 beginbfchar\n<0001> <0041> endcmap", "DOCUMENT_PARSE_ERROR");
        assert!(message.contains("Unexpected PDF ToUnicode operator"), "{message}");
        // The PostScript packaging after endcmap is not interpreted.
        let table = parse(TWO, "1 beginbfchar\n<0001> <0041>\nendbfchar").unwrap();
        assert_eq!(table.entries(), 1);
    }
    #[test]
    fn codespace_ranges_are_bounded_and_share_one_width() {
        let mut body = String::from("65 begincodespacerange\n");
        body.push_str("<0000> <00FF>\n");
        let error = CMap::parse(&raw(&format!("{body}endcodespacerange")))
            .err().expect("a CMap may declare at most 64 codespace ranges");
        assert_eq!(error.code, "OUTPUT_LIMIT", "{}", error.message);
        let error = CMap::parse(&raw(
            "2 begincodespacerange <00> <FF> <0000> <FFFF> endcodespacerange",
        ))
        .err().expect("mixed code widths cannot be split");
        assert_eq!(error.code, "DOCUMENT_PARSE_ERROR", "{}", error.message);
        assert!(
            error.message.contains("mixes code widths"),
            "{}",
            error.message
        );
        rejected_raw(
            "1 begincodespacerange <0000> <FF> endcodespacerange",
            "DOCUMENT_PARSE_ERROR",
        );
        rejected_raw(
            "1 begincodespacerange <FFFF> <0000> endcodespacerange",
            "DOCUMENT_PARSE_ERROR",
        );
        rejected_raw(
            "1 begincodespacerange <0000000000> <0000000000> endcodespacerange",
            "DOCUMENT_PARSE_ERROR",
        );
    }
    #[test]
    fn mapping_budgets_are_enforced() {
        // A four-byte codespace can describe 2^32 codes, far beyond the mapping budget.
        let error = parse(
            "<00000000> <FFFFFFFF>",
            "1 beginbfrange\n<00000000> <FFFFFFFF> <0041>\nendbfrange",
        )
        .err().expect("a CMap maps at most 65536 codes");
        assert_eq!(error.code, "OUTPUT_LIMIT", "{}", error.message);
        let mut table = Table::default();
        let mut failure = None;
        for code in 0..MAX_ENTRIES as u32 {
            match table.insert(code, "x".repeat(MAX_TARGET)) {
                Ok(()) => {}
                Err(error) => {
                    failure = Some(error);
                    break;
                }
            }
        }
        let error = failure.expect("the mapping memory budget must end the insert loop");
        assert_eq!(error.code, "OUTPUT_LIMIT", "{}", error.message);
        assert!(error.message.contains("1 MiB"), "{}", error.message);
        assert!(table.targets.len() < MAX_ENTRIES);
        // The same CMap path: a declared block larger than the entry budget is refused up front.
        let error = parse(TWO, "65537 beginbfchar\nendbfchar").err().expect("too many declared entries");
        assert_eq!(error.code, "OUTPUT_LIMIT", "{}", error.message);
    }
}
