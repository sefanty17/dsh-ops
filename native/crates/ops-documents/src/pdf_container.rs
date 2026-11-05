//! A deliberately restricted PDF container: one classic xref table or one cross-reference
//! stream, plus object streams.
//!
//! Every structure is bounded before it is trusted: a 16 MiB file, 10 000 objects, 1 MiB per
//! encoded or decoded stream, 8 MiB of decoded xref and object streams, 4096 objects per object
//! stream, eight nested object-stream expansions, 64 nested object reads, and a 20-second
//! container deadline. A malformed structure is a structured failure, never a recovered guess.
//!
//! Exactly one cross-reference section is read, and it must end at the final footer. A trailer
//! that names `/Prev` or `/XRefStm` describes an incremental update, which this container
//! refuses (#432): superseded object bytes stay in the file, so following the earlier section
//! would be a different, unsupported format. Parsed object ranges must stay disjoint from each
//! other and from the xref section.
//!
//! The sibling `pdf_xref` module decodes cross-reference stream and object stream bytes; this
//! module owns the document frame around them: footer, classic tables, the loader and ranges.
use crate::pdf_syntax::{Dict, Lexer, MAX_NODES, Value, bad, limit, unsupported, white};
use crate::pdf_xref::{XrefStream, object_stream, xref_stream};
use ops_protocol::{Failure, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};
pub const MAX_FILE: usize = 16 * 1024 * 1024;
pub const MAX_OBJECTS: usize = 10_000;
pub const MAX_STREAM: usize = 1024 * 1024;
/// Decoded bytes of cross-reference streams and object streams, in aggregate.
pub const MAX_XREF_DECODED: usize = 8 * 1024 * 1024;
/// Objects one object stream may hold.
pub const MAX_OBJECT_STREAM_OBJECTS: usize = 4096;
/// Object streams one `/N` or `/First` chain may expand into each other. Both values are direct
/// integers in every supported producer; an indirect one that reaches another object stream is
/// bounded instead of recursing.
pub const MAX_OBJECT_STREAM_DEPTH: usize = 8;
/// Objects one indirect `/Length` chain may hold in flight. Parsing a referenced stream resolves
/// its own `/Length` first, so each link nests one read.
const MAX_NESTED_OBJECT_READS: usize = 64;
const CONTAINER_DEADLINE: Duration = Duration::from_secs(20);
pub type ObjectId = (u32, u16);
/// One cross-reference entry, from a classic table or a stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum XrefEntry {
    /// Type 0: the object number is not in use.
    Free { generation: u16 },
    /// Type 1: an object header at a byte offset.
    Offset { offset: usize, generation: u16 },
    /// Type 2: the object is entry `index` of object stream `stream`.
    Compressed { stream: u32, index: u32 },
}
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
fn check_deadline(started: Instant) -> Result<()> {
    if started.elapsed() >= CONTAINER_DEADLINE {
        return Err(Failure::new(
            "DEADLINE_EXCEEDED",
            "PDF container scan reached its 20-second deadline",
        ));
    }
    Ok(())
}
/// One parsed object, its value, and (for a stream) its exact encoded bytes.
pub(crate) struct Raw<'a> {
    pub(crate) value: Value,
    pub(crate) stream: Option<&'a [u8]>,
    pub(crate) end: usize,
    nodes: usize,
}
/// Read the object at `offset`, resolving `/Length` through `length`.
///
/// `/Length` is the only value read before the object is complete: a stream's extent fixes where
/// its `endstream` must be, so the caller supplies the resolver for the table it already has.
/// The header must start its own token: a byte inside a comment is never an object.
pub(crate) fn read_object<'a>(
    data: &'a [u8],
    offset: usize,
    expected: Option<ObjectId>,
    length: &mut dyn FnMut(&Value) -> Result<usize>,
) -> Result<Raw<'a>> {
    if !data.get(offset).is_some_and(u8::is_ascii_digit) {
        return Err(bad("PDF xref must point exactly to an object header"));
    }
    if offset == 0 || !white(data[offset - 1]) {
        return Err(bad(
            "PDF object header must follow whitespace; a comment cannot introduce an object",
        ));
    }
    let mut lexer = Lexer {
        data,
        at: offset,
        nodes: 0,
    };
    let number = lexer.unsigned()?;
    let generation = lexer.unsigned()?;
    if number > u32::MAX as usize || generation > u16::MAX as usize {
        return Err(bad("PDF object header is not a bounded object identity"));
    }
    let id = (number as u32, generation as u16);
    if expected.is_some_and(|expected| expected != id) {
        return Err(bad("PDF xref object identity mismatch"));
    }
    lexer.keyword(b"obj")?;
    let value = lexer.value(0)?;
    let mut stream = None;
    let token = lexer.word()?;
    if token == b"stream" {
        let length_value = get(value.dict()?, b"Length")?.clone();
        let length = length(&length_value)?;
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
    Ok(Raw {
        value,
        stream,
        end: lexer.at,
        nodes: lexer.nodes,
    })
}
/// The one cross-reference section the file names from its footer.
struct Section {
    entries: BTreeMap<usize, XrefEntry>,
    trailer: Dict,
    /// True when this section is a cross-reference stream rather than a classic table.
    stream: bool,
}
/// Refuse a trailer that names an earlier cross-reference section.
///
/// `/Prev` and `/XRefStm` describe an incremental update only as trailer keys; the same names
/// inside a page or outline dictionary are ordinary data. A file with either key keeps
/// superseded object bytes, so reading it is a different format than the single-revision
/// container below resolves.
fn reject_incremental_trailer(trailer: &Dict) -> Result<()> {
    for key in [b"Prev".as_slice(), b"XRefStm"] {
        if trailer.contains_key(key) {
            return Err(unsupported(
                "Incremental PDF updates with a trailer /Prev or /XRefStm are unsupported",
            ));
        }
    }
    Ok(())
}
fn read_section(data: &[u8], offset: usize, footer_start: usize) -> Result<Section> {
    if offset < 9 || offset >= footer_start {
        return Err(bad("PDF xref offset points outside the object section"));
    }
    if !white(data[offset - 1]) {
        return Err(bad("PDF xref section must follow whitespace"));
    }
    if data[offset..].starts_with(b"xref") {
        classic_section(data, offset, footer_start)
    } else {
        stream_section(data, offset, footer_start)
    }
}
fn classic_section(data: &[u8], offset: usize, footer_start: usize) -> Result<Section> {
    let mut lexer = Lexer {
        data,
        at: offset,
        nodes: 0,
    };
    lexer.keyword(b"xref")?;
    let mut entries: BTreeMap<usize, XrefEntry> = BTreeMap::new();
    loop {
        lexer.skip();
        if data[lexer.at..].starts_with(b"trailer") {
            lexer.keyword(b"trailer")?;
            break;
        }
        let start = lexer.unsigned()?;
        let count = lexer.unsigned()?;
        // Every incremental update starts with an empty subsection; declaring nothing is not a
        // section of this container, and skipping it is not recovery.
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
            let entry = if flag == b"n" {
                XrefEntry::Offset {
                    offset,
                    generation: generation as u16,
                }
            } else {
                XrefEntry::Free {
                    generation: generation as u16,
                }
            };
            if entries.insert(id, entry).is_some() {
                return Err(bad("Duplicate PDF xref object number"));
            }
        }
    }
    let Value::Dict(trailer) = lexer.value(0)? else {
        return Err(bad("Expected a PDF dictionary"));
    };
    reject_incremental_trailer(&trailer)?;
    finish_section(&mut lexer, footer_start)?;
    Ok(Section {
        entries,
        trailer,
        stream: false,
    })
}
fn stream_section(data: &[u8], offset: usize, footer_start: usize) -> Result<Section> {
    let XrefStream {
        entries,
        trailer,
        end,
    } = xref_stream(data, offset)?;
    reject_incremental_trailer(&trailer)?;
    let mut lexer = Lexer {
        data,
        at: end,
        nodes: 0,
    };
    finish_section(&mut lexer, footer_start)?;
    Ok(Section {
        entries,
        trailer,
        stream: true,
    })
}
/// Verify that the section ends at the final footer: nothing follows the only revision.
fn finish_section(lexer: &mut Lexer<'_>, footer_start: usize) -> Result<()> {
    lexer.skip();
    if lexer.at != footer_start {
        return Err(bad("Unexpected PDF data between trailer and final footer"));
    }
    Ok(())
}
/// Parsed object values plus their resolution budgets.
struct Loader<'a> {
    data: &'a [u8],
    entries: BTreeMap<usize, XrefEntry>,
    objects: BTreeMap<ObjectId, Object<'a>>,
    expanded: BTreeMap<u32, Vec<(u32, Value)>>,
    ranges: Vec<(usize, usize)>,
    resolving: BTreeSet<ObjectId>,
    /// Object streams whose `/N` and `/First` are being resolved. Its length is the current
    /// expansion depth.
    expanding: BTreeSet<u32>,
    decoded: usize,
    nodes: usize,
    started: Instant,
}
impl<'a> Loader<'a> {
    fn charge(&mut self, nodes: usize) -> Result<()> {
        self.nodes = self.nodes.saturating_add(nodes);
        if self.nodes > MAX_NODES {
            return Err(limit("PDF metadata exceeds 100000 values"));
        }
        Ok(())
    }
    fn entry(&self, id: ObjectId) -> Result<XrefEntry> {
        let entry = *self
            .entries
            .get(&(id.0 as usize))
            .ok_or_else(|| bad("PDF reference does not name an active xref object"))?;
        if let XrefEntry::Offset { generation, .. } = entry
            && generation != id.1
        {
            return Err(bad("PDF reference generation is not the active xref entry"));
        }
        Ok(entry)
    }
    /// Resolve a value that `/Length`, `/N`, `/First` and `/Size` may write as a reference chain.
    fn length_of(&mut self, value: &Value) -> Result<usize> {
        if !matches!(value, Value::Ref(..)) {
            return value.integer();
        }
        let mut current = value.clone();
        let mut seen = BTreeSet::new();
        loop {
            check_deadline(self.started)?;
            match &current {
                Value::Integer(_) => return current.integer(),
                Value::Ref(number, generation) => {
                    let id = (*number, *generation);
                    if seen.len() >= 64 {
                        return Err(limit("PDF reference chain exceeds 64"));
                    }
                    if !seen.insert(id) {
                        return Err(bad("Cyclic PDF reference"));
                    }
                    current = self.value_of(id)?;
                }
                _ => {
                    return Err(bad(
                        "PDF indirect integer must reference a direct integer object",
                    ));
                }
            }
        }
    }
    fn value_of(&mut self, id: ObjectId) -> Result<Value> {
        match self.entry(id)? {
            XrefEntry::Offset { offset, .. } => {
                self.object_at(id, offset)?;
                Ok(self.objects[&id].value.clone())
            }
            XrefEntry::Compressed { stream, index } => {
                if id.1 != 0 {
                    return Err(bad(
                        "PDF compressed object references must use generation zero",
                    ));
                }
                self.expand(stream)?;
                let Some((number, value)) = self.expanded[&stream].get(index as usize) else {
                    return Err(bad("PDF object stream index is out of range"));
                };
                if *number != id.0 {
                    return Err(bad(
                        "PDF object stream entry does not name the referenced object",
                    ));
                }
                Ok(value.clone())
            }
            XrefEntry::Free { .. } => Err(bad("PDF reference does not name an active xref object")),
        }
    }
    fn object_at(&mut self, id: ObjectId, offset: usize) -> Result<()> {
        if self.objects.contains_key(&id) {
            return Ok(());
        }
        check_deadline(self.started)?;
        if self.resolving.len() >= MAX_NESTED_OBJECT_READS {
            return Err(limit("PDF object reads nest more than 64 deep"));
        }
        if !self.resolving.insert(id) {
            return Err(bad("PDF streams cannot resolve each other's length"));
        }
        let data = self.data;
        let raw = {
            let mut length = |value: &Value| self.length_of(value);
            read_object(data, offset, Some(id), &mut length)
        };
        let raw = raw?;
        self.resolving.remove(&id);
        self.charge(raw.nodes)?;
        self.ranges.push((offset, raw.end));
        self.objects.insert(
            id,
            Object {
                value: raw.value,
                stream: raw.stream,
            },
        );
        Ok(())
    }
    /// Expand one `/Type /ObjStm` stream into its inner objects, in stream order.
    ///
    /// `/N` and `/First` may be indirect and may name an object another object stream holds, so
    /// expansion re-enters here. `expanding` records every stream whose expansion is in progress
    /// before any of its values is resolved: a reference back into one of them is a cycle, and its
    /// length is the nesting depth this method bounds.
    fn expand(&mut self, stream: u32) -> Result<()> {
        if self.expanded.contains_key(&stream) {
            return Ok(());
        }
        check_deadline(self.started)?;
        if self.expanding.contains(&stream) {
            return Err(bad(
                "Cyclic PDF object stream expansion through /N or /First",
            ));
        }
        if self.expanding.len() >= MAX_OBJECT_STREAM_DEPTH {
            return Err(Failure::new(
                "OUTPUT_LIMIT",
                format!(
                    "PDF object stream expansion exceeds {MAX_OBJECT_STREAM_DEPTH} nested streams"
                ),
            ));
        }
        self.expanding.insert(stream);
        let result = self.expand_stream(stream);
        self.expanding.remove(&stream);
        result
    }
    /// Expand `stream`; [`Loader::expand`] owns the cycle and depth guard.
    fn expand_stream(&mut self, stream: u32) -> Result<()> {
        let id = (stream, 0u16);
        let offset = match *self
            .entries
            .get(&(stream as usize))
            .ok_or_else(|| bad("PDF object stream does not name an active xref entry"))?
        {
            XrefEntry::Offset { offset, generation: 0 } => offset,
            _ => {
                return Err(bad(
                    "PDF object stream must be an uncompressed generation-zero object",
                ));
            }
        };
        self.object_at(id, offset)?;
        let (count, first) = {
            let object = &self.objects[&id];
            let dict = object.value.dict()?;
            if get(dict, b"Type")?.name()? != b"ObjStm" {
                return Err(bad("PDF object stream object must have Type /ObjStm"));
            }
            (get(dict, b"N")?.clone(), get(dict, b"First")?.clone())
        };
        let count = self.length_of(&count)?;
        let first = self.length_of(&first)?;
        if count > MAX_OBJECT_STREAM_OBJECTS {
            return Err(limit("PDF object stream exceeds 4096 objects"));
        }
        let decoded = {
            let object = &self.objects[&id];
            decode(object)?
        };
        self.decoded = self.decoded.saturating_add(decoded.len());
        if self.decoded > MAX_XREF_DECODED {
            return Err(limit("PDF decoded xref and object streams exceed 8 MiB"));
        }
        let mut charge = |nodes: usize| self.charge(nodes);
        let values = object_stream(&decoded, count, first, &mut charge)?;
        self.expanded.insert(stream, values);
        Ok(())
    }
}
/// Reject ranges that overlap: an xref offset is an object header, never a byte inside one.
///
/// The header region is checked separately: before the first indexed construct a PDF has its
/// version line and at most a comment line, and anything else there is unindexed data rather
/// than part of the document. Gaps between indexed constructs are not read.
fn verify_ranges(
    data: &[u8],
    mut ranges: Vec<(usize, usize)>,
    footer_start: usize,
) -> Result<()> {
    ranges.sort_unstable();
    if let Some((first, _)) = ranges.first() {
        let mut gap = Lexer::new(data);
        gap.skip();
        if gap.at != *first {
            return Err(bad("Unindexed data before the first PDF object"));
        }
    }
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (start, end) in ranges {
        if start < 9 || end > footer_start || end <= start {
            return Err(bad("PDF construct range is outside the object section"));
        }
        match merged.last_mut() {
            Some(last) if last.0 == start => last.1 = last.1.max(end),
            Some(last) if last.1 > start => return Err(bad("PDF object ranges overlap")),
            _ => merged.push((start, end)),
        }
    }
    Ok(())
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
            "Only PDF 1.0-1.7 with an exact header is supported",
        ));
    }
    let (startxref, footer_start) = footer(data)?;
    let started = Instant::now();
    check_deadline(started)?;
    let Section {
        entries,
        trailer,
        stream: sparse,
    } = read_section(data, startxref, footer_start)?;
    let mut ranges = vec![(startxref, footer_start)];
    let mut loader = Loader {
        data,
        entries,
        objects: BTreeMap::new(),
        expanded: BTreeMap::new(),
        ranges: Vec::new(),
        resolving: BTreeSet::new(),
        expanding: BTreeSet::new(),
        decoded: 0,
        nodes: 0,
        started,
    };
    let size = {
        let size = get(&trailer, b"Size")?.clone();
        loader.length_of(&size)?
    };
    if size == 0 {
        return Err(bad("PDF Size must name at least object zero"));
    }
    if size > MAX_OBJECTS {
        return Err(limit("PDF Size exceeds 10000 objects"));
    }
    if loader.entries.keys().any(|number| *number >= size) {
        return Err(bad(
            "PDF xref declares an object number at or above Size",
        ));
    }
    // A classic table must declare every number below Size; a cross-reference stream may leave
    // holes (its `/Index` need not be contiguous), which then fail only when referenced.
    if !sparse && loader.entries.len() != size {
        return Err(bad(
            "PDF classic xref must declare every object number below Size",
        ));
    }
    if loader
        .entries
        .get(&0)
        .copied()
        .is_none_or(|entry| entry != XrefEntry::Free { generation: u16::MAX })
    {
        return Err(bad("PDF object zero must be free with generation 65535"));
    }
    let root = trailer
        .get(b"Root".as_slice())
        .ok_or_else(|| bad("Missing PDF Root"))?;
    let root = match root {
        Value::Ref(a, b) => (*a, *b),
        _ => return Err(bad("PDF Root must be an indirect reference")),
    };
    let pending: Vec<(usize, XrefEntry)> = loader
        .entries
        .iter()
        .map(|(number, entry)| (*number, *entry))
        .collect();
    for (number, entry) in &pending {
        check_deadline(started)?;
        match entry {
            XrefEntry::Free { .. } => {}
            XrefEntry::Offset { offset, generation } => {
                if *offset < 9 || *offset >= footer_start {
                    return Err(bad("Active PDF object offset is outside the object section"));
                }
                loader.object_at((*number as u32, *generation), *offset)?;
            }
            XrefEntry::Compressed { stream, index } => {
                let _ = index;
                loader.expand(*stream)?;
            }
        }
    }
    let mut objects = loader.objects;
    for (number, entry) in &pending {
        let XrefEntry::Compressed { stream, index } = entry else {
            continue;
        };
        let Some((named, value)) = loader.expanded[stream].get(*index as usize) else {
            return Err(bad("PDF object stream index is out of range"));
        };
        if *named != *number as u32 {
            return Err(bad(
                "PDF object stream entry does not name the referenced object",
            ));
        }
        objects.insert(
            (*number as u32, 0),
            Object {
                value: value.clone(),
                stream: None,
            },
        );
    }
    ranges.append(&mut loader.ranges);
    verify_ranges(data, ranges, footer_start)?;
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
