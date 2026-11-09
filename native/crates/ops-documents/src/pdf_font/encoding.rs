//! The two ISO 32000-1 Annex D encodings this reader maps: WinAnsi and the standard Latin subset.
use crate::pdf_syntax::unsupported;
use ops_protocol::Result;
/// ISO 32000-1 Annex D WinAnsiEncoding; differs from CP1252 at the bullet, space and hyphen slots.
pub fn winansi(byte: u8) -> Result<char> {
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
