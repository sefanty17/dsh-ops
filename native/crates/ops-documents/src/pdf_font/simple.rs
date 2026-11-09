//! Simple fonts: a one-byte ToUnicode CMap, a named encoding, or the standard Latin encoding.
use super::{Codes, FontDecoder, to_unicode};
use crate::pdf_container::{Document, get};
use crate::pdf_syntax::{Dict, Value, bad, limit, unsupported};
use ops_protocol::Result;
use std::collections::BTreeSet;
/// A simple font is decoded from its ToUnicode CMap, else from its named encoding.
pub(super) fn simple(doc: &Document<'_>, font: &Dict) -> Result<FontDecoder> {
    if let Some(value) = font.get(b"ToUnicode".as_slice()) {
        let (cmap, decoded_bytes) = to_unicode(doc, value)?;
        if cmap.width() != 1 {
            return Err(unsupported(format!(
                "PDF simple font ToUnicode codespace is {} bytes wide; a simple font shows \
                 one-byte codes",
                cmap.width()
            )));
        }
        return Ok(FontDecoder {
            codes: Codes::ToUnicode(cmap),
            decoded_bytes,
        });
    }
    match encoding(doc, font)? {
        Some(Encoding::Name(name)) if name == b"WinAnsiEncoding" => Ok(FontDecoder {
            codes: Codes::WinAnsi,
            decoded_bytes: 0,
        }),
        Some(Encoding::Name(name)) => Err(unsupported(format!(
            "PDF font encoding /{} is unsupported without a ToUnicode mapping",
            String::from_utf8_lossy(name)
        ))),
        Some(Encoding::Unsupported) => Err(unsupported(
            "PDF font encodings with character differences or indirect CMaps are unsupported \
             without a ToUnicode mapping",
        )),
        None => Ok(FontDecoder {
            codes: builtin(font)?,
            decoded_bytes: 0,
        }),
    }
}
/// A font dictionary's /Encoding entry, with indirect references followed.
pub(super) enum Encoding<'a> {
    /// A predefined encoding name.
    Name(&'a [u8]),
    /// A differences dictionary or a CMap stream, neither of which this reader interprets.
    Unsupported,
}
/// The /Encoding entry of font, which may be an indirect reference; a stream target is a CMap, and
/// Document::resolve refuses a stream as a value, so this walk reports one as unsupported instead.
pub(super) fn encoding<'a>(doc: &'a Document<'_>, font: &'a Dict) -> Result<Option<Encoding<'a>>> {
    let Some(value) = font.get(b"Encoding".as_slice()) else {
        return Ok(None);
    };
    let mut current = value;
    let mut seen = BTreeSet::new();
    while let Value::Ref(number, generation) = current {
        if seen.len() >= 64 {
            return Err(limit("PDF reference chain exceeds 64"));
        }
        if !seen.insert((*number, *generation)) {
            return Err(bad("Cyclic PDF reference"));
        }
        let object = doc.object((*number, *generation))?;
        if object.stream.is_some() {
            return Ok(Some(Encoding::Unsupported));
        }
        current = &object.value;
    }
    Ok(Some(match current {
        Value::Name(name) => Encoding::Name(name),
        _ => Encoding::Unsupported,
    }))
}
/// Built-in encoding of a plain standard Latin Type1 font, the only font PDF defines one for.
fn builtin(font: &Dict) -> Result<Codes> {
    if get(font, b"Subtype")?.name()? != b"Type1" {
        return Err(unsupported(
            "PDF font without an /Encoding must be a standard Latin Type1 font",
        ));
    }
    if font.contains_key(b"FontDescriptor".as_slice()) {
        return Err(unsupported(
            "PDF embedded fonts require a ToUnicode mapping or /WinAnsiEncoding",
        ));
    }
    match get(font, b"BaseFont")?.name()? {
        b"Helvetica" | b"Helvetica-Bold" | b"Helvetica-Oblique" | b"Helvetica-BoldOblique"
        | b"Courier" | b"Courier-Bold" | b"Courier-Oblique" | b"Courier-BoldOblique"
        | b"Times-Roman" | b"Times-Bold" | b"Times-Italic" | b"Times-BoldItalic" => {
            Ok(Codes::Standard)
        }
        _ => Err(unsupported(
            "PDF font is outside the standard Helvetica/Courier/Times families",
        )),
    }
}
