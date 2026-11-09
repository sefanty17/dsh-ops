//! CMap mappings and the four font decoding paths.
mod common;

use common::{cmap, font_document, read, stream, text};
use serde_json::json;

const TWO: &str = "<0000> <FFFF>";
const ONE: &str = "<00> <FF>";
const IDENTITY: &str = "<< /Type /Font /Subtype /Type0 /BaseFont /AAAAAA+SimSun \
                        /Encoding /Identity-H /ToUnicode 6 0 R >>";
const SIMPLE: &str = "<< /Type /Font /Subtype /TrueType /BaseFont /BAAAAA+LiberationMono \
                      /FirstChar 0 /LastChar 2 /ToUnicode 6 0 R >>";
const WINANSI: &str = "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica \
                       /Encoding /WinAnsiEncoding >>";

fn shown(font: &str, extras: &[Vec<u8>], content: &[u8]) -> serde_json::Value {
    read(&font_document(font, extras, content), json!({})).expect("read_pdf returns an observation")
}

fn font_text(font: &str, extras: &[Vec<u8>], content: &[u8]) -> String {
    let output = shown(font, extras, content);
    assert_eq!(output["status"], "completed", "{output}");
    text(&output).to_owned()
}

fn to_unicode(body: &str) -> Vec<u8> {
    stream("", &cmap(TWO, body))
}

fn one_byte_unicode(body: &str) -> Vec<u8> {
    stream("", &cmap(ONE, body))
}

/// A CMap with `body` inside `begincmap`, for bodies that declare their own codespaces.
fn raw_cmap(body: &str) -> Vec<u8> {
    stream(
        "",
        format!(
            "/CIDInit /ProcSet findresource begin\n12 dict begin\nbegincmap\n{body}\n\
             endcmap CMapName currentdict /CMap defineresource pop end end\n"
        )
        .as_bytes(),
    )
}

fn font_failure(font: &str, extras: &[Vec<u8>], code: &str) -> String {
    let output = shown(font, extras, b"BT /F1 12 Tf (A) Tj ET");
    assert_eq!(output["status"], "partial", "{output}");
    assert_eq!(output["error"]["code"], code, "{output}");
    output["error"]["message"].as_str().expect("failure message").to_owned()
}

#[test]
fn identity_font_maps_bfchar_bfrange_and_surrogates() {
    let table = to_unicode(
        "4 beginbfchar\n<0E3C> <4E2D>\n<0003> <0020>\n<0004> <D83DDE00>\n<0005> <00660066>\n\
         endbfchar\n\
         1 beginbfrange\n<0010> <0012> <0041>\nendbfrange\n\
         1 beginbfrange\n<0020> <0022> [<4E2D> <D83DDE00> <0066>]\nendbfrange",
    );
    let content = b"BT /F1 12 Tf <0E3C000300040005001000110012002000210022> Tj ET";
    assert_eq!(
        font_text(IDENTITY, &[table], content),
        "中 😀ffABC中😀f",
        "bfchar, scalar bfrange, array bfrange and surrogate pairs must decode"
    );
}

#[test]
fn simple_truetype_font_maps_one_byte_codes() {
    let table = one_byte_unicode("2 beginbfchar\n<01> <0051>\n<02> <0033>\nendbfchar");
    assert_eq!(font_text(SIMPLE, &[table], b"BT /F1 12 Tf <0102> Tj ET"), "Q3");
}

#[test]
fn winansi_font_decodes_without_a_to_unicode_mapping() {
    assert_eq!(
        font_text(WINANSI, &[], b"BT /F1 12 Tf <519394917FA0AD> Tj ET"),
        "Q“”‘• -"
    );
}

#[test]
fn standard_helvetica_keeps_its_built_in_encoding() {
    let font = "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>";
    assert_eq!(font_text(font, &[], b"BT /F1 12 Tf <2760> Tj ET"), "’‘");
}

#[test]
fn unsupported_fonts_and_mappings_are_refused() {
    let table = to_unicode("1 beginbfchar\n<0001> <0041>\nendbfchar");
    let one_byte = one_byte_unicode("1 beginbfchar\n<01> <0041>\nendbfchar");
    for (case, font, extras, code, needle) in [
        (
            "type0 without ToUnicode",
            "<< /Type /Font /Subtype /Type0 /BaseFont /X /Encoding /Identity-H >>",
            vec![],
            "UNSUPPORTED_DOCUMENT",
            "no ToUnicode mapping",
        ),
        (
            "type0 with a non-identity encoding",
            "<< /Type /Font /Subtype /Type0 /BaseFont /X /Encoding /UniGB-UCS2-H /ToUnicode 6 0 R >>",
            vec![table.clone()],
            "UNSUPPORTED_DOCUMENT",
            "Identity-H or Identity-V",
        ),
        (
            "identity font with a one-byte CMap",
            "<< /Type /Font /Subtype /Type0 /BaseFont /X /Encoding /Identity-H /ToUnicode 6 0 R >>",
            vec![one_byte.clone()],
            "UNSUPPORTED_DOCUMENT",
            "two-byte codes",
        ),
        (
            "simple font with a two-byte CMap",
            SIMPLE,
            vec![table.clone()],
            "UNSUPPORTED_DOCUMENT",
            "one-byte codes",
        ),
        (
            "inherited CMap mappings",
            SIMPLE,
            vec![one_byte_unicode("usecmap")],
            "UNSUPPORTED_DOCUMENT",
            "usecmap",
        ),
        (
            "malformed destination",
            SIMPLE,
            vec![one_byte_unicode("1 beginbfchar\n<01> <D83D>\nendbfchar")],
            "DOCUMENT_PARSE_ERROR",
            "surrogate",
        ),
    ] {
        let message = font_failure(font, &extras, code);
        assert!(message.contains(needle), "{case}: {message}");
    }
}

#[test]
fn cmap_budgets_reject_one_over_each_bound() {
    let declared = raw_cmap("1 begincodespacerange\n<00> <FF>\nendcodespacerange\n65537 beginbfchar\nendbfchar");
    let long_target = raw_cmap(&format!(
        "1 begincodespacerange\n<00> <FF>\nendcodespacerange\n1 beginbfchar\n<01> <{}>\nendbfchar",
        "41".repeat(1026)
    ));
    let ranges = raw_cmap(&format!(
        "65 begincodespacerange\n{}endcodespacerange",
        "<0000> <FFFF>\n".repeat(65)
    ));
    let mixed = raw_cmap("2 begincodespacerange\n<00> <FF>\n<0000> <FFFF>\nendcodespacerange");
    for (case, table, font, code, needle) in [
        ("65537 declared mappings", declared, SIMPLE, "OUTPUT_LIMIT", "65536"),
        ("513-byte destination", long_target, SIMPLE, "OUTPUT_LIMIT", "512 bytes"),
        ("65 codespace ranges", ranges, IDENTITY, "OUTPUT_LIMIT", "64"),
        (
            "mixed code widths",
            mixed,
            IDENTITY,
            "UNSUPPORTED_DOCUMENT",
            "mixes code widths",
        ),
    ] {
        let message = font_failure(font, &[table], code);
        assert!(message.contains(needle), "{case}: {message}");
    }
    let output = shown(
        IDENTITY,
        &[to_unicode("1 beginbfchar\n<0001> <0041>\nendbfchar")],
        b"BT /F1 12 Tf <0001> Tj ET",
    );
    assert_eq!(output["status"], "completed", "{output}");
    assert_eq!(text(&output), "A", "{output}");
}
