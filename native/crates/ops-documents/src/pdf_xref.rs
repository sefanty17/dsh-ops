//! Cross-reference stream and object stream decoding, pure over bytes the caller already bounded.
use crate::pdf_container::{
    MAX_OBJECT_STREAM_OBJECTS, MAX_OBJECTS, Object, XrefEntry, decode, get, read_object,
};
use crate::pdf_syntax::{Dict, Lexer, Value, bad, limit, unsupported};
use ops_protocol::Result;
use std::collections::{BTreeMap, BTreeSet};
/// Bytes of one /W field; every value of a PDF file below 2^32 fits four bytes.
const MAX_FIELD: usize = 4;
const MAX_ROW: usize = 3 * MAX_FIELD;
/// One decoded cross-reference stream: its section entries, trailer and end of endobj.
pub(crate) struct XrefStream {
    pub(crate) entries: BTreeMap<usize, XrefEntry>,
    pub(crate) trailer: Dict,
    /// Byte just after the stream's endobj.
    pub(crate) end: usize,
    /// Decoded payload bytes, which the caller charges to the shared decoded-stream budget.
    pub(crate) decoded: usize,
}
/// The cross-reference stream at offset: its entries, its dictionary, and the end of endobj.
/// Its /Length must be direct, because resolving one needs the table this stream defines.
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
    let entries = xref_entries(data, &trailer, &decoded)?;
    Ok(XrefStream {
        entries,
        trailer,
        end: raw.end,
        decoded: decoded.len(),
    })
}
fn field(bytes: &[u8], widths: &[usize], index: usize) -> usize {
    let begin: usize = widths[..index].iter().sum();
    bytes[begin..begin + widths[index]]
        .iter()
        .fold(0usize, |value, byte| (value << 8) | *byte as usize)
}
/// Decode /W and /Index into bounded entries, then read the /Size that bounds them; a stream
/// without /Index takes its row count from the payload, equal to the default [0 Size].
fn xref_entries(data: &[u8], dict: &Dict, decoded: &[u8]) -> Result<BTreeMap<usize, XrefEntry>> {
    let size_value = get(dict, b"Size")?;
    let direct_size = match size_value {
        Value::Integer(_) => Some(size_value.integer()?),
        Value::Ref(..) => None,
        _ => return Err(bad("PDF xref stream Size must be a bounded integer")),
    };
    if direct_size.is_some_and(|size| size > MAX_OBJECTS) {
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
    let (ranges, derived_size) = match dict.get(b"Index".as_slice()) {
        None => match direct_size {
            Some(size) => (vec![(0usize, size)], None),
            None => {
                // The default /Index [0 Size] makes the payload exactly Size rows of row bytes.
                if !decoded.len().is_multiple_of(row) {
                    return Err(bad(
                        "PDF xref stream bytes disagree with its Size, Index and W",
                    ));
                }
                let size = decoded.len() / row;
                (vec![(0usize, size)], Some(size))
            }
        },
        Some(value) => {
            let Value::Array(values) = value else {
                return Err(bad("PDF xref stream Index must be an array"));
            };
            if values.is_empty() || values.len() % 2 != 0 {
                return Err(bad("PDF xref stream Index must hold offset/count pairs"));
            }
            let ranges = values
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| Ok((pair[0].integer()?, pair[1].integer()?)))
                .collect::<Result<Vec<_>>>()?;
            (ranges, None)
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
                    if third >= MAX_OBJECT_STREAM_OBJECTS {
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
    let size = size_of(data, &entries, size_value)?;
    if size > MAX_OBJECTS {
        return Err(limit("PDF xref stream Size exceeds 10000 objects"));
    }
    if derived_size.is_some_and(|derived| derived != size) {
        return Err(bad("PDF xref stream Size disagrees with its default Index"));
    }
    for entry in entries.values() {
        if let XrefEntry::Compressed { stream, .. } = entry
            && *stream as usize >= size
        {
            return Err(bad(
                "PDF xref stream compressed entry is outside its bounds",
            ));
        }
    }
    Ok(entries)
}
/// Resolve a cross-reference stream's /Size through the entries it declares; the named object
/// must be uncompressed, because expanding one needs the document frame this section defines.
fn size_of(data: &[u8], entries: &BTreeMap<usize, XrefEntry>, value: &Value) -> Result<usize> {
    if !matches!(value, Value::Ref(..)) {
        return value.integer();
    }
    let mut current = value.clone();
    let mut seen = BTreeSet::new();
    loop {
        match current {
            Value::Integer(_) => return current.integer(),
            Value::Ref(number, generation) => {
                if seen.len() >= 64 {
                    return Err(limit("PDF reference chain exceeds 64"));
                }
                if !seen.insert((number, generation)) {
                    return Err(bad("Cyclic PDF reference"));
                }
                let entry = *entries.get(&(number as usize)).ok_or_else(|| {
                    bad("PDF xref stream Size reference does not name an object in its section")
                })?;
                let XrefEntry::Offset {
                    offset,
                    generation: active,
                } = entry
                else {
                    return Err(bad(
                        "PDF xref stream Size reference must name an uncompressed object",
                    ));
                };
                if active != generation {
                    return Err(bad("PDF reference generation is not the active xref entry"));
                }
                let mut direct = |value: &Value| value.integer();
                let raw = read_object(data, offset, Some((number, generation)), &mut direct)?;
                if raw.stream.is_some() {
                    return Err(bad("PDF /Size must be a direct integer"));
                }
                current = raw.value;
            }
            _ => {
                return Err(bad(
                    "PDF indirect integer must reference a direct integer object",
                ));
            }
        }
    }
}
/// Parse the decoded payload of one /Type /ObjStm stream; count is /N and first is /First, and
/// each value is charged through charge so the caller keeps one metadata budget across streams.
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
