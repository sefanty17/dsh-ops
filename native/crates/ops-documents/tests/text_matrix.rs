//! Content operators, form XObjects, patterns and text concatenation.
mod common;

use common::{CATALOG, HELVETICA, PAGES, classic, read, stream, text};
use serde_json::json;

const FONTS: &str = "<< /Font << /F1 4 0 R >> >>";
const FONTS_AND_XOBJECTS: &str = "<< /Font << /F1 4 0 R >> /XObject << /Im1 6 0 R >> >>";

fn page(resources: &str, content: &[u8], objects: &[Vec<u8>]) -> Vec<u8> {
    let mut all = vec![
        CATALOG.to_vec(),
        PAGES.to_vec(),
        format!("<< /Type /Page /Parent 2 0 R /Resources {resources} /Contents 5 0 R >>")
            .into_bytes(),
        HELVETICA.to_vec(),
        stream("", content),
    ];
    all.extend(objects.iter().cloned());
    classic(&all)
}

fn form(entries: &str, content: &[u8]) -> Vec<u8> {
    stream(
        &format!(" /Type /XObject /Subtype /Form /BBox [0 0 1 1]{entries}"),
        content,
    )
}

fn image() -> Vec<u8> {
    stream(
        " /Type /XObject /Subtype /Image /Width 1 /Height 1 /BitsPerComponent 8 /ColorSpace /DeviceGray",
        b"x",
    )
}

fn failed(bytes: &[u8], code: &str) -> String {
    let output = read(bytes, json!({})).expect("read_pdf returns an observation");
    assert_eq!(output["status"], "partial", "{output}");
    assert_eq!(output["error"]["code"], code, "{output}");
    output["error"]["message"]
        .as_str()
        .expect("failure message")
        .to_owned()
}

fn completed(bytes: &[u8]) -> serde_json::Value {
    let output = read(bytes, json!({})).expect("readable content");
    assert_eq!(output["status"], "completed", "{output}");
    assert!(output.get("error").is_none(), "{output}");
    output
}

#[test]
fn text_runs_concatenate_in_content_order_without_separators() {
    let output = completed(&page(
        FONTS,
        b"BT /F1 12 Tf (a) Tj (b) Tj (c) Tj ET BT /F1 12 Tf (d) Tj ET",
        &[],
    ));
    assert_eq!(text(&output), "abcd", "{output}");
    assert_eq!(
        output["items"][0]["runs"],
        json!(["a", "b", "c", "d"]),
        "{output}"
    );
}

#[test]
fn paint_operators_between_text_are_skipped() {
    for (case, resources, content, objects) in [
        (
            "an image XObject",
            FONTS_AND_XOBJECTS,
            b"BT /F1 12 Tf (before) Tj ET /Im1 Do BT /F1 12 Tf (after) Tj ET".as_slice(),
            vec![image()],
        ),
        (
            "a shading",
            FONTS,
            b"BT /F1 12 Tf (before) Tj ET /Sh1 sh BT /F1 12 Tf (after) Tj ET",
            Vec::new(),
        ),
        (
            "an inline image",
            FONTS,
            b"BT /F1 12 Tf (before) Tj ET BI /W 4 /H 1 /CS /G /BPC 8 /DP << /K 1 >> /Name (ID) ID \
              \x01EI\x02 EI BT /F1 12 Tf (after) Tj ET",
            Vec::new(),
        ),
    ] {
        assert_eq!(
            text(&completed(&page(resources, content, &objects))),
            "beforeafter",
            "{case}"
        );
    }
}

#[test]
fn form_text_is_appended_where_its_do_appears() {
    let resources = "<< /Font << /F1 4 0 R >> /XObject << /Fm1 6 0 R >> >>";
    let output = completed(&page(
        resources,
        b"BT /F1 12 Tf (before) Tj ET /Fm1 Do BT /F1 12 Tf (after) Tj ET",
        &[form("", b"BT /F1 12 Tf (inside) Tj ET")],
    ));
    assert_eq!(text(&output), "beforeinsideafter", "{output}");

    // The page names only /F1; each form resolves its own resource dictionary.
    let resources = "<< /Font << /F1 4 0 R >> /XObject << /Outer 6 0 R >> >>";
    let objects = [
        form(
            " /Resources << /Font << /F2 4 0 R >> /XObject << /Inner 7 0 R >> >>",
            b"BT /F2 12 Tf (outer) Tj ET /Inner Do",
        ),
        form("", b"BT /F2 12 Tf (inner) Tj ET"),
    ];
    let output = completed(&page(
        resources,
        b"BT /F1 12 Tf (page) Tj ET /Outer Do",
        &objects,
    ));
    assert_eq!(text(&output), "pageouterinner", "{output}");
}

#[test]
fn form_cycles_are_parse_errors() {
    let self_reference = [form(
        " /Resources << /XObject << /Fm1 6 0 R >> >>",
        b"/Fm1 Do",
    )];
    let message = failed(
        &page(
            "<< /Font << /F1 4 0 R >> /XObject << /Fm1 6 0 R >> >>",
            b"BT /F1 12 Tf (A) Tj ET /Fm1 Do",
            &self_reference,
        ),
        "DOCUMENT_PARSE_ERROR",
    );
    assert!(message.contains("Cyclic PDF form XObject"), "{message}");

    let mutual = [
        form(" /Resources << /XObject << /B 7 0 R >> >>", b"/B Do"),
        form(" /Resources << /XObject << /A 6 0 R >> >>", b"/A Do"),
    ];
    let message = failed(
        &page(
            "<< /Font << /F1 4 0 R >> /XObject << /A 6 0 R /B 7 0 R >> >>",
            b"BT /F1 12 Tf (A) Tj ET /A Do",
            &mutual,
        ),
        "DOCUMENT_PARSE_ERROR",
    );
    assert!(message.contains("Cyclic PDF form XObject"), "{message}");
}

fn nested_forms(depth: usize, text: &str) -> (String, Vec<Vec<u8>>) {
    let mut objects = Vec::new();
    for index in 0..depth {
        let id = 6 + index;
        let body = if index + 1 == depth {
            form(
                " /Resources << /Font << /F1 4 0 R >> >>",
                format!("BT /F1 12 Tf ({text}) Tj ET").as_bytes(),
            )
        } else {
            form(
                &format!(" /Resources << /XObject << /N {} 0 R >> >>", id + 1),
                b"/N Do",
            )
        };
        objects.push(body);
    }
    (
        "<< /Font << /F1 4 0 R >> /XObject << /N 6 0 R >> >>".to_string(),
        objects,
    )
}

#[test]
fn form_nesting_is_bounded_at_eight_levels() {
    let (resources, objects) = nested_forms(8, "deep");
    let output = completed(&page(
        &resources,
        b"BT /F1 12 Tf (before) Tj ET /N Do",
        &objects,
    ));
    assert_eq!(text(&output), "beforedeep", "{output}");

    let (resources, objects) = nested_forms(9, "deep");
    let message = failed(
        &page(&resources, b"BT /F1 12 Tf (before) Tj ET /N Do", &objects),
        "OUTPUT_LIMIT",
    );
    assert!(message.contains("8"), "{message}");
}

#[test]
fn form_decoded_bytes_are_bounded_per_page() {
    let mut first = vec![b' '; 600 * 1024];
    first.extend_from_slice(b"BT /F1 12 Tf (a) Tj ET");
    let mut second = vec![b' '; 600 * 1024];
    second.extend_from_slice(b"BT /F1 12 Tf (b) Tj ET");
    let message = failed(
        &page(
            "<< /Font << /F1 4 0 R >> /XObject << /A 6 0 R /B 7 0 R >> >>",
            b"/A Do /B Do",
            &[form("", &first), form("", &second)],
        ),
        "OUTPUT_LIMIT",
    );
    assert!(message.contains("1 MiB per page"), "{message}");
}

#[test]
fn pattern_colours_accept_components_and_a_name() {
    for (case, content, expected) in [
        (
            "a pattern name through scn",
            b"BT /F1 12 Tf (before) Tj ET /P31 scn BT /F1 12 Tf (after) Tj ET".as_slice(),
            "beforeafter",
        ),
        (
            "a pattern name through sc",
            b"BT /P1 sc /F1 12 Tf (A) Tj ET",
            "A",
        ),
        (
            "four components and a pattern name",
            b"BT /F1 12 Tf (A) Tj ET 1 2 3 4 /P1 scn",
            "A",
        ),
        (
            "a device colour space before the pattern",
            b"BT /F1 12 Tf (A) Tj ET /DeviceRGB cs 0.2 0.4 /P1 scn",
            "A",
        ),
        (
            "a name through sc after text",
            b"BT /F1 12 Tf (A) Tj ET /P1 sc",
            "A",
        ),
    ] {
        assert_eq!(
            text(&completed(&page(FONTS, content, &[]))),
            expected,
            "{case}"
        );
    }
    for (case, content) in [
        ("a string component", b"(P1) scn".as_slice()),
        ("two names", b"/P1 /P2 scn"),
        ("five numbers", b"1 2 3 4 5 scn"),
        ("no operands", b"sc"),
        ("no operands through scn", b"scn"),
        ("a name before the components", b"/P1 1 2 3 4 scn"),
    ] {
        assert_eq!(
            failed(&page(FONTS, content, &[]), "DOCUMENT_PARSE_ERROR"),
            "Invalid PDF colour components: 1-4 numbers or one pattern name",
            "{case}"
        );
    }
    let output = completed(&page(FONTS, b"BT /F1 12 Tf (A) Tj ET", &[]));
    let excluded = output["excludedContent"]
        .as_array()
        .expect("excludedContent array");
    assert!(
        excluded
            .iter()
            .any(|entry| entry.as_str() == Some("text painted by tiling patterns")),
        "{output}"
    );
}

#[test]
fn xobject_and_operand_validation_is_explicit() {
    let message = failed(
        &page(
            FONTS_AND_XOBJECTS,
            b"BT /F1 12 Tf (A) Tj ET /NoSuchImage Do",
            &[image()],
        ),
        "DOCUMENT_PARSE_ERROR",
    );
    assert!(message.contains("NoSuchImage"), "{message}");

    let resources = "<< /Font << /F1 4 0 R >> /XObject << /Other 6 0 R /Bare 7 0 R >> >>";
    let objects = [
        stream(" /Type /XObject /Subtype /PS", b""),
        b"<< /Type /XObject >>".to_vec(),
    ];
    let message = failed(
        &page(resources, b"BT /F1 12 Tf (A) Tj ET /Other Do", &objects),
        "UNSUPPORTED_DOCUMENT",
    );
    assert!(message.contains("PS"), "{message}");
    let message = failed(
        &page(resources, b"BT /F1 12 Tf (A) Tj ET /Bare Do", &objects),
        "DOCUMENT_PARSE_ERROR",
    );
    assert!(message.contains("Subtype"), "{message}");

    for content in [
        b"BT /F1 12 Tf (A) Tj ET /A /B Do".as_slice(),
        b"BT /F1 12 Tf (A) Tj ET 1 sh",
        b"BT /F1 12 Tf (A) Tj ET BI /W 1 /H 1 ID x",
    ] {
        failed(&page(FONTS, content, &[]), "DOCUMENT_PARSE_ERROR");
    }
    let message = failed(
        &page(
            FONTS_AND_XOBJECTS,
            b"BT /F1 12 Tf (A) Tj /Im1 Do ET",
            &[image()],
        ),
        "DOCUMENT_PARSE_ERROR",
    );
    assert!(
        message.contains("Non-text PDF operator inside a text object"),
        "{message}"
    );
    let message = failed(
        &page(FONTS, b"BT /F1 12 Tf (A) Tj ET QX", &[]),
        "UNSUPPORTED_DOCUMENT",
    );
    assert!(
        message.contains("QX") && message.contains("unknown operators are not traversed"),
        "{message}"
    );
}

#[test]
fn paint_free_operators_keep_text_wherever_they_appear() {
    let resources = "<< /Font << /F1 4 0 R /FT8 4 0 R >> >>";
    for (case, content, expected) in [
        (
            "WPS colour, font and text matrix",
            b"q BT 0.212 0.376 0.569 rg /FT8 281 Tf /GS13 gs 0.05 0 0 -0.05 90 109.37 Tm <59> Tj ET Q".as_slice(),
            "Y",
        ),
        (
            "marked content around the text object",
            b"q /G3 gs /NonStruct << /MCID 0 >> BDC BT /F1 12 Tf (Hi) Tj ET EMC Q",
            "Hi",
        ),
        (
            "marked content inside the text object",
            b"BT /F1 12 Tf /Span << /MCID 0 >> BDC (A) Tj EMC (B) Tj ET",
            "AB",
        ),
        (
            "colour spaces and marked-content points",
            b"/DeviceRGB cs /DeviceGray CS /P MP /Span << /MCID 0 >> DP BT /F1 12 Tf (B) Tj ET",
            "B",
        ),
    ] {
        let output = completed(&page(resources, content, &[]));
        assert_eq!(text(&output), expected, "{case}");
    }
    for content in [
        b"BT /F1 12 Tf (A) Tj 0 0 m 1 1 l S ET".as_slice(),
        b"BT /F1 12 Tf (A) Tj BI /W 1 /H 1 ID x EI ET",
        b"BT /F1 12 Tf (A) Tj q ET",
    ] {
        failed(&page(FONTS, content, &[]), "DOCUMENT_PARSE_ERROR");
    }
}
