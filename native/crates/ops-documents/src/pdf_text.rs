//! PDF text operators over decoded character codes. No glyph substitution or layout inference.
use crate::{
    pdf_container::{Document, ObjectId, get},
    pdf_font::FontDecoder,
    pdf_limits::{MAX_CONTENT_DECODED, MAX_TEXT, deadline},
    pdf_syntax::{Lexer, Value, bad, limit, unsupported},
};
use ops_protocol::Result;
use std::{collections::BTreeMap, rc::Rc, time::Instant};
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
pub fn extract(
    doc: &Document<'_>,
    resources: Option<&Value>,
    bytes: &[u8],
    started: Instant,
    fonts: &mut Fonts,
    decoded: &mut usize,
) -> Result<Vec<String>> {
    // Inline font dictionaries have no object identity, so each page charges and caches its own.
    let mut page_fonts = PageFonts::new();
    if bytes.iter().all(|b| crate::pdf_syntax::white(*b)) {
        return Ok(Vec::new());
    }
    let mut lexer = Lexer::new(bytes);
    lexer.skip();
    if lexer.at == bytes.len() {
        return Ok(Vec::new());
    }
    let mut args = Vec::new();
    let mut inside = false;
    let mut selected: Option<Selection> = None;
    let mut stack: Vec<Option<Selection>> = Vec::new();
    let (mut runs, mut text_bytes) = (Vec::new(), 0usize);
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
                if inside {
                    return Err(unsupported("PDF q inside a text object is unsupported"));
                }
                if stack.len() >= 64 {
                    return Err(limit("PDF graphics state exceeds depth 64"));
                }
                stack.push(selected.clone())
            }
            b"Q" => {
                no_args(&args)?;
                if inside {
                    return Err(unsupported("PDF Q inside a text object is unsupported"));
                }
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
            _ => {
                return Err(unsupported(format!(
                    "Unsupported PDF content operator {}; forms, images, marked content and external graphics state are not traversed",
                    String::from_utf8_lossy(op)
                )));
            }
        }
        if let Some(run) = shown {
            if runs.len() >= 10_000 || text_bytes + run.len() + 1 > MAX_TEXT {
                return Err(limit("PDF page text exceeds 1 MiB or 10000 runs"));
            }
            text_bytes += run.len() + 1;
            runs.push(run);
        }
        args.clear();
    }
    if inside || !stack.is_empty() || !args.is_empty() {
        return Err(bad("Unclosed PDF text/graphics state or dangling operands"));
    }
    if runs.iter().all(String::is_empty) {
        return Err(unsupported(
            "PDF content has no supported text layer; OCR is not provided",
        ));
    }
    Ok(runs)
}
