//! Font-level character codes to Unicode: Identity-H/V CID fonts, `ToUnicode` CMaps and WinAnsi.
//!
//! Character codes are split by the width the font's encoding defines, then looked up in its
//! `ToUnicode` CMap or in the named encoding. Glyph programs, `DescendantFonts` and `CIDToGIDMap`
//! are never read: a `ToUnicode` CMap maps character codes directly, and under Identity-H/V the
//! character code is the CID.
use crate::{
    pdf_cmap::{CMap, code},
    pdf_container::{Document, decode, get},
    pdf_syntax::{Dict, Value, bad, limit, unsupported},
    pdf_text::MAX_TEXT,
};
use ops_protocol::Result;
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
}
enum Codes {
    Identity(CMap),
    ToUnicode(CMap),
    WinAnsi,
    Standard,
}
impl FontDecoder {
    /// Build a decoder from a font dictionary, which may be an indirect reference.
    ///
    /// Fails with `UNSUPPORTED_DOCUMENT` when the font carries no Unicode information (a Type0
    /// Identity font without `ToUnicode`, a custom CMap, an inherited CMap, or a built-in encoding
    /// this reader does not implement) and with `DOCUMENT_PARSE_ERROR` when it is malformed.
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
    /// Decode one text-showing string.
    ///
    /// A code the font does not map, and a string whose length is not a whole number of codes,
    /// are failures rather than silently dropped characters.
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
/// A Type0 font reaches Unicode only through `ToUnicode`; its descendant font is not needed.
fn identity(doc: &Document<'_>, font: &Dict) -> Result<FontDecoder> {
    match font.get(b"Encoding".as_slice()) {
        Some(Value::Name(encoding)) if encoding == b"Identity-H" || encoding == b"Identity-V" => {}
        Some(_) => {
            return Err(unsupported(
                "PDF Type0 font requires the predefined Identity-H or Identity-V encoding; \
                 embedded and custom CMaps are unsupported",
            ));
        }
        None => return Err(bad("PDF Type0 font is missing its Encoding")),
    }
    let Some(value) = font.get(b"ToUnicode".as_slice()) else {
        return Err(unsupported(
            "PDF Type0 Identity font has no ToUnicode mapping, so its character codes carry no \
             Unicode information",
        ));
    };
    let cmap = to_unicode(doc, value)?;
    if cmap.width() != 2 {
        return Err(unsupported(format!(
            "PDF Type0 Identity ToUnicode codespace is {} bytes wide; the encoding defines \
             two-byte codes",
            cmap.width()
        )));
    }
    Ok(FontDecoder {
        codes: Codes::Identity(cmap),
    })
}
/// A simple font is decoded from its `ToUnicode` CMap, else from its named encoding.
fn simple(doc: &Document<'_>, font: &Dict) -> Result<FontDecoder> {
    if let Some(value) = font.get(b"ToUnicode".as_slice()) {
        let cmap = to_unicode(doc, value)?;
        if cmap.width() != 1 {
            return Err(unsupported(format!(
                "PDF simple font ToUnicode codespace is {} bytes wide; a simple font shows \
                 one-byte codes",
                cmap.width()
            )));
        }
        return Ok(FontDecoder {
            codes: Codes::ToUnicode(cmap),
        });
    }
    match font.get(b"Encoding".as_slice()) {
        Some(Value::Name(name)) if name == b"WinAnsiEncoding" => Ok(FontDecoder {
            codes: Codes::WinAnsi,
        }),
        Some(Value::Name(name)) => Err(unsupported(format!(
            "PDF font encoding /{} is unsupported without a ToUnicode mapping",
            String::from_utf8_lossy(name)
        ))),
        Some(_) => Err(unsupported(
            "PDF font encodings with character differences or indirect CMaps are unsupported \
             without a ToUnicode mapping",
        )),
        None => Ok(FontDecoder {
            codes: builtin(font)?,
        }),
    }
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
/// Decode the `ToUnicode` CMap stream of a font dictionary.
fn to_unicode(doc: &Document<'_>, value: &Value) -> Result<CMap> {
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
    CMap::parse(&decode(object)?)
}
/// ISO 32000-1 Annex D WinAnsi table.
///
/// This is the single WinAnsi table in the crate: the classic standard-encoding path in
/// `pdf_text` should call it instead of carrying its own copy.
pub fn winansi(byte: u8) -> Result<char> {
    // Annex D differs from a generic CP1252 decoder at the bullet, space and hyphen slots.
    const C1: [char; 32] = [
        '€', '•', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '•', 'Ž', '•', '•', '‘',
        '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', '•', 'ž', 'Ÿ',
    ];
    match byte {
        32..=126 => Ok(byte as char),
        127 => Ok('•'),
        128..=159 => Ok(C1[usize::from(byte - 128)]),
        160 => Ok(' '),
        173 => Ok('-'),
        161..=255 => Ok(byte as char),
        _ => Err(unsupported(
            "PDF WinAnsi character has no supported glyph mapping",
        )),
    }
}
/// ISO 32000-1 Annex D StandardEncoding, restricted to the printable Latin subset.
pub fn standard(byte: u8) -> Result<char> {
    match byte {
        39 => Ok('’'),
        96 => Ok('‘'),
        32..=126 => Ok(byte as char),
        _ => Err(unsupported(
            "PDF StandardEncoding character is outside the supported printable Latin subset",
        )),
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pdf_container::parse;
    const HELVETICA: &str = "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>";
    /// One-page PDF whose `/F1` resource is `font`; `cmap` becomes the `ToUnicode` stream of 5 0 R.
    fn document(font: &str, cmap: Option<(&[u8], &str)>) -> Vec<u8> {
        let mut objects = vec![
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /Resources << /Font << /F1 4 0 R >> >> >>".to_vec(),
            font.as_bytes().to_vec(),
        ];
        if let Some((body, extra)) = cmap {
            let mut object = format!("<< /Length {} {extra}>>\nstream\n", body.len()).into_bytes();
            object.extend_from_slice(body);
            object.extend_from_slice(b"\nendstream");
            objects.push(object);
        }
        let mut pdf = b"%PDF-1.4\n%\xE2\xE3\xCF\xD3\n".to_vec();
        let mut offsets = vec![0];
        for (index, object) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.extend_from_slice(format!("{} 0 obj\n", index + 1).as_bytes());
            pdf.extend_from_slice(object);
            pdf.extend_from_slice(b"\nendobj\n");
        }
        let xref = pdf.len();
        pdf.extend_from_slice(
            format!("xref\n0 {}\n0000000000 65535 f \n", offsets.len()).as_bytes(),
        );
        for offset in offsets.iter().skip(1) {
            pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        pdf.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                offsets.len()
            )
            .as_bytes(),
        );
        pdf
    }
    /// A CMap over the given codespace with `body` after it.
    fn cmap(codespace: &str, body: &str) -> Vec<u8> {
        format!(
            "/CIDInit /ProcSet findresource begin\n12 dict begin\nbegincmap\n\
             /CMapType 2 def\n1 begincodespacerange\n{codespace}\nendcodespacerange\n{body}\n\
             endcmap CMapName currentdict /CMap defineresource pop end end\n"
        )
        .into_bytes()
    }
    /// Build a decoder for the `/F1` font of a one-page document.
    fn decoder(font: &str, to_unicode: Option<(&[u8], &str)>) -> Result<FontDecoder> {
        let bytes = document(font, to_unicode);
        let pdf = parse(&bytes).expect("the fixture document must parse");
        FontDecoder::from_value(&pdf, &Value::Ref(4, 0))
    }
    fn rejected(font: &str, cmap: Option<(&[u8], &str)>, code: &str) -> String {
        let error = decoder(font, cmap).err().expect("the font must be rejected");
        assert_eq!(error.code, code, "{}", error.message);
        error.message
    }
    /// Bytes of a producer export the repository keeps as a fixture.
    fn fixture(name: &str) -> Vec<u8> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name);
        std::fs::read(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
    }
    /// The decoder of the one font in `name` whose `/BaseFont` contains `family`.
    fn fixture_font(name: &str, family: &[u8]) -> FontDecoder {
        let bytes = fixture(name);
        let document = parse(&bytes).expect("the fixture must parse");
        for object in document.objects.values() {
            let Ok(dict) = object.value.dict() else {
                continue;
            };
            let base = dict
                .get(b"BaseFont".as_slice())
                .and_then(|value| value.name().ok());
            if base.is_some_and(|base| base.windows(family.len()).any(|part| part == family)) {
                let family = String::from_utf8_lossy(family);
                return FontDecoder::from_value(&document, &object.value)
                    .unwrap_or_else(|error| panic!("{name} font {family}: {}", error.message));
            }
        }
        panic!("{} has no font matching {}", name, String::from_utf8_lossy(family));
    }
    const IDENTITY: &str = "<< /Type /Font /Subtype /Type0 /BaseFont /AAAAAA+SimSun \
                            /Encoding /Identity-H /DescendantFonts [6 0 R] /ToUnicode 5 0 R >>";
    const SIMPLE: &str = "<< /Type /Font /Subtype /TrueType /BaseFont /BAAAAA+LiberationMono \
                          /FirstChar 0 /LastChar 2 /ToUnicode 5 0 R >>";
    #[test]
    fn identity_font_decodes_two_byte_codes_without_its_descendant_font() {
        let table = cmap(
            "<0000> <FFFF>",
            "2 beginbfchar\n<0E3C> <4E2D>\n<0003> <0020>\nendbfchar",
        );
        let decoder = decoder(IDENTITY, Some((table.as_slice(), ""))).unwrap();
        assert_eq!(decoder.kind(), Kind::Identity);
        assert_eq!(decoder.code_width(), 2);
        assert_eq!(decoder.decode(&[0x0e, 0x3c]).unwrap(), "中");
        assert_eq!(decoder.decode(&[0x0e, 0x3c, 0x00, 0x03]).unwrap(), "中 ");
        assert_eq!(decoder.decode(&[]).unwrap(), "");
    }
    #[test]
    fn simple_font_decodes_one_byte_codes_from_its_to_unicode_cmap() {
        let table = cmap("<00> <FF>", "2 beginbfchar\n<01> <0051>\n<02> <0033>\nendbfchar");
        let decoder = decoder(SIMPLE, Some((table.as_slice(), ""))).unwrap();
        assert_eq!(decoder.kind(), Kind::ToUnicode);
        assert_eq!(decoder.code_width(), 1);
        assert_eq!(decoder.decode(&[1, 2]).unwrap(), "Q3");
    }
    #[test]
    fn type0_identity_without_a_to_unicode_mapping_is_unsupported() {
        let font = "<< /Type /Font /Subtype /Type0 /BaseFont /AAAAAA+SimSun \
                    /Encoding /Identity-V /DescendantFonts [6 0 R] >>";
        let message = rejected(font, None, "UNSUPPORTED_DOCUMENT");
        assert!(message.contains("no ToUnicode mapping"), "{message}");
    }
    #[test]
    fn type0_fonts_must_use_the_predefined_identity_encoding() {
        let table = cmap("<0000> <FFFF>", "1 beginbfchar\n<0001> <0041>\nendbfchar");
        let message = rejected(
            "<< /Type /Font /Subtype /Type0 /BaseFont /X /Encoding /UniGB-UCS2-H \
              /ToUnicode 5 0 R >>",
            Some((table.as_slice(), "")),
            "UNSUPPORTED_DOCUMENT",
        );
        assert!(message.contains("Identity-H or Identity-V"), "{message}");
        rejected(
            "<< /Type /Font /Subtype /Type0 /BaseFont /X /ToUnicode 5 0 R >>",
            Some((table.as_slice(), "")),
            "DOCUMENT_PARSE_ERROR",
        );
    }
    #[test]
    fn a_codespace_that_disagrees_with_the_font_encoding_is_unsupported() {
        let two_byte = cmap("<0000> <FFFF>", "1 beginbfchar\n<0001> <0041>\nendbfchar");
        let message = rejected(SIMPLE, Some((two_byte.as_slice(), "")), "UNSUPPORTED_DOCUMENT");
        assert!(message.contains("one-byte codes"), "{message}");
        let font = "<< /Type /Font /Subtype /Type0 /BaseFont /X /Encoding /Identity-H \
                    /ToUnicode 5 0 R >>";
        let one_byte = cmap("<00> <FF>", "1 beginbfchar\n<01> <0041>\nendbfchar");
        let message = rejected(font, Some((one_byte.as_slice(), "")), "UNSUPPORTED_DOCUMENT");
        assert!(message.contains("two-byte codes"), "{message}");
    }
    #[test]
    fn winansi_font_decodes_without_a_to_unicode_mapping() {
        let font = "<< /Type /Font /Subtype /TrueType /BaseFont /BCDEEE+Calibri \
                    /Encoding /WinAnsiEncoding \
                    /FontDescriptor << /Type /FontDescriptor /FontName /BCDEEE+Calibri >> >>";
        let decoder = decoder(font, None).unwrap();
        assert_eq!(decoder.kind(), Kind::WinAnsi);
        assert_eq!(decoder.code_width(), 1);
        assert_eq!(decoder.decode(b"Q3").unwrap(), "Q3");
        assert_eq!(
            decoder.decode(&[0x93, 0x94, 0x91, 0x7f, 0xa0, 0xad]).unwrap(),
            "“”‘• -"
        );
        assert_eq!(
            decoder.decode(&[0xe9]).unwrap(),
            "é",
            "WinAnsi keeps Latin-1 letters"
        );
        assert_eq!(
            decoder
                .decode(&[0u8])
                .expect_err("control bytes have no glyph")
                .code,
            "UNSUPPORTED_DOCUMENT"
        );
    }
    #[test]
    fn standard_type1_fonts_keep_their_built_in_encoding() {
        let decoder = decoder(HELVETICA, None).unwrap();
        assert_eq!(decoder.kind(), Kind::Standard);
        assert_eq!(decoder.decode(b"Y").unwrap(), "Y");
        assert_eq!(decoder.decode(&[0x27, 0x60]).unwrap(), "’‘");
        let message = rejected(
            "<< /Type /Font /Subtype /Type1 /BaseFont /Symbol >>",
            None,
            "UNSUPPORTED_DOCUMENT",
        );
        assert!(message.contains("standard Helvetica/Courier/Times"), "{message}");
        rejected(
            "<< /Type /Font /Subtype /TrueType /BaseFont /Arial >>",
            None,
            "UNSUPPORTED_DOCUMENT",
        );
        rejected(
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /FontDescriptor 6 0 R >>",
            None,
            "UNSUPPORTED_DOCUMENT",
        );
        rejected(
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /MacRomanEncoding >>",
            None,
            "UNSUPPORTED_DOCUMENT",
        );
        rejected(
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica \
              /Encoding << /Type /Encoding /Differences [65 /A] >> >>",
            None,
            "UNSUPPORTED_DOCUMENT",
        );
    }
    #[test]
    fn unmapped_and_truncated_character_codes_are_failures() {
        let table = cmap("<0000> <FFFF>", "1 beginbfchar\n<0041> <0059>\nendbfchar");
        let identity = decoder(IDENTITY, Some((table.as_slice(), ""))).unwrap();
        let message = identity.decode(&[0x00, 0x42]).unwrap_err().message;
        assert!(message.contains("character code 0042"), "{message}");
        assert_eq!(
            identity.decode(&[0x00, 0x41, 0x00]).unwrap_err().code,
            "DOCUMENT_PARSE_ERROR"
        );
        let table = cmap("<00> <FF>", "1 beginbfchar\n<41> <0059>\nendbfchar");
        let simple = decoder(SIMPLE, Some((table.as_slice(), ""))).unwrap();
        assert_eq!(
            simple.decode(&[0x42]).unwrap_err().code,
            "UNSUPPORTED_DOCUMENT"
        );
    }
    #[test]
    fn inherited_cmaps_are_unsupported() {
        let table = cmap("<0000> <FFFF>", "1 beginbfchar\n<0041> <0059>\nendbfchar");
        let message = rejected(
            IDENTITY,
            Some((table.as_slice(), "/UseCMap /Identity-H ")),
            "UNSUPPORTED_DOCUMENT",
        );
        assert!(message.contains("/UseCMap"), "{message}");
        let message = rejected(
            IDENTITY,
            Some((b"begincmap usecmap endcmap".as_slice(), "")),
            "UNSUPPORTED_DOCUMENT",
        );
        assert!(message.contains("usecmap"), "{message}");
    }
    #[test]
    fn other_font_subtypes_are_unsupported() {
        let message = rejected(
            "<< /Type /Font /Subtype /Type3 /FontMatrix [1 0 0 1 0 0] >>",
            None,
            "UNSUPPORTED_DOCUMENT",
        );
        assert!(message.contains("Type3"), "{message}");
        rejected(
            "<< /Type /XObject /Subtype /Type1 /BaseFont /Helvetica >>",
            None,
            "DOCUMENT_PARSE_ERROR",
        );
    }
    /// The WPS export embeds a SimSun subset whose ToUnicode CMap maps two-byte identity codes.
    #[test]
    fn the_wps_export_decodes_its_embedded_chinese_font() {
        let decoder = fixture_font("producer-wps.pdf", b"SimSun");
        assert_eq!(decoder.kind(), Kind::Identity);
        assert_eq!(decoder.code_width(), 2);
        assert_eq!(decoder.decode(&[0x04, 0x5d]).unwrap(), "中");
        // Two consecutive codes split on the two-byte width and keep CMap order.
        assert_eq!(decoder.decode(&[0x04, 0x5d, 0x0a, 0x7a]).unwrap(), "中告");
    }
    /// The LibreOffice export is a simple TrueType font with one-byte codes and no /Encoding.
    #[test]
    fn the_libreoffice_export_decodes_its_one_byte_font() {
        let decoder = fixture_font("libreoffice-export.pdf", b"LiberationMono");
        assert_eq!(decoder.kind(), Kind::ToUnicode);
        assert_eq!(decoder.code_width(), 1);
        assert_eq!(
            decoder.decode(&[1, 2, 3, 4, 5, 6, 7, 8, 9]).unwrap(),
            "Q3 Report"
        );
    }
}
