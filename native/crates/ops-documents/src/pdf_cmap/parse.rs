//! The stream reader: mapping blocks, their entry counts and the operators wrapped around them.
use super::{CMap, MAX_ENTRIES, MAX_OPERANDS, MAX_RANGES, ranges::Table};
use crate::pdf_syntax::{Lexer, Value, bad, limit, unsupported};
use ops_protocol::Result;
impl CMap {
    /// Read a decoded ToUnicode stream, interpreting mappings only; usecmap, mixed code widths
    /// and mappings that disagree with the codespace are rejected; codespaces may follow mappings.
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
            // /UseCMap states mappings this CMap inherits, as an operand or as a dictionary key.
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
                        // A mapping whose block never opened is malformed, not absent.
                        if bare_mapping(&args) {
                            return Err(bad(
                                "PDF ToUnicode mapping outside a bfchar/bfrange block",
                            ));
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
                    // Other operators only carry operands the reader ignores, bar mapping strings.
                    _ => {
                        if !packaging(word) && bare_mapping(&args) {
                            return Err(bad(
                                "PDF ToUnicode mapping outside a bfchar/bfrange block",
                            ));
                        }
                    }
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
            return Err(bad(
                "PDF ToUnicode CMap ends inside a block without its end operator",
            ));
        }
        if !opened {
            return Err(bad(
                "PDF ToUnicode stream is not a CMap: it has no begincmap",
            ));
        }
        if !closed {
            return Err(bad("PDF ToUnicode CMap is missing endcmap"));
        }
        table.finish()
    }
}

#[derive(Clone, Copy)]
/// The begin-end construct currently open, with the entries it still declares.
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
/// Whether these operands state inherited mappings through /UseCMap.
fn inherits(args: &[Value]) -> bool {
    args.iter().any(|arg| match arg {
        Value::Name(name) => name == b"UseCMap",
        Value::Dict(dict) => dict.contains_key(b"UseCMap".as_slice()),
        _ => false,
    })
}
/// Whether these operands are the hexadecimal strings of one mapping entry: two for bfchar, three
/// for bfrange; outside their block such strings carry no mapping meaning.
fn bare_mapping(args: &[Value]) -> bool {
    args.len() >= 2 && args.iter().all(|arg| matches!(arg, Value::String(_)))
}
/// Whether word only packages a CMap and never begins, ends or interprets a mapping.
fn packaging(word: &[u8]) -> bool {
    matches!(
        word,
        b"def" | b"dict" | b"begin" | b"end" | b"findresource" | b"pop"
    )
}
/// Open the block named by word, which declares args[0] entries.
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
