//! PDF text operators for standard Latin Type1 fonts. No glyph substitution or layout inference.
use crate::{
    pdf_container::{Document, get},
    pdf_syntax::{Dict, Lexer, Value, bad, limit, unsupported},
};
use ops_protocol::{Failure, Result};
use std::time::{Duration, Instant};
pub const MAX_TEXT: usize = 1024 * 1024;
pub fn deadline(started: Instant) -> Result<()> {
    if started.elapsed() >= Duration::from_secs(30) {
        return Err(Failure::new(
            "DEADLINE_EXCEEDED",
            "PDF reading reached its 30-second deadline",
        ));
    }
    Ok(())
}
#[derive(Clone, Copy)]
enum Encoding {
    Standard,
    WinAnsi,
}
fn font(doc: &Document<'_>, resources: &Dict, name: &[u8]) -> Result<Encoding> {
    let fonts = doc.resolve(get(resources, b"Font")?)?.dict()?;
    let font = doc.resolve(get(fonts, name)?)?.dict()?;
    if get(font, b"Type")?.name()? != b"Font" || get(font, b"Subtype")?.name()? != b"Type1" {
        return Err(unsupported("PDF text requires standard Latin Type1 fonts"));
    }
    if font.contains_key(b"ToUnicode".as_slice()) || font.contains_key(b"FontDescriptor".as_slice())
    {
        return Err(unsupported(
            "PDF embedded fonts and ToUnicode mappings are unsupported",
        ));
    }
    if !matches!(
        get(font, b"BaseFont")?.name()?,
        b"Helvetica"
            | b"Helvetica-Bold"
            | b"Helvetica-Oblique"
            | b"Helvetica-BoldOblique"
            | b"Courier"
            | b"Courier-Bold"
            | b"Courier-Oblique"
            | b"Courier-BoldOblique"
            | b"Times-Roman"
            | b"Times-Bold"
            | b"Times-Italic"
            | b"Times-BoldItalic"
    ) {
        return Err(unsupported(
            "PDF font is outside the standard Helvetica/Courier/Times families",
        ));
    }
    match font.get(b"Encoding".as_slice()) {
        None => Ok(Encoding::Standard),
        Some(Value::Name(name)) if name == b"WinAnsiEncoding" => Ok(Encoding::WinAnsi),
        _ => Err(unsupported(
            "PDF font requires built-in StandardEncoding or named WinAnsiEncoding; differences and other encodings are unsupported",
        )),
    }
}
fn text(bytes: &[u8], encoding: Encoding) -> Result<String> {
    // ISO 32000-1 Annex D: WinAnsi differs from a generic CP1252 decoder at bullet/space/hyphen slots.
    const C1: [char; 32] = [
        '€', '•', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '•', 'Ž', '•', '•', '‘',
        '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', '•', 'ž', 'Ÿ',
    ];
    let mut output = String::new();
    for &byte in bytes {
        let c = match encoding {
            Encoding::Standard => match byte {
                39 => '’',
                96 => '‘',
                32..=126 => byte as char,
                _ => {
                    return Err(unsupported(
                        "PDF StandardEncoding character is outside the supported printable Latin subset",
                    ));
                }
            },
            Encoding::WinAnsi => match byte {
                32..=126 => byte as char,
                127 => '•',
                128..=159 => C1[(byte - 128) as usize],
                160 => ' ',
                173 => '-',
                161..=255 => byte as char,
                _ => {
                    return Err(unsupported(
                        "PDF WinAnsi character has no supported glyph mapping",
                    ));
                }
            },
        };
        if output.len() + c.len_utf8() > MAX_TEXT {
            return Err(limit("PDF text run exceeds 1 MiB"));
        }
        output.push(c);
    }
    Ok(output)
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
fn named(args: &[Value], count: usize) -> Result<()> {
    if args.len() != count || !args.iter().all(|arg| matches!(arg, Value::Name(_))) {
        return Err(bad("Invalid PDF name operator arguments"));
    }
    Ok(())
}
fn string(value: &Value, encoding: Encoding) -> Result<String> {
    match value {
        Value::String(bytes) => text(bytes, encoding),
        _ => Err(bad("PDF text-showing operand is not a string")),
    }
}
// Operators tolerated inside a text object although they are not text operators. Each changes
// graphics state or opens/closes a marked-content sequence: none emits a glyph, moves the text
// position or reorders runs, so skipping them cannot change the extracted text. ISO 32000-1
// section 9.4 allows only text operators between BT and ET, but WPS exports put `rg` and `gs`
// there. Operators that paint content (Do, inline images, path fills) stay rejected.
fn tolerated_in_text(op: &[u8]) -> bool {
    matches!(
        op,
        b"gs"
            | b"g"
            | b"G"
            | b"rg"
            | b"RG"
            | b"k"
            | b"K"
            | b"sc"
            | b"scn"
            | b"SC"
            | b"SCN"
            | b"BMC"
            | b"BDC"
            | b"EMC"
    )
}
pub fn extract(
    doc: &Document<'_>,
    resources: Option<&Value>,
    bytes: &[u8],
    started: Instant,
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
                selected = Some(font(doc, resources, args[0].name()?)?);
            }
            b"Tj" | b"TJ" | b"'" | b"\"" => {
                if !inside {
                    return Err(bad("PDF text show outside text object"));
                }
                let encoding = selected
                    .ok_or_else(|| bad("PDF text show before a supported font is selected"))?;
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
                            let fragment = string(part, encoding)?;
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
                        string(&args[2], encoding)?
                    }
                    _ => {
                        if args.len() != 1 {
                            return Err(bad("Invalid PDF text-show arguments"));
                        }
                        string(&args[0], encoding)?
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
                stack.push(selected)
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
            // Skipped without semantics: they either change graphics state or label content,
            // and none of them produces glyphs or reorders the text runs.
            b"gs" => named(&args, 1)?,
            b"BMC" => named(&args, 1)?,
            b"BDC" => {
                // The property list may be a name, an inline dictionary or an indirect reference;
                // it labels the content and is never traversed.
                if args.len() != 2
                    || !matches!(args[0], Value::Name(_))
                    || !matches!(args[1], Value::Name(_) | Value::Dict(_) | Value::Ref(..))
                {
                    return Err(bad("Invalid PDF marked-content BDC arguments"));
                }
            }
            b"EMC" => no_args(&args)?,
            b"sc" | b"scn" | b"SC" | b"SCN" => {
                // A name operand selects a pattern colour, whose paint is not traversed.
                if args.iter().any(|arg| matches!(arg, Value::Name(_))) {
                    return Err(unsupported(
                        "PDF pattern colours are unsupported; pattern content is not traversed",
                    ));
                }
                if args.is_empty() || args.len() > 4 || !args.iter().all(numeric) {
                    return Err(bad("Invalid PDF colour component arguments"));
                }
            }
            _ => {
                return Err(unsupported(format!(
                    "Unsupported PDF content operator {}; forms, images and shading are not traversed",
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pdf_container::parse;
    /// One page whose `/Resources` aliases Helvetica as `/F1` and `/FT8`; content is per test.
    fn document_bytes() -> Vec<u8> {
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>",
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            "<< /Type /Page /Parent 2 0 R /Resources << /Font << /F1 4 0 R /FT8 4 0 R >> >> >>",
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
        ];
        let mut pdf = b"%PDF-1.4\n%\xE2\xE3\xCF\xD3\n".to_vec();
        let mut offsets = vec![0];
        for (index, value) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.extend_from_slice(format!("{} 0 obj\n{value}\nendobj\n", index + 1).as_bytes());
        }
        let xref = pdf.len();
        pdf.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", offsets.len()).as_bytes());
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
    fn extract_text(content: &[u8]) -> Result<Vec<String>> {
        let bytes = document_bytes();
        let document = parse(&bytes).unwrap();
        let page = document.object((3, 0)).unwrap().value.dict().unwrap();
        extract(
            &document,
            page.get(b"Resources".as_slice()),
            content,
            Instant::now(),
        )
    }
    #[test]
    fn graphics_state_and_colour_inside_a_text_object_keep_the_text() {
        // A WPS export: `rg` and `gs` appear between BT and ET, before the glyphs.
        let runs = extract_text(
            b"q BT 0.212 0.376 0.569 rg /FT8 281 Tf /GS13 gs 0.05 0 0 -0.05 90 109.37 Tm <59>Tj ET Q",
        )
        .unwrap();
        assert_eq!(runs, ["Y"]);
    }
    #[test]
    fn graphics_state_and_marked_content_outside_a_text_object_are_skipped() {
        // A Chrome (Skia) export: an external graphics state, then marked content around the text.
        let runs =
            extract_text(b"q /G3 gs /NonStruct << /MCID 0 >> BDC BT /F1 12 Tf (Hi) Tj ET EMC Q")
                .unwrap();
        assert_eq!(runs, ["Hi"]);
        let runs = extract_text(
            b"/Artifact BMC 0.1 w 0.5 G 0.5 0.25 0.25 rg /Span <</MCID 1>> BDC BT /F1 12 Tf (ok) Tj ET EMC EMC",
        )
        .unwrap();
        assert_eq!(runs, ["ok"]);
    }
    #[test]
    fn marked_content_inside_a_text_object_does_not_change_the_runs() {
        let runs = extract_text(b"BT /F1 12 Tf /Span <</MCID 0>> BDC (A) Tj EMC (B) Tj ET")
            .unwrap();
        assert_eq!(runs, ["A", "B"]);
    }
    #[test]
    fn unknown_and_painting_operators_remain_unsupported_outside_text() {
        for content in [
            b"BT /F1 12 Tf (A) Tj ET QX".as_slice(),
            b"BT /F1 12 Tf (A) Tj ET /Im1 Do",
            b"BT /F1 12 Tf (A) Tj ET BI /W 1 /H 1 ID x EI",
            b"BT /F1 12 Tf (A) Tj ET /Sh1 sh",
        ] {
            let error = extract_text(content).unwrap_err();
            assert_eq!(error.code, "UNSUPPORTED_DOCUMENT", "{}", error.message);
        }
    }
    #[test]
    fn painting_operators_inside_a_text_object_are_still_rejected() {
        for content in [
            b"BT /F1 12 Tf (A) Tj /Im1 Do ET".as_slice(),
            b"BT /F1 12 Tf (A) Tj BI /W 1 /H 1 ID x EI ET",
            b"BT /F1 12 Tf (A) Tj 0 0 m 1 1 l S ET",
        ] {
            let error = extract_text(content).unwrap_err();
            assert_eq!(error.code, "DOCUMENT_PARSE_ERROR", "{}", error.message);
            assert!(
                error
                    .message
                    .contains("Non-text PDF operator inside a text object"),
                "{}",
                error.message
            );
        }
    }
    #[test]
    fn skipped_operators_still_validate_their_operands() {
        for content in [
            b"BT /F1 12 Tf (A) Tj ET gs".as_slice(),
            b"BT /F1 12 Tf (A) Tj ET 0 0 gs",
            b"BT /F1 12 Tf (A) Tj ET /A /B BMC",
            b"BT /F1 12 Tf (A) Tj ET /Span BDC",
            b"BT /F1 12 Tf (A) Tj ET /Tag EMC",
            b"BT /F1 12 Tf (A) Tj ET 1 2 3 4 rg",
            b"BT /F1 12 Tf (A) Tj ET 1 2 3 4 5 scn",
        ] {
            let error = extract_text(content).expect_err(std::str::from_utf8(content).unwrap());
            assert_eq!(error.code, "DOCUMENT_PARSE_ERROR", "{}", error.message);
        }
        let pattern = extract_text(b"BT /F1 12 Tf (A) Tj ET /P1 scn").unwrap_err();
        assert_eq!(pattern.code, "UNSUPPORTED_DOCUMENT", "{}", pattern.message);
    }
}
