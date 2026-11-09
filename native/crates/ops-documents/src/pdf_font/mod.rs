//! Font-level character codes to Unicode: Identity-H/V CID fonts, ToUnicode CMaps and WinAnsi.
//! Glyph programs, DescendantFonts and CIDToGIDMap are never read.
mod encoding;
mod identity;
mod simple;
#[cfg(test)]
mod tests;
use crate::{
    pdf_cmap::{CMap, code},
    pdf_container::{Document, decode, get},
    pdf_limits::MAX_TEXT,
    pdf_syntax::{Value, bad, limit, unsupported},
};
pub(crate) use encoding::{standard, winansi};
use identity::identity;
use ops_protocol::Result;
use simple::simple;
/// How a PDF font supplies Unicode for its character codes; `decode` never branches on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg(test)]
pub enum Kind {
    /// Type0 with the predefined Identity-H or Identity-V encoding: two-byte character codes.
    Identity,
    /// Simple font with a `ToUnicode` CMap: one-byte character codes.
    ToUnicode,
    /// Simple font with `/WinAnsiEncoding` and no `ToUnicode` CMap: one-byte character codes.
    WinAnsi,
    /// Standard Latin Type1 font with neither `/Encoding` nor `ToUnicode`: StandardEncoding.
    Standard,
}
/// One font's character codes and the Unicode text each code produces.
pub struct FontDecoder {
    codes: Codes,
    /// Decoded ToUnicode stream bytes this decoder read.
    decoded_bytes: usize,
}
enum Codes {
    Identity(CMap),
    ToUnicode(CMap),
    WinAnsi,
    Standard,
}
impl FontDecoder {
    /// Build a decoder from a font dictionary, which may be an indirect reference; a font with no
    /// Unicode information is UNSUPPORTED_DOCUMENT and a malformed one DOCUMENT_PARSE_ERROR.
    pub fn from_value(doc: &Document<'_>, value: &Value) -> Result<Self> {
        let font = doc.resolve(value)?.dict()?;
        if let Some(font_type) = font.get(b"Type".as_slice())
            && font_type.name()? != b"Font"
        {
            return Err(bad("PDF font dictionary has a non-font /Type"));
        }
        match get(font, b"Subtype")?.name()? {
            b"Type0" => identity(doc, font),
            b"Type1" | b"MMType1" | b"TrueType" => simple(doc, font),
            subtype => Err(unsupported(format!(
                "PDF font subtype {} is unsupported; only Type0 Identity fonts and simple fonts \
                 with ToUnicode, WinAnsiEncoding or StandardEncoding are decoded",
                String::from_utf8_lossy(subtype)
            ))),
        }
    }
    /// How this font supplies Unicode.
    #[cfg(test)]
    pub fn kind(&self) -> Kind {
        match self.codes {
            Codes::Identity(_) => Kind::Identity,
            Codes::ToUnicode(_) => Kind::ToUnicode,
            Codes::WinAnsi => Kind::WinAnsi,
            Codes::Standard => Kind::Standard,
        }
    }
    /// Character-code width in bytes: two for Identity-H/V, one for a simple font.
    pub fn code_width(&self) -> usize {
        match self.codes {
            Codes::Identity(_) => 2,
            _ => 1,
        }
    }
    /// Decoded ToUnicode stream bytes this decoder read, which the text layer charges to the
    /// document's decoded-stream budget; a named or built-in encoding reports zero.
    pub fn decoded_bytes(&self) -> usize {
        self.decoded_bytes
    }
    /// Decode one text-showing string; an unmapped code and a string that is not a whole number of
    /// codes are failures rather than silently dropped characters.
    pub fn decode(&self, bytes: &[u8]) -> Result<String> {
        let width = self.code_width();
        if !bytes.len().is_multiple_of(width) {
            return Err(bad(format!(
                "PDF text string of {} bytes is not a whole number of {width}-byte character codes",
                bytes.len()
            )));
        }
        let mut output = String::new();
        for chunk in bytes.chunks_exact(width) {
            match &self.codes {
                Codes::Identity(cmap) | Codes::ToUnicode(cmap) => {
                    let text = cmap.target(code(chunk)).ok_or_else(|| {
                        unsupported(format!(
                            "PDF font has no Unicode mapping for character code {}",
                            hex(chunk)
                        ))
                    })?;
                    room(output.len(), text.len())?;
                    output.push_str(text);
                }
                Codes::WinAnsi => {
                    let character = winansi(chunk[0])?;
                    room(output.len(), character.len_utf8())?;
                    output.push(character);
                }
                Codes::Standard => {
                    let character = standard(chunk[0])?;
                    room(output.len(), character.len_utf8())?;
                    output.push(character);
                }
            }
        }
        Ok(output)
    }
}
/// Decode the ToUnicode CMap stream of a font dictionary, returning the mapping with the decoded
/// stream length, which the text layer charges to the document's decoded-stream budget.
fn to_unicode(doc: &Document<'_>, value: &Value) -> Result<(CMap, usize)> {
    let Value::Ref(number, generation) = value else {
        return Err(bad("PDF ToUnicode mapping must be an indirect stream"));
    };
    let object = doc.object((*number, *generation))?;
    if object.stream.is_none() {
        return Err(bad("PDF ToUnicode object is not a stream"));
    }
    if object.value.dict()?.contains_key(b"UseCMap".as_slice()) {
        return Err(unsupported(
            "PDF ToUnicode CMaps that inherit mappings through /UseCMap are unsupported",
        ));
    }
    let decoded = decode(object)?;
    let decoded_bytes = decoded.len();
    Ok((CMap::parse(&decoded)?, decoded_bytes))
}
/// Reject a decoded run that outgrows the shared text budget.
fn room(used: usize, bytes: usize) -> Result<()> {
    if used + bytes > MAX_TEXT {
        return Err(limit("PDF decoded text run exceeds 1 MiB"));
    }
    Ok(())
}
fn hex(code: &[u8]) -> String {
    code.iter().map(|byte| format!("{byte:02X}")).collect()
}
