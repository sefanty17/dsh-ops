//! Bounded PDF container: classic xref tables, xref streams, object streams, incremental updates.
use crate::pdf_predictor::decode_parms;
use crate::pdf_syntax::{Dict, Lexer, MAX_NODES, Value, bad, limit, unsupported, white};
use crate::pdf_xref::{XrefStream, object_stream, xref_stream};
use ops_protocol::{Failure, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};
pub const MAX_FILE: usize = 16 * 1024 * 1024;
pub const MAX_OBJECTS: usize = 10_000;
pub const MAX_STREAM: usize = 1024 * 1024;
/// Cross-reference sections read through one /Prev chain.
pub const MAX_SECTIONS: usize = 64;
/// Decoded bytes of cross-reference streams and object streams, in aggregate.
pub const MAX_XREF_DECODED: usize = 8 * 1024 * 1024;
/// Objects one object stream may hold.
pub const MAX_OBJECT_STREAM_OBJECTS: usize = 4096;
/// Object streams that nested /N or /First chains may expand into each other.
pub const MAX_OBJECT_STREAM_DEPTH: usize = 8;
/// Objects one indirect /Length chain may hold in flight, one per nesting link.
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
    /// Type 2: entry index of object stream stream.
    Compressed { stream: u32, index: u32 },
}
impl XrefEntry {
    fn active(self) -> bool {
        !matches!(self, Self::Free { .. })
    }
}
/// One object read from the file; a stream's bytes are never decoded here.
#[derive(Debug)]
pub struct Object<'a> {
    pub value: Value,
    pub stream: Option<&'a [u8]>,
}
/// The container: every object the chain reached, plus the trailer's /Root.
#[derive(Debug)]
pub struct Document<'a> {
    pub objects: BTreeMap<ObjectId, Object<'a>>,
    pub root: ObjectId,
}
impl<'a> Document<'a> {
    /// The object at id; a free entry or a number outside the chain is DOCUMENT_PARSE_ERROR.
    pub fn object(&self, id: ObjectId) -> Result<&Object<'a>> {
        self.objects
            .get(&id)
            .ok_or_else(|| bad("PDF reference does not name an active object"))
    }
    /// Follow indirect references to their plain value; cycles, streams and chains over 64 fail.
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
/// The value at key, for an entry the format requires; a missing key is DOCUMENT_PARSE_ERROR.
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
#[derive(Debug)]
pub(crate) struct Raw<'a> {
    pub(crate) value: Value,
    pub(crate) stream: Option<&'a [u8]>,
    /// Where the object header actually starts, after any tolerated leading whitespace.
    pub(crate) start: usize,
    pub(crate) end: usize,
    nodes: usize,
}
/// Whitespace bytes a cross-reference offset may lead its object header by. Microsoft Print to
/// PDF records every in-use offset one byte short, at the line feed before the header.
const MAX_HEADER_LEAD: usize = 32;
/// True when a `%` outside a literal string starts a comment before `header`. String content such
/// as `(100%)` is data, not a comment, so it cannot hide the header; `endobj % 1 0 obj` is refused.
/// Only a string opened on this line is tracked: bytes of a string continued from an earlier line
/// are classified as outside a string.
fn comment_before(data: &[u8], line_start: usize, header: usize) -> bool {
    let mut depth = 0usize;
    let mut escaped = false;
    for byte in data[line_start..header].iter().copied() {
        if depth == 0 {
            match byte {
                b'(' => depth = 1,
                b'%' => return true,
                _ => {}
            }
        } else if escaped {
            escaped = false;
        } else {
            match byte {
                b'\\' => escaped = true,
                b'(' => depth += 1,
                b')' => depth -= 1,
                _ => {}
            }
        }
    }
    false
}
/// Read the object at offset; /Length is the only value read before the object is complete.
pub(crate) fn read_object<'a>(
    data: &'a [u8],
    offset: usize,
    expected: Option<ObjectId>,
    length: &mut dyn FnMut(&Value) -> Result<usize>,
) -> Result<Raw<'a>> {
    // Only leading whitespace is tolerated before the header: an offset that names anything else,
    // or that leads the header by more than MAX_HEADER_LEAD bytes, is the original failure.
    let mut header = offset;
    while header - offset < MAX_HEADER_LEAD && data.get(header).is_some_and(|byte| white(*byte)) {
        header += 1;
    }
    if !data.get(header).is_some_and(u8::is_ascii_digit) {
        return Err(bad("PDF xref must point exactly to an object header"));
    }
    if header == 0 {
        return Err(bad("PDF object header must follow whitespace"));
    }
    // A % starts a comment to the line end, so anything after one is comment text; an object
    // header cannot be introduced from inside a comment, even when the % is not the line's first
    // token. Literal-string bytes are skipped so a value such as `(100%)` stays data.
    let line_start = data[..header]
        .iter()
        .rposition(|byte| matches!(byte, b'\n' | b'\r'))
        .map_or(0, |at| at + 1);
    if comment_before(data, line_start, header) {
        return Err(bad(
            "PDF object header must follow whitespace; a comment cannot introduce an object",
        ));
    }
    if !white(data[header - 1]) {
        return Err(bad("PDF object header must follow whitespace"));
    }
    let mut lexer = Lexer {
        data,
        at: header,
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
        start: header,
        end: lexer.at,
        nodes: lexer.nodes,
    })
}
/// One cross-reference section: its entries, trailer and the next older section.
struct Section {
    entries: BTreeMap<usize, XrefEntry>,
    trailer: Dict,
    /// /Prev: the byte offset of the next older cross-reference section.
    prev: Option<usize>,
    /// /XRefStm: the hybrid cross-reference stream that supplements this classic table.
    xref_stm: Option<usize>,
    /// True when this section is a cross-reference stream rather than a classic table.
    stream: bool,
    /// Decoded bytes of a cross-reference stream; a classic table decodes none.
    decoded: usize,
    /// Byte just after the section's trailer, or after its %%EOF when another section follows.
    end: usize,
    /// Values the section and trailer consumed; the file-wide budget starts here.
    nodes: usize,
}
/// A trailer integer that names another section, as a direct bounded offset.
fn optional_offset(dict: &Dict, key: &[u8]) -> Result<Option<usize>> {
    match dict.get(key) {
        None => Ok(None),
        Some(value) => value.integer().map(Some),
    }
}
/// Merge a hybrid /XRefStm table into its classic one; disagreeing in-use entries are a failure.
fn merge_hybrid(
    entries: &mut BTreeMap<usize, XrefEntry>,
    extra: BTreeMap<usize, XrefEntry>,
) -> Result<()> {
    for (number, entry) in extra {
        match entries.get(&number).copied() {
            None => {
                entries.insert(number, entry);
            }
            Some(existing) if existing == entry => {}
            Some(existing) if existing.active() && entry.active() => {
                return Err(bad("PDF hybrid xref entries disagree for one object"));
            }
            Some(existing) if !existing.active() => {
                entries.insert(number, entry);
            }
            Some(_) => {}
        }
    }
    Ok(())
}
fn read_section(
    data: &[u8],
    offset: usize,
    last: bool,
    footer_start: usize,
    started: Instant,
) -> Result<Section> {
    check_deadline(started)?;
    if offset < 9 || offset >= footer_start {
        return Err(bad("PDF xref offset points outside the object section"));
    }
    if !white(data[offset - 1]) {
        return Err(bad("PDF xref section must follow whitespace"));
    }
    if data[offset..].starts_with(b"xref") {
        classic_section(data, offset, last, footer_start, started)
    } else {
        stream_section(data, offset, last, footer_start, started)
    }
}
fn classic_section(
    data: &[u8],
    offset: usize,
    last: bool,
    footer_start: usize,
    started: Instant,
) -> Result<Section> {
    let mut lexer = Lexer {
        data,
        at: offset,
        nodes: 0,
    };
    lexer.keyword(b"xref")?;
    let mut entries: BTreeMap<usize, XrefEntry> = BTreeMap::new();
    loop {
        check_deadline(started)?;
        lexer.skip();
        if data[lexer.at..].starts_with(b"trailer") {
            lexer.keyword(b"trailer")?;
            break;
        }
        let start = lexer.unsigned()?;
        let count = lexer.unsigned()?;
        if start >= MAX_OBJECTS || count > MAX_OBJECTS - start {
            return Err(limit("PDF xref exceeds 10000 entries"));
        }
        // An empty subsection declares nothing; every incremental update ends with one.
        if count == 0 {
            continue;
        }
        if entries.len() + count > MAX_OBJECTS {
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
    // /Prev and /XRefStm mean earlier structure only as trailer keys; elsewhere they are data.
    let prev = optional_offset(&trailer, b"Prev")?;
    let xref_stm = optional_offset(&trailer, b"XRefStm")?;
    let end = finish_section(&mut lexer, offset, last, footer_start)?;
    Ok(Section {
        entries,
        trailer,
        prev,
        xref_stm,
        stream: false,
        decoded: 0,
        end,
        nodes: lexer.nodes,
    })
}
fn stream_section(
    data: &[u8],
    offset: usize,
    last: bool,
    footer_start: usize,
    started: Instant,
) -> Result<Section> {
    check_deadline(started)?;
    let XrefStream {
        entries,
        trailer,
        end,
        decoded,
    } = xref_stream(data, offset)?;
    if trailer.contains_key(b"XRefStm".as_slice()) {
        return Err(bad("PDF xref stream cannot contain another /XRefStm"));
    }
    let prev = optional_offset(&trailer, b"Prev")?;
    let mut lexer = Lexer {
        data,
        at: end,
        nodes: 0,
    };
    let end = finish_section(&mut lexer, offset, last, footer_start)?;
    Ok(Section {
        entries,
        trailer,
        prev,
        xref_stm: None,
        stream: true,
        decoded,
        end,
        nodes: lexer.nodes,
    })
}
/// Verify the section's startxref and %%EOF, and return where its bytes end.
fn finish_section(
    lexer: &mut Lexer<'_>,
    offset: usize,
    last: bool,
    footer_start: usize,
) -> Result<usize> {
    lexer.skip();
    if last {
        if lexer.at != footer_start {
            return Err(bad("Unexpected PDF data between trailer and final footer"));
        }
        return Ok(footer_start);
    }
    if lexer.word()? != b"startxref" {
        return Err(bad("PDF incremental section must be followed by startxref"));
    }
    if lexer.unsigned()? != offset {
        return Err(bad(
            "PDF incremental startxref must name the section that follows it",
        ));
    }
    // %%EOF starts with '%', which the whitespace skipper would consume as a comment.
    let mut at = lexer.at;
    while at < lexer.data.len() && white(lexer.data[at]) {
        at += 1;
    }
    if !lexer.data[at..].starts_with(b"%%EOF") {
        return Err(bad("Missing PDF %%EOF after an incremental section"));
    }
    lexer.at = at + 5;
    Ok(lexer.at)
}
/// Parsed object values plus their resolution budgets.
struct Loader<'a> {
    data: &'a [u8],
    entries: BTreeMap<usize, XrefEntry>,
    objects: BTreeMap<ObjectId, Object<'a>>,
    expanded: BTreeMap<u32, Vec<(u32, Value)>>,
    ranges: Vec<(usize, usize)>,
    resolving: BTreeSet<ObjectId>,
    /// Object streams whose /N and /First are being resolved; its length is the expansion depth.
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
    /// Resolve a value that /Length, /N, /First and /Size may write as a reference chain.
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
        // The range starts at the header, so a tolerated leading whitespace byte cannot make the
        // first object look unindexed or overlap the construct before it.
        self.ranges.push((raw.start, raw.end));
        self.objects.insert(
            id,
            Object {
                value: raw.value,
                stream: raw.stream,
            },
        );
        Ok(())
    }
    /// Read an object only to cover its bytes: older revisions that a newer xref superseded are
    /// still indexed by their own section, so the gap verifier must not call them unindexed data.
    fn record_at(&mut self, id: ObjectId, offset: usize) -> Result<()> {
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
        self.ranges.push((raw.start, raw.end));
        Ok(())
    }
    /// Expand one /Type /ObjStm stream into its inner objects, in stream order; /N and /First may
    /// name another object stream, so in-progress streams are recorded before their values resolve.
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
    /// Expand stream; Loader::expand owns the cycle and depth guard.
    fn expand_stream(&mut self, stream: u32) -> Result<()> {
        let id = (stream, 0u16);
        let offset = match *self
            .entries
            .get(&(stream as usize))
            .ok_or_else(|| bad("PDF object stream does not name an active xref entry"))?
        {
            XrefEntry::Offset {
                offset,
                generation: 0,
            } => offset,
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
/// Reject overlapping ranges and unindexed bytes: every gap may hold only whitespace or comments.
fn verify_ranges(data: &[u8], mut ranges: Vec<(usize, usize)>, footer_start: usize) -> Result<()> {
    ranges.sort_unstable();
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
    // Section ranges cover each revision's xref table/stream and startxref/%%EOF, so every gap is
    // lexed from the previous construct to the next: only whitespace and % comments may be skipped.
    let mut previous = 0;
    for (start, end) in merged {
        let mut gap = Lexer {
            data,
            at: previous,
            nodes: 0,
        };
        gap.skip();
        if gap.at != start {
            return Err(bad(
                "Unindexed data or overlapping comments in PDF object section",
            ));
        }
        previous = end;
    }
    Ok(())
}
/// Parse the file into its objects from startxref; a structural mismatch is DOCUMENT_PARSE_ERROR,
/// a version outside 1.0-1.7 UNSUPPORTED_DOCUMENT, a size, section or decode bound OUTPUT_LIMIT.
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
    let mut sections: Vec<Section> = Vec::new();
    let mut seen = BTreeSet::new();
    let mut ranges = Vec::new();
    // A cross-reference stream may leave /Index holes; a classic-only chain declares every number.
    let mut sparse = false;
    let mut decoded = 0usize;
    let mut section_nodes = 0usize;
    let mut cursor = Some(startxref);
    while let Some(offset) = cursor {
        if sections.len() >= MAX_SECTIONS {
            return Err(limit("PDF xref chain exceeds 64 sections"));
        }
        check_deadline(started)?;
        if !seen.insert(offset) {
            return Err(bad("Cyclic PDF xref /Prev chain"));
        }
        let last = sections.is_empty();
        let mut section = read_section(data, offset, last, footer_start, started)?;
        ranges.push((offset, section.end));
        sparse |= section.stream;
        decoded = decoded.saturating_add(section.decoded);
        section_nodes = section_nodes.saturating_add(section.nodes);
        if let Some(hybrid) = section.xref_stm.take() {
            check_deadline(started)?;
            let table = xref_stream(data, hybrid)?;
            ranges.push((hybrid, table.end));
            sparse = true;
            decoded = decoded.saturating_add(table.decoded);
            if table.trailer.contains_key(b"XRefStm".as_slice()) {
                return Err(bad("PDF xref stream cannot contain another /XRefStm"));
            }
            let hybrid_prev = optional_offset(&table.trailer, b"Prev")?;
            merge_hybrid(&mut section.entries, table.entries)?;
            section.prev = match (section.prev, hybrid_prev) {
                (Some(current), Some(hybrid)) if current != hybrid => {
                    return Err(bad(
                        "PDF hybrid trailer and its xref stream disagree on /Prev",
                    ));
                }
                (current, hybrid) => current.or(hybrid),
            };
        }
        if decoded > MAX_XREF_DECODED {
            return Err(limit("PDF decoded xref and object streams exceed 8 MiB"));
        }
        cursor = match section.prev {
            Some(prev) => {
                if seen.contains(&prev) {
                    return Err(bad("Cyclic PDF xref /Prev chain"));
                }
                if prev >= offset {
                    return Err(bad("PDF /Prev must name an earlier xref section"));
                }
                Some(prev)
            }
            None => None,
        };
        sections.push(section);
    }
    // Newest sections are merged first, so an older entry never overrides a newer one.
    let mut entries: BTreeMap<usize, XrefEntry> = BTreeMap::new();
    for section in &sections {
        for (number, entry) in &section.entries {
            entries.entry(*number).or_insert(*entry);
        }
    }
    // A trailer key comes from the newest revision that declares it.
    let size_value = sections
        .iter()
        .find_map(|section| section.trailer.get(b"Size".as_slice()))
        .ok_or_else(|| bad("Missing PDF Size"))?
        .clone();
    let root = sections
        .iter()
        .find_map(|section| section.trailer.get(b"Root".as_slice()))
        .ok_or_else(|| bad("Missing PDF Root"))?;
    let root = match root {
        Value::Ref(a, b) => (*a, *b),
        _ => return Err(bad("PDF Root must be an indirect reference")),
    };
    let mut loader = Loader {
        data,
        entries,
        objects: BTreeMap::new(),
        expanded: BTreeMap::new(),
        ranges: Vec::new(),
        resolving: BTreeSet::new(),
        expanding: BTreeSet::new(),
        decoded,
        nodes: section_nodes,
        started,
    };
    loader.charge(0)?;
    let size = loader.length_of(&size_value)?;
    if size == 0 {
        return Err(bad("PDF Size must name at least object zero"));
    }
    if size > MAX_OBJECTS {
        return Err(limit("PDF Size exceeds 10000 objects"));
    }
    if loader.entries.keys().any(|number| *number >= size) {
        return Err(bad("PDF xref declares an object number at or above Size"));
    }
    // A classic table declares every number below Size; a stream may leave holes until referenced.
    if !sparse && loader.entries.len() != size {
        return Err(bad(
            "PDF classic xref must declare every object number below Size",
        ));
    }
    // Object zero heads the free list, so the newest declaration decides; a classic table spells
    // generation as 65535, while a stream's narrow /W field can only hold 255.
    let zero = loader.entries.get(&0).copied();
    if sparse {
        if !matches!(zero, Some(XrefEntry::Free { .. })) {
            return Err(bad("PDF object zero must be a free entry"));
        }
    } else if zero
        != Some(XrefEntry::Free {
            generation: u16::MAX,
        })
    {
        return Err(bad("PDF object zero must be free with generation 65535"));
    }
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
                    return Err(bad(
                        "Active PDF object offset is outside the object section",
                    ));
                }
                loader.object_at((*number as u32, *generation), *offset)?;
            }
            XrefEntry::Compressed { stream, index } => {
                let _ = index;
                loader.expand(*stream)?;
            }
        }
    }
    // Older revisions declare objects that later sections supersede. They are still indexed by
    // their own section, so read them only to cover their bytes; the gap verifier must not mistake
    // them for unindexed data.
    let mut covered: BTreeSet<usize> = loader.ranges.iter().map(|(start, _)| *start).collect();
    for (_, entry) in &pending {
        if let XrefEntry::Offset { offset, .. } = entry {
            covered.insert(*offset);
        }
    }
    for section in &sections {
        for (number, entry) in &section.entries {
            if let XrefEntry::Offset { offset, generation } = *entry {
                if covered.insert(offset) {
                    loader.record_at((*number as u32, generation), offset)?;
                }
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
/// Decode one stream object through its /Filter; another filter or external /F bytes are
/// UNSUPPORTED_DOCUMENT, a bound overrun OUTPUT_LIMIT, and malformed zlib a parse error.
pub fn decode(object: &Object<'_>) -> Result<Vec<u8>> {
    let raw = object.stream.ok_or_else(|| bad("Expected a PDF stream"))?;
    if raw.len() > MAX_STREAM {
        return Err(limit("PDF encoded stream exceeds 1 MiB"));
    }
    let dict = object.value.dict()?;
    if dict.contains_key(b"F".as_slice()) {
        return Err(unsupported(
            "PDF streams whose bytes live in an external file (/F) are unsupported",
        ));
    }
    let predictor = decode_parms(dict)?;
    let Some(filter) = dict.get(b"Filter".as_slice()) else {
        if predictor.is_some() {
            return Err(bad("PDF stream decode parameters require a filter"));
        }
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
            return match predictor {
                Some(predictor) => predictor.apply(output),
                None => Ok(output),
            };
        }
        if decoder.total_in() as usize == before_in && written == 0 {
            return Err(bad("Truncated PDF zlib stream"));
        }
    }
}
#[cfg(test)]
#[path = "pdf_container_tests.rs"]
mod tests;
