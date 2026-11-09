//! PDF text operators over decoded character codes. No glyph substitution or layout inference.
use crate::{
    pdf_container::{Document, ObjectId, get},
    pdf_font::FontDecoder,
    pdf_limits::{MAX_CONTENT_DECODED, MAX_TEXT, deadline},
    pdf_syntax::{Lexer, Value, bad, limit, unsupported, white},
};
use ops_protocol::Result;
use std::{
    collections::{BTreeMap, BTreeSet},
    rc::Rc,
    time::Instant,
};
/// Fonts whose character-code tables one document may decode.
pub const MAX_FONTS: usize = 64;
/// The decoders one page has built, keyed by the /Font resource name its Tf selected; an inline
/// dictionary has no object identity, so this map charges it once per page, not once per file.
type PageFonts = BTreeMap<Box<[u8]>, Rc<FontDecoder>>;
/// The character-code decoder one Tf selected, built on the first operator that shows text.
#[derive(Clone)]
enum Selection {
    /// The /Font resource name and value; a font selected but never used cannot fail the page.
    Named(Box<[u8]>, Value),
    Decoded(Rc<FontDecoder>),
}
/// The fonts one document has decoded, keyed by the object that holds each font dictionary; a page
/// builds one on first use and later uses of the same object reuse it.
pub struct Fonts {
    cache: BTreeMap<ObjectId, Rc<FontDecoder>>,
}
impl Fonts {
    /// An empty cache; one document fills it as its pages select fonts.
    pub fn new() -> Self {
        Self {
            cache: BTreeMap::new(),
        }
    }
    /// The decoder for one /Font resource, built once per font object; decoded is the document's
    /// decoded-stream budget, so large tables spread over many fonts cannot buy decompression work.
    fn decoder(
        &mut self,
        doc: &Document<'_>,
        value: &Value,
        decoded: &mut usize,
    ) -> Result<Rc<FontDecoder>> {
        let id = font_object(value);
        if let Some(id) = id {
            if let Some(decoder) = self.cache.get(&id) {
                return Ok(Rc::clone(decoder));
            }
            if self.cache.len() >= MAX_FONTS {
                return Err(limit("PDF document selects more than 64 distinct fonts"));
            }
        }
        let decoder = Rc::new(FontDecoder::from_value(doc, value)?);
        let size = decoder.decoded_bytes();
        if *decoded + size > MAX_CONTENT_DECODED {
            return Err(limit("PDF aggregate decoded streams exceed 16 MiB"));
        }
        *decoded += size;
        if let Some(id) = id {
            self.cache.insert(id, Rc::clone(&decoder));
        }
        Ok(decoder)
    }
}
/// The object holding a font dictionary, when the /Font resource is an indirect reference.
fn font_object(value: &Value) -> Option<ObjectId> {
    match value {
        Value::Ref(number, generation) => Some((*number, *generation)),
        _ => None,
    }
}
fn numeric(value: &Value) -> bool {
    matches!(value, Value::Integer(_) | Value::Number(_))
}
fn numbers(args: &[Value], count: usize) -> Result<()> {
    if args.len() != count || !args.iter().all(numeric) {
        return Err(bad("Invalid PDF numeric operator arguments"));
    }
    Ok(())
}
fn no_args(args: &[Value]) -> Result<()> {
    numbers(args, 0)
}
/// Charge one text run against the page's 1 MiB / 10,000-run bound and append it.
fn push_run(runs: &mut Vec<String>, text_bytes: &mut usize, run: String) -> Result<()> {
    if runs.len() >= 10_000 || *text_bytes + run.len() + 1 > MAX_TEXT {
        return Err(limit("PDF page text exceeds 1 MiB or 10000 runs"));
    }
    *text_bytes += run.len() + 1;
    runs.push(run);
    Ok(())
}
fn named(args: &[Value], count: usize) -> Result<()> {
    if args.len() != count || !args.iter().all(|arg| matches!(arg, Value::Name(_))) {
        return Err(bad("Invalid PDF name operator arguments"));
    }
    Ok(())
}
fn string(value: &Value, decoder: &FontDecoder) -> Result<String> {
    match value {
        Value::String(bytes) => decoder.decode(bytes),
        _ => Err(bad("PDF text-showing operand is not a string")),
    }
}
/// The decoder of the font a Tf selected, built on first use and cached from then on.
fn selected_decoder(
    doc: &Document<'_>,
    fonts: &mut Fonts,
    page: &mut PageFonts,
    decoded: &mut usize,
    selected: &mut Option<Selection>,
) -> Result<Rc<FontDecoder>> {
    match selected.as_mut() {
        None => Err(bad("PDF text show before a font is selected")),
        Some(Selection::Decoded(decoder)) => Ok(Rc::clone(decoder)),
        Some(Selection::Named(name, value)) => {
            // This map holds the inline dictionaries the document-wide cache cannot key.
            let decoder = match page.get(name.as_ref()) {
                Some(decoder) => Rc::clone(decoder),
                None => {
                    let decoder = fonts.decoder(doc, value, decoded)?;
                    if font_object(value).is_none() {
                        page.insert(name.clone(), Rc::clone(&decoder));
                    }
                    decoder
                }
            };
            *selected = Some(Selection::Decoded(Rc::clone(&decoder)));
            Ok(decoder)
        }
    }
}
// Operators tolerated inside a text object although ISO 32000-1 9.4 allows only text operators:
// each sets graphics state, a colour space or rendering intent, or marks marked content, so none
// emits a glyph or reorders runs. WPS and LibreOffice exports put several between BT and ET.
fn tolerated_in_text(op: &[u8]) -> bool {
    matches!(
        op,
        b"gs"
            | b"g"
            | b"G"
            | b"w"
            | b"J"
            | b"j"
            | b"M"
            | b"d"
            | b"i"
            | b"ri"
            | b"rg"
            | b"RG"
            | b"k"
            | b"K"
            | b"sc"
            | b"scn"
            | b"SC"
            | b"SCN"
            | b"cs"
            | b"CS"
            | b"BMC"
            | b"BDC"
            | b"EMC"
            | b"MP"
            | b"DP"
    )
}
/// Skip one inline image whose BI operator the lexer has just read: read the parameter dictionary
/// to ID, then skip the binary data to EI. An inline image paints no glyphs, so text cannot change.
fn inline_image(lexer: &mut Lexer<'_>) -> Result<()> {
    loop {
        lexer.skip();
        let Some(&b) = lexer.data.get(lexer.at) else {
            return Err(bad("PDF inline image is missing its ID keyword"));
        };
        if b.is_ascii_digit() || b"+-./(<[/".contains(&b) {
            lexer.value(0)?;
            continue;
        }
        match lexer.word()? {
            b"ID" => break,
            // An inline image entry may be a value keyword (/IM true), not an operator.
            b"true" | b"false" | b"null" => {}
            _ => return Err(bad("Invalid PDF inline image parameter")),
        }
    }
    if lexer.data.get(lexer.at).is_none_or(|b| !white(*b)) {
        return Err(bad("PDF inline image ID is not followed by whitespace"));
    }
    lexer.at += 1;
    // EI ends the data only when whitespace surrounds it, so the same bytes inside data do not.
    for at in lexer.at..lexer.data.len().saturating_sub(1) {
        if lexer.data[at] == b'E'
            && lexer.data[at + 1] == b'I'
            && white(lexer.data[at - 1])
            && (at + 2 == lexer.data.len() || white(lexer.data[at + 2]))
        {
            lexer.at = at + 2;
            return Ok(());
        }
    }
    Err(bad("PDF inline image is missing its EI keyword"))
}
/// Extract the text runs of one page content stream in content order, following form XObjects;
/// fonts caches the decoders already selected, decoded is the document's decoded-stream budget and
/// form_bytes bounds the form content this page reads.
pub fn extract(
    doc: &Document<'_>,
    resources: Option<&Value>,
    bytes: &[u8],
    started: Instant,
    fonts: &mut Fonts,
    decoded: &mut usize,
) -> Result<Vec<String>> {
    // Blank and comment-only content is an empty page, not a content gap: the stream holds no
    // operator at all.
    let mut probe = Lexer::new(bytes);
    probe.skip();
    if probe.at == bytes.len() {
        return Ok(Vec::new());
    }
    let mut form_bytes = 0usize;
    let mut expanding = BTreeSet::new();
    let runs = traverse(
        doc,
        resources,
        bytes,
        started,
        fonts,
        decoded,
        &mut form_bytes,
        &mut expanding,
        0,
    )?;
    if runs.iter().all(String::is_empty) {
        return Err(unsupported(
            "PDF content has no supported text layer; OCR is not provided",
        ));
    }
    Ok(runs)
}
/// Extract one content stream — a page's or a form XObject's — appending the runs of the forms
/// its `Do` reaches; form_bytes bounds the form content one page decodes and expanding holds
/// the objects currently being expanded, so a form cannot reference itself into unbounded
/// recursion.
#[allow(clippy::too_many_arguments)]
pub(crate) fn traverse(
    doc: &Document<'_>,
    resources: Option<&Value>,
    bytes: &[u8],
    started: Instant,
    fonts: &mut Fonts,
    decoded: &mut usize,
    form_bytes: &mut usize,
    expanding: &mut BTreeSet<ObjectId>,
    depth: usize,
) -> Result<Vec<String>> {
    if bytes.iter().all(|b| crate::pdf_syntax::white(*b)) {
        return Ok(Vec::new());
    }
    let mut lexer = Lexer::new(bytes);
    lexer.skip();
    if lexer.at == bytes.len() {
        return Ok(Vec::new());
    }
    let mut args = Vec::new();
    let (mut inside, mut selected, mut stack) = (false, None, Vec::new());
    let (mut runs, mut text_bytes) = (Vec::new(), 0usize);
    let mut page_fonts = PageFonts::new();
    loop {
        deadline(started)?;
        lexer.skip();
        if lexer.at == bytes.len() {
            break;
        }
        let b = bytes[lexer.at];
        if b.is_ascii_digit() || b"+-./(<[".contains(&b) {
            if args.len() >= 64 {
                return Err(limit("PDF operator exceeds 64 operands"));
            }
            args.push(lexer.value(0)?);
            continue;
        }
        let op = lexer.word()?;
        if inside
            && !matches!(
                op,
                b"ET"
                    | b"Tf"
                    | b"Tj"
                    | b"TJ"
                    | b"'"
                    | b"\""
                    | b"T*"
                    | b"Td"
                    | b"TD"
                    | b"Tm"
                    | b"Tc"
                    | b"Tw"
                    | b"Tz"
                    | b"TL"
                    | b"Ts"
                    | b"Tr"
                    | b"BT"
            )
            && !tolerated_in_text(op)
        {
            return Err(bad("Non-text PDF operator inside a text object"));
        }
        let mut shown = None;
        match op {
            b"BT" => {
                no_args(&args)?;
                if inside {
                    return Err(bad("Nested PDF text objects"));
                }
                inside = true
            }
            b"ET" => {
                no_args(&args)?;
                if !inside {
                    return Err(bad("PDF ET outside text object"));
                }
                inside = false
            }
            b"Tf" => {
                if !inside || args.len() != 2 || !numeric(&args[1]) {
                    return Err(bad("Invalid PDF Tf text state"));
                }
                let resources = doc
                    .resolve(resources.ok_or_else(|| bad("Missing PDF page Resources"))?)?
                    .dict()?;
                let font_resources = doc.resolve(get(resources, b"Font")?)?.dict()?;
                // Only the resource lookup happens here: the font is decoded when text is shown.
                let name = args[0].name()?;
                let value = get(font_resources, name)?;
                selected = Some(Selection::Named(name.into(), value.clone()));
            }
            b"Tj" | b"TJ" | b"'" | b"\"" => {
                if !inside {
                    return Err(bad("PDF text show outside text object"));
                }
                let decoder =
                    selected_decoder(doc, fonts, &mut page_fonts, decoded, &mut selected)?;
                shown = Some(match op {
                    b"TJ" => {
                        if args.len() != 1 {
                            return Err(bad("Invalid PDF TJ arguments"));
                        }
                        let Value::Array(parts) = &args[0] else {
                            return Err(bad("PDF TJ operand is not an array"));
                        };
                        let mut run = String::new();
                        for part in parts {
                            if numeric(part) {
                                continue;
                            }
                            let fragment = string(part, &decoder)?;
                            if run.len() + fragment.len() > MAX_TEXT {
                                return Err(limit("PDF text run exceeds 1 MiB"));
                            }
                            run.push_str(&fragment);
                        }
                        run
                    }
                    b"\"" => {
                        if args.len() != 3 {
                            return Err(bad("Invalid PDF quote arguments"));
                        }
                        numbers(&args[..2], 2)?;
                        string(&args[2], &decoder)?
                    }
                    _ => {
                        if args.len() != 1 {
                            return Err(bad("Invalid PDF text-show arguments"));
                        }
                        string(&args[0], &decoder)?
                    }
                });
            }
            b"T*" | b"Td" | b"TD" | b"Tm" | b"Tc" | b"Tw" | b"Tz" | b"TL" | b"Ts" | b"Tr" => {
                if !inside {
                    return Err(bad("PDF text positioning outside text object"));
                }
                let count = match op {
                    b"T*" => 0,
                    b"Td" | b"TD" => 2,
                    b"Tm" => 6,
                    _ => 1,
                };
                numbers(&args, count)?;
                if op == b"Tr" && !matches!(args[0], Value::Integer(0..=7)) {
                    return Err(bad("Invalid PDF text rendering mode"));
                }
            }
            b"q" => {
                no_args(&args)?;
                if stack.len() >= 64 {
                    return Err(limit("PDF graphics state exceeds depth 64"));
                }
                stack.push(selected.clone())
            }
            b"Q" => {
                no_args(&args)?;
                selected = stack.pop().ok_or_else(|| bad("Unbalanced PDF Q"))?
            }
            b"cm" | b"c" => numbers(&args, 6)?,
            b"v" | b"y" | b"re" | b"k" | b"K" => numbers(&args, 4)?,
            b"rg" | b"RG" => numbers(&args, 3)?,
            b"m" | b"l" => numbers(&args, 2)?,
            b"w" | b"J" | b"j" | b"M" | b"i" | b"g" | b"G" => numbers(&args, 1)?,
            b"h" | b"S" | b"s" | b"f" | b"F" | b"f*" | b"B" | b"B*" | b"b" | b"b*" | b"n"
            | b"W" | b"W*" => no_args(&args)?,
            b"d" => {
                if args.len() != 2
                    || !numeric(&args[1])
                    || !matches!(&args[0],Value::Array(v) if v.iter().all(numeric))
                {
                    return Err(bad("Invalid PDF dash arguments"));
                }
            }
            b"ri" => {
                if args.len() != 1 {
                    return Err(bad("Invalid PDF rendering intent arguments"));
                }
                args[0].name()?;
            }
            // Paint that carries no text of its own: image XObjects, shadings and inline images
            // emit no glyph, so skipping one cannot change the runs. A form XObject may hold text
            // of its own, so its content stream is traversed within the depth and byte bounds.
            b"Do" => {
                named(&args, 1)?;
                let name = args[0].name()?;
                let nested = crate::pdf_form::paint(
                    doc, resources, name, started, fonts, decoded, form_bytes, expanding, depth,
                )?;
                for run in nested {
                    push_run(&mut runs, &mut text_bytes, run)?;
                }
            }
            b"BI" => {
                if !args.is_empty() {
                    return Err(bad("Invalid PDF inline image operands"));
                }
                inline_image(&mut lexer)?;
            }
            b"sh" => named(&args, 1)?,
            // Skipped without semantics: these change graphics state or label content only.
            b"gs" => named(&args, 1)?,
            b"cs" | b"CS" => {
                // The colour space is named here; its components arrive later and paint nothing.
                if args.len() != 1 || !matches!(args[0], Value::Name(_)) {
                    return Err(bad("Invalid PDF colour space arguments"));
                }
            }
            b"BMC" => named(&args, 1)?,
            b"BDC" => {
                // The property list may be a name, dictionary or reference; it is never traversed.
                if args.len() != 2
                    || !matches!(args[0], Value::Name(_))
                    || !matches!(args[1], Value::Name(_) | Value::Dict(_) | Value::Ref(..))
                {
                    return Err(bad("Invalid PDF marked-content BDC arguments"));
                }
            }
            b"EMC" => no_args(&args)?,
            b"MP" => {
                if args.len() != 1 || !matches!(args[0], Value::Name(_)) {
                    return Err(bad("Invalid PDF marked-content MP arguments"));
                }
            }
            b"DP" => {
                // The property list is optional and labels a point; it is never traversed.
                if args.is_empty()
                    || args.len() > 2
                    || !matches!(args[0], Value::Name(_))
                    || (args.len() == 2
                        && !matches!(args[1], Value::Name(_) | Value::Dict(_) | Value::Ref(..)))
                {
                    return Err(bad("Invalid PDF marked-content DP arguments"));
                }
            }
            b"sc" | b"scn" | b"SC" | b"SCN" => {
                // 1-4 components with an optional trailing pattern name, or one name for a coloured
                // pattern. The pattern's own content is not traversed by this layer, so text it
                // paints is excluded; the operands are checked and then skipped.
                let components = match args.last() {
                    Some(Value::Name(_)) => &args[..args.len() - 1],
                    _ => &args[..],
                };
                if args.is_empty() || components.len() > 4 || !components.iter().all(numeric) {
                    return Err(bad(
                        "Invalid PDF colour components: 1-4 numbers or one pattern name",
                    ));
                }
            }
            _ => {
                return Err(unsupported(format!(
                    "Unsupported PDF content operator {}; unknown operators are not traversed",
                    String::from_utf8_lossy(op)
                )));
            }
        }
        if let Some(run) = shown {
            push_run(&mut runs, &mut text_bytes, run)?;
        }
        args.clear();
    }
    if inside || !stack.is_empty() || !args.is_empty() {
        return Err(bad("Unclosed PDF text/graphics state or dangling operands"));
    }
    Ok(runs)
}
