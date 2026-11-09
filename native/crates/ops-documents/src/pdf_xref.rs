//! Cross-reference streams and object streams: the two PDF 1.5+ structures that carry container
//! metadata inside a Flate stream.
//!
//! [`crate::pdf_container`] owns the document frame: the footer, the single classic or stream
//! cross-reference section, object reads, the loader and range verification. This module owns
//! only the decoding of the two stream types that section delegates to it — the `/Type /XRef`
//! stream that spells a cross-reference section and its trailer, and the `/Type /ObjStm` payload
//! that holds inner object values. Both decoders are pure over bytes the caller already bounded;
//! file-level budgets, the deadline and the decoded-byte accounting stay with the caller.
use crate::pdf_container::{
    MAX_OBJECT_STREAM_OBJECTS, MAX_OBJECTS, Object, XrefEntry, decode, get, read_object,
};
use crate::pdf_syntax::{Dict, Lexer, Value, bad, limit, unsupported};
use ops_protocol::Result;
use std::collections::BTreeMap;
/// Bytes of one `/W` field; every value of a PDF file below 2^32 fits four bytes.
const MAX_FIELD: usize = 4;
const MAX_ROW: usize = 3 * MAX_FIELD;
/// One decoded cross-reference stream: its section entries, trailer and end of `endobj`.
pub(crate) struct XrefStream {
    pub(crate) entries: BTreeMap<usize, XrefEntry>,
    pub(crate) trailer: Dict,
    /// Byte just after the stream's `endobj`.
    pub(crate) end: usize,
}
/// The cross-reference stream at `offset`: its entries, its dictionary, and the end of `endobj`.
///
/// The section's stream must be readable with a direct `/Length`; resolving a reference would
/// need the very table this stream defines, so such a file is refused instead of guessed at.
pub(crate) fn xref_stream(data: &[u8], offset: usize) -> Result<XrefStream> {
    let mut direct = |value: &Value| {
        value
            .integer()
            .map_err(|_| bad("PDF xref stream /Length must be a direct bounded integer"))
    };
    let raw = read_object(data, offset, None, &mut direct)?;
    let object = Object {
        value: raw.value,
        stream: raw.stream,
    };
    let dict = object.value.dict()?;
    if get(dict, b"Type")?.name()? != b"XRef" {
        return Err(bad("PDF cross-reference stream must have Type /XRef"));
    }
    let decoded = decode(&object)?;
    let Value::Dict(trailer) = object.value else {
        return Err(bad("Expected a PDF dictionary"));
    };
    let entries = xref_entries(&trailer, &decoded)?;
    Ok(XrefStream {
        entries,
        trailer,
        end: raw.end,
    })
}
fn field(bytes: &[u8], widths: &[usize], index: usize) -> usize {
    let begin: usize = widths[..index].iter().sum();
    bytes[begin..begin + widths[index]]
        .iter()
        .fold(0usize, |value, byte| (value << 8) | *byte as usize)
}
/// Decode `/W` and `/Index` into bounded entries.
fn xref_entries(dict: &Dict, decoded: &[u8]) -> Result<BTreeMap<usize, XrefEntry>> {
    let size = get(dict, b"Size")?.integer()?;
    if size > MAX_OBJECTS {
        return Err(limit("PDF xref stream Size exceeds 10000 objects"));
    }
    let widths: Vec<usize> = match get(dict, b"W")? {
        Value::Array(values) if values.len() == 3 => {
            values.iter().map(Value::integer).collect::<Result<_>>()?
        }
        _ => return Err(bad("PDF xref stream W must be three bounded integers")),
    };
    if widths.iter().any(|width| *width > MAX_FIELD) {
        return Err(limit("PDF xref stream field exceeds 4 bytes"));
    }
    let row: usize = widths.iter().sum();
    if row == 0 || row > MAX_ROW {
        return Err(bad("PDF xref stream entry has no bounded byte width"));
    }
    let ranges = match dict.get(b"Index".as_slice()) {
        None => vec![(0usize, size)],
        Some(value) => {
            let Value::Array(values) = value else {
                return Err(bad("PDF xref stream Index must be an array"));
            };
            if values.is_empty() || values.len() % 2 != 0 {
                return Err(bad("PDF xref stream Index must hold offset/count pairs"));
            }
            values
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| Ok((pair[0].integer()?, pair[1].integer()?)))
                .collect::<Result<Vec<_>>>()?
        }
    };
    let mut total = 0usize;
    for (start, count) in &ranges {
        if *start >= MAX_OBJECTS || *count > MAX_OBJECTS - *start {
            return Err(limit("PDF xref stream exceeds 10000 entries"));
        }
        total = total
            .checked_add(*count)
            .filter(|total| *total <= MAX_OBJECTS)
            .ok_or_else(|| limit("PDF xref stream exceeds 10000 entries"))?;
    }
    let expected = total
        .checked_mul(row)
        .ok_or_else(|| limit("PDF xref stream exceeds 10000 entries"))?;
    if decoded.len() != expected {
        return Err(bad(
            "PDF xref stream bytes disagree with its Size, Index and W",
        ));
    }
    let mut entries: BTreeMap<usize, XrefEntry> = BTreeMap::new();
    let mut at = 0;
    for (start, count) in ranges {
        for number in start..start + count {
            let bytes = &decoded[at..at + row];
            at += row;
            // A zero-width type field means type 1, the most common entry.
            let kind = if widths[0] == 0 {
                1
            } else {
                field(bytes, &widths, 0)
            };
            let second = field(bytes, &widths, 1);
            let third = field(bytes, &widths, 2);
            let entry = match kind {
                0 => {
                    if third > u16::MAX as usize {
                        return Err(bad("Invalid PDF xref stream free entry"));
                    }
                    XrefEntry::Free {
                        generation: third as u16,
                    }
                }
                1 => {
                    if third >= u16::MAX as usize {
                        return Err(bad("Invalid PDF xref stream in-use entry"));
                    }
                    XrefEntry::Offset {
                        offset: second,
                        generation: third as u16,
                    }
                }
                2 => {
                    if second >= size || third >= MAX_OBJECT_STREAM_OBJECTS {
                        return Err(bad(
                            "PDF xref stream compressed entry is outside its bounds",
                        ));
                    }
                    XrefEntry::Compressed {
                        stream: second as u32,
                        index: third as u32,
                    }
                }
                _ => {
                    return Err(unsupported(
                        "PDF xref stream entry types other than 0, 1 and 2 are unsupported",
                    ));
                }
            };
            if entries.insert(number, entry).is_some() {
                return Err(bad("Duplicate PDF xref object number"));
            }
        }
    }
    Ok(entries)
}
/// Parse the decoded payload of one `/Type /ObjStm` stream into its inner objects.
///
/// `count` is the stream's `/N` and `first` its `/First`: the header before `first` pairs each
/// object number with its offset from `first`, and every value must end at the next pair's
/// offset. Each parsed value is charged through `charge`, so the caller keeps one metadata
/// budget across every expanded stream.
pub(crate) fn object_stream(
    decoded: &[u8],
    count: usize,
    first: usize,
    charge: &mut dyn FnMut(usize) -> Result<()>,
) -> Result<Vec<(u32, Value)>> {
    if first > decoded.len() {
        return Err(bad("PDF object stream First points outside its stream"));
    }
    let mut header = Lexer {
        data: decoded,
        at: 0,
        nodes: 0,
    };
    let mut pairs = Vec::with_capacity(count);
    for _ in 0..count {
        let number = header.unsigned()?;
        let offset = header.unsigned()?;
        if number >= MAX_OBJECTS {
            return Err(limit(
                "PDF object stream names an object number at or above 10000",
            ));
        }
        if offset > decoded.len() {
            return Err(bad("PDF object stream entry offset is outside its stream"));
        }
        pairs.push((number, offset));
    }
    header.skip();
    if header.at != first {
        return Err(bad("PDF object stream header does not end at First"));
    }
    let mut values = Vec::with_capacity(count);
    for (index, (number, offset)) in pairs.iter().enumerate() {
        let at = first
            .checked_add(*offset)
            .filter(|at| *at < decoded.len())
            .ok_or_else(|| bad("PDF object stream entry offset is outside its stream"))?;
        let mut lexer = Lexer {
            data: decoded,
            at,
            nodes: 0,
        };
        let value = lexer.value(0)?;
        let nodes = lexer.nodes;
        lexer.skip();
        let next = pairs
            .get(index + 1)
            .map_or(decoded.len(), |(_, offset)| first + offset);
        if lexer.at != next {
            return Err(bad(
                "PDF object stream entry does not end at the next entry offset",
            ));
        }
        charge(nodes)?;
        values.push((*number as u32, value));
    }
    Ok(values)
}
