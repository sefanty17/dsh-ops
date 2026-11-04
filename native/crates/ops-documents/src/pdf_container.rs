//! A deliberately restricted PDF container: one classic xref table, no recovery.
use crate::pdf_syntax::{Dict, Lexer, Value, bad, limit, unsupported, white};
use ops_protocol::Result;
use std::collections::{BTreeMap, BTreeSet};
pub const MAX_FILE: usize = 16 * 1024 * 1024;
pub const MAX_OBJECTS: usize = 10_000;
pub const MAX_STREAM: usize = 1024 * 1024;
pub type ObjectId = (u32, u16);
/// Parsed classic xref table: object number -> (header byte offset, generation, in use).
type Xref = BTreeMap<usize, (usize, u16, bool)>;
#[derive(Debug)]
pub struct Object<'a> {
    pub value: Value,
    pub stream: Option<&'a [u8]>,
}
#[derive(Debug)]
pub struct Document<'a> {
    pub objects: BTreeMap<ObjectId, Object<'a>>,
    pub root: ObjectId,
}
impl<'a> Document<'a> {
    pub fn object(&self, id: ObjectId) -> Result<&Object<'a>> {
        self.objects
            .get(&id)
            .ok_or_else(|| bad("PDF reference does not name an active object"))
    }
    pub fn resolve<'b>(&'b self, value: &'b Value) -> Result<&'b Value> {
        let mut current = value;
        let mut seen = BTreeSet::new();
        while let Value::Ref(number, generation) = current {
            if seen.len() >= 64 {
                return Err(limit("PDF reference chain exceeds 64"));
            }
            if !seen.insert((*number, *generation)) {
                return Err(bad("Cyclic PDF reference"));
            }
            let object = self.object((*number, *generation))?;
            if object.stream.is_some() {
                return Err(bad("A PDF stream cannot be used as a plain value"));
            }
            current = &object.value;
        }
        Ok(current)
    }
}
pub fn get<'a>(dict: &'a Dict, key: &[u8]) -> Result<&'a Value> {
    dict.get(key)
        .ok_or_else(|| bad(format!("Missing PDF {}", String::from_utf8_lossy(key))))
}
fn trim_end(mut data: &[u8]) -> &[u8] {
    while data.last().is_some_and(|b| white(*b)) {
        data = &data[..data.len() - 1];
    }
    data
}
fn footer(data: &[u8]) -> Result<(usize, usize)> {
    let end = trim_end(data);
    let Some(before_eof) = end.strip_suffix(b"%%EOF") else {
        return Err(bad("PDF must end with %%EOF"));
    };
    if !before_eof.last().is_some_and(|b| white(*b)) {
        return Err(bad("Missing PDF EOF separator"));
    }
    let before_eof = trim_end(before_eof);
    let number_start = before_eof
        .iter()
        .rposition(|b| !b.is_ascii_digit())
        .map_or(0, |n| n + 1);
    if number_start == 0 || !white(before_eof[number_start - 1]) {
        return Err(bad("Missing PDF xref number separator"));
    }
    let mut number = Lexer::new(&before_eof[number_start..]);
    let xref = number.unsigned()?;
    let prefix = trim_end(&before_eof[..number_start]);
    let Some(before_start) = prefix.strip_suffix(b"startxref") else {
        return Err(bad("Missing final PDF startxref"));
    };
    if !before_start.last().is_some_and(|b| white(*b)) {
        return Err(bad("Missing PDF startxref separator"));
    }
    let footer_start = before_start.len();
    if xref >= footer_start {
        return Err(bad("PDF xref offset points outside object section"));
    }
    Ok((xref, footer_start))
}
/// Read one xref-indexed object value from `data`, which is bounded to the object section.
fn referenced_value(
    data: &[u8],
    entries: &Xref,
    id: ObjectId,
    nodes: usize,
) -> Result<(Value, usize)> {
    let Some((offset, generation, true)) = entries.get(&(id.0 as usize)).copied() else {
        return Err(bad("PDF reference does not name an active xref object"));
    };
    if generation != id.1 {
        return Err(bad("PDF reference generation is not the active xref entry"));
    }
    if offset < 9 || offset >= data.len() {
        return Err(bad("PDF reference offset is outside the object section"));
    }
    let mut lexer = Lexer {
        data,
        at: offset,
        nodes,
    };
    if lexer.unsigned()? != id.0 as usize || lexer.unsigned()? != id.1 as usize {
        return Err(bad("PDF referenced object identity mismatch"));
    }
    lexer.keyword(b"obj")?;
    let value = lexer.value(0)?;
    if lexer.word()? != b"endobj" {
        return Err(bad("A PDF stream cannot be used as a plain value"));
    }
    Ok((value, lexer.nodes))
}
/// Read a dictionary value that PDF may write as an indirect reference to an integer.
///
/// A stream `/Length` is allowed to be an indirect reference, and LibreOffice writes that
/// integer object after the stream using it. Objects are read in file-offset order, so the
/// object map is still incomplete here, while the xref table already names every offset:
/// resolve through the xref table, under the same 64-hop and cycle bounds as
/// `Document::resolve`. The trailer `/Size` is a direct integer by specification; a reference
/// is still resolved this way so it cannot surface as a malformed integer.
fn resolve_integer(
    data: &[u8],
    entries: &Xref,
    value: &Value,
    nodes: usize,
) -> Result<(usize, usize)> {
    if !matches!(value, Value::Ref(..)) {
        return value.integer().map(|integer| (integer, nodes));
    }
    let mut nodes = nodes;
    let mut current = value.clone();
    let mut seen = BTreeSet::new();
    while let Value::Ref(number, generation) = &current {
        let id = (*number, *generation);
        if seen.len() >= 64 {
            return Err(limit("PDF reference chain exceeds 64"));
        }
        if !seen.insert(id) {
            return Err(bad("Cyclic PDF reference"));
        }
        let (target, consumed) = referenced_value(data, entries, id, nodes)?;
        nodes = consumed;
        current = target;
    }
    if !matches!(&current, Value::Integer(_)) {
        return Err(bad(
            "PDF indirect integer must reference a direct integer object",
        ));
    }
    Ok((current.integer()?, nodes))
}
fn object<'a>(
    data: &'a [u8],
    offset: usize,
    id: ObjectId,
    nodes: usize,
    entries: &Xref,
) -> Result<(Object<'a>, usize, usize)> {
    if !data.get(offset).is_some_and(u8::is_ascii_digit) {
        return Err(bad("PDF xref must point exactly to an object header"));
    }
    let mut lexer = Lexer {
        data,
        at: offset,
        nodes,
    };
    if lexer.unsigned()? != id.0 as usize || lexer.unsigned()? != id.1 as usize {
        return Err(bad("PDF xref object identity mismatch"));
    }
    lexer.keyword(b"obj")?;
    let value = lexer.value(0)?;
    let mut stream = None;
    let token = lexer.word()?;
    if token == b"stream" {
        let (length, consumed) =
            resolve_integer(data, entries, get(value.dict()?, b"Length")?, lexer.nodes)?;
        lexer.nodes = consumed;
        if length > MAX_STREAM {
            return Err(limit("PDF encoded stream exceeds 1 MiB"));
        }
        // The length fixes the stream extent; endstream is verified, never searched for.
        if lexer.data.get(lexer.at..lexer.at + 2) == Some(b"\r\n") {
            lexer.at += 2;
        } else if lexer.data.get(lexer.at) == Some(&b'\n') {
            lexer.at += 1;
        } else {
            return Err(bad("PDF stream keyword must be followed by LF or CRLF"));
        }
        let end = lexer
            .at
            .checked_add(length)
            .filter(|n| *n <= data.len())
            .ok_or_else(|| bad("PDF stream length exceeds its object"))?;
        stream = Some(&data[lexer.at..end]);
        lexer.at = end;
        lexer.keyword(b"endstream")?;
        lexer.keyword(b"endobj")?;
    } else if token != b"endobj" {
        return Err(bad("Unexpected token after PDF object"));
    }
    Ok((Object { value, stream }, lexer.at, lexer.nodes))
}
pub fn parse(data: &[u8]) -> Result<Document<'_>> {
    if data.len() > MAX_FILE {
        return Err(limit("PDF file exceeds 16 MiB"));
    }
    if data.len() < 9
        || &data[..7] != b"%PDF-1."
        || !matches!(data[7], b'0'..=b'7')
        || !matches!(data[8], 10 | 13)
    {
        return Err(unsupported(
            "Only PDF 1.0-1.7 with an exact header and classic xref table is supported",
        ));
    }
    let (xref, footer_start) = footer(data)?;
    if !data[xref..].starts_with(b"xref") {
        return Err(unsupported(
            "PDF requires a classic xref table; xref streams are unsupported",
        ));
    }
    let mut lexer = Lexer {
        data,
        at: xref,
        nodes: 0,
    };
    lexer.keyword(b"xref")?;
    let mut entries: Xref = BTreeMap::new();
    loop {
        lexer.skip();
        if data[lexer.at..].starts_with(b"trailer") {
            lexer.keyword(b"trailer")?;
            break;
        }
        let start = lexer.unsigned()?;
        let count = lexer.unsigned()?;
        if count == 0 {
            return Err(bad("PDF xref subsection cannot be empty"));
        }
        if start >= MAX_OBJECTS
            || count > MAX_OBJECTS - start
            || entries.len() + count > MAX_OBJECTS
        {
            return Err(limit("PDF xref exceeds 10000 entries"));
        }
        for id in start..start + count {
            let offset = lexer.unsigned()?;
            let generation = lexer.unsigned()?;
            let flag = lexer.word()?;
            if generation > u16::MAX as usize
                || (flag == b"n" && generation == u16::MAX as usize)
                || !matches!(flag, b"n" | b"f")
            {
                return Err(bad("Invalid PDF xref entry"));
            }
            if entries
                .insert(id, (offset, generation as u16, flag == b"n"))
                .is_some()
            {
                return Err(bad("Duplicate PDF xref object number"));
            }
        }
    }
    let trailer = lexer.value(0)?;
    let trailer = trailer.dict()?;
    // Only a trailer key means "this file has an earlier xref section"; the same
    // names inside an object dictionary are ordinary keys (outline items carry
    // `/Prev` for their previous sibling).
    for key in [b"Prev".as_slice(), b"XRefStm"] {
        if trailer.contains_key(key) {
            return Err(unsupported(
                "Encrypted, incremental, object-stream and xref-stream PDFs are unsupported",
            ));
        }
    }
    let (size, consumed) =
        resolve_integer(&data[..xref], &entries, get(trailer, b"Size")?, lexer.nodes)?;
    lexer.nodes = consumed;
    if size == 0
        || size > MAX_OBJECTS
        || entries.len() != size
        || entries.keys().copied().ne(0..size)
    {
        return Err(bad(
            "PDF xref must fully declare its bounded Size without incremental sections",
        ));
    }
    if entries
        .get(&0)
        .is_none_or(|(_, generation, active)| *active || *generation != u16::MAX)
    {
        return Err(bad("PDF object zero must be free with generation 65535"));
    }
    let root = match get(trailer, b"Root")? {
        Value::Ref(a, b) => (*a, *b),
        _ => return Err(bad("PDF Root must be an indirect reference")),
    };
    lexer.skip();
    if lexer.at != footer_start {
        return Err(bad("Unexpected PDF data between trailer and final footer"));
    }
    let mut ordered = Vec::new();
    for (number, (offset, generation, active)) in &entries {
        if !*active {
            continue;
        }
        if *offset < 9 || *offset >= xref {
            return Err(bad(
                "Active PDF object offset is outside the object section",
            ));
        }
        ordered.push((*offset, (*number as u32, *generation)));
    }
    ordered.sort_unstable();
    let mut previous = 0;
    let mut nodes = lexer.nodes;
    let mut objects = BTreeMap::new();
    for (offset, id) in ordered {
        if offset < previous {
            return Err(bad("PDF object ranges overlap"));
        }
        let mut gap = Lexer {
            data,
            at: previous,
            nodes,
        };
        gap.skip();
        if gap.at != offset {
            return Err(bad(
                "Unindexed data or overlapping comments in PDF object section",
            ));
        }
        let (item, end, count) = object(&data[..xref], offset, id, nodes, &entries)?;
        previous = end;
        nodes = count;
        objects.insert(id, item);
    }
    let mut gap = Lexer {
        data,
        at: previous,
        nodes,
    };
    gap.skip();
    if gap.at != xref {
        return Err(bad("Unindexed trailing data before PDF xref"));
    }
    let document = Document { objects, root };
    let root_object = document.object(root)?;
    if root_object.stream.is_some() {
        return Err(bad("PDF Root cannot be a stream"));
    }
    root_object.value.dict()?;
    Ok(document)
}
// Decode one already bounded stream; verify the actual zlib terminator and complete input consumption.
pub fn decode(object: &Object<'_>) -> Result<Vec<u8>> {
    let raw = object.stream.ok_or_else(|| bad("Expected a PDF stream"))?;
    if raw.len() > MAX_STREAM {
        return Err(limit("PDF encoded stream exceeds 1 MiB"));
    }
    let dict = object.value.dict()?;
    if dict.contains_key(b"DecodeParms".as_slice()) || dict.contains_key(b"F".as_slice()) {
        return Err(unsupported(
            "PDF stream predictors, decode parameters and external data are unsupported",
        ));
    }
    let Some(filter) = dict.get(b"Filter".as_slice()) else {
        return Ok(raw.to_vec());
    };
    let filter = match filter {
        Value::Array(filters) if filters.len() == 1 => &filters[0],
        value => value,
    };
    if filter.name()? != b"FlateDecode" {
        return Err(unsupported(
            "Only unfiltered or single FlateDecode PDF streams are supported",
        ));
    }
    let mut decoder = flate2::Decompress::new(true);
    let mut output = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        let before_in = decoder.total_in() as usize;
        let before_out = decoder.total_out() as usize;
        let room = (MAX_STREAM + 1 - output.len()).min(buffer.len());
        let status = decoder
            .decompress(
                &raw[before_in..],
                &mut buffer[..room],
                flate2::FlushDecompress::None,
            )
            .map_err(|_| bad("Invalid PDF zlib stream or checksum"))?;
        let written = decoder.total_out() as usize - before_out;
        output.extend_from_slice(&buffer[..written]);
        if output.len() > MAX_STREAM {
            return Err(limit("PDF decoded stream exceeds 1 MiB"));
        }
        if status == flate2::Status::StreamEnd {
            if decoder.total_in() as usize != raw.len() {
                return Err(bad("Trailing data after PDF zlib stream"));
            }
            return Ok(output);
        }
        if decoder.total_in() as usize == before_in && written == 0 {
            return Err(bad("Truncated PDF zlib stream"));
        }
    }
}
#[cfg(test)]
#[path = "pdf_container_tests.rs"]
mod tests;
