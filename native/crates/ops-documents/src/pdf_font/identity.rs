//! Type0 fonts, whose character codes reach Unicode only through a ToUnicode CMap.
use super::simple::{Encoding, encoding};
use super::{Codes, FontDecoder, to_unicode};
use crate::pdf_container::Document;
use crate::pdf_syntax::{Dict, bad, unsupported};
use ops_protocol::Result;
/// A Type0 font reaches Unicode only through ToUnicode; its descendant font is not needed.
pub(super) fn identity(doc: &Document<'_>, font: &Dict) -> Result<FontDecoder> {
    match encoding(doc, font)? {
        Some(Encoding::Name(name)) if name == b"Identity-H" || name == b"Identity-V" => {}
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
    let (cmap, decoded_bytes) = to_unicode(doc, value)?;
    if cmap.width() != 2 {
        return Err(unsupported(format!(
            "PDF Type0 Identity ToUnicode codespace is {} bytes wide; the encoding defines \
             two-byte codes",
            cmap.width()
        )));
    }
    Ok(FontDecoder {
        codes: Codes::Identity(cmap),
        decoded_bytes,
    })
}
