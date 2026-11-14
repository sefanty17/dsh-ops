//! Cross-reference tables and streams, object streams, indirect integers, offset tolerance and
//! structural limits.
mod common;

use common::{
    CATALOG, HEADER, HELVETICA, PAGE, PAGES, Pdf, classic, object_stream, read, row, stream,
    stream_with_length, text,
};
use ops_documents::pdf_container::{MAX_FILE, MAX_STREAM, parse};
use serde_json::json;

const CONTENT: &[u8] = b"BT /F1 12 Tf (Hello) Tj ET";

fn parse_error(bytes: &[u8], code: &str, needle: &str) {
    let error = parse(bytes).expect_err("the document must be refused");
    assert_eq!(error.code, code, "{}", error.message);
    assert!(error.message.contains(needle), "{}", error.message);
}

fn hello_objects() -> Vec<Vec<u8>> {
    vec![
        CATALOG.to_vec(),
        PAGES.to_vec(),
        b"<< /Type /Page /Parent 2 0 R /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>"
            .to_vec(),
        stream("", CONTENT),
        HELVETICA.to_vec(),
    ]
}

fn hello_xref_stream() -> Vec<u8> {
    let mut pdf = Pdf::new();
    for body in hello_objects() {
        pdf.object(&body);
    }
    pdf.finish_xref_stream("")
}

fn read_text(bytes: &[u8], expected: &str) {
    let output = read(bytes, json!({})).expect("read_pdf returns an observation");
    assert_eq!(output["status"], "completed", "{output}");
    assert_eq!(text(&output), expected, "{output}");
}

fn xref_stream_document(dict: &str, rows: &[[u8; 7]]) -> Vec<u8> {
    let mut pdf = Pdf::new();
    pdf.object(CATALOG);
    pdf.object(PAGES);
    pdf.object(PAGE);
    let offset = pdf.bytes.len();
    pdf.stream(dict, &rows.concat());
    pdf.bytes
        .extend_from_slice(format!("startxref\n{offset}\n%%EOF\n").as_bytes());
    pdf.bytes
}

#[test]
fn indirect_integers_resolve_in_every_direction() {
    let mut direct = Pdf::new();
    direct.object(CATALOG);
    direct.object(PAGES);
    direct.object(
        b"<< /Type /Page /Parent 2 0 R /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>",
    );
    direct.object(&stream("", CONTENT));
    direct.object(HELVETICA);

    let mut forward = Pdf::new();
    forward.object(CATALOG);
    forward.object(PAGES);
    forward.object(
        b"<< /Type /Page /Parent 2 0 R /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>",
    );
    forward.object(&stream_with_length("6 0 R", "", CONTENT));
    forward.object(HELVETICA);
    forward.object(CONTENT.len().to_string().as_bytes());

    let mut backward = Pdf::new();
    backward.object(CATALOG);
    backward.object(PAGES);
    backward.object(
        b"<< /Type /Page /Parent 2 0 R /Resources << /Font << /F1 6 0 R >> >> /Contents 5 0 R >>",
    );
    backward.object(CONTENT.len().to_string().as_bytes());
    backward.object(&stream_with_length("4 0 R", "", CONTENT));
    backward.object(HELVETICA);

    for (case, mut pdf) in [
        ("direct", direct),
        ("forward", forward),
        ("backward", backward),
    ] {
        let size = pdf.offsets.len() + 1;
        pdf.classic_section(pdf.offsets.len(), size, "/Root 1 0 R");
        let bytes = pdf.bytes;
        let document = parse(&bytes).unwrap_or_else(|error| panic!("{case}: {}", error.message));
        assert_eq!(document.objects.len(), size - 1, "{case}");
        let content = if case == "backward" { 5 } else { 4 };
        assert_eq!(
            document
                .object((content, 0))
                .expect("content object")
                .stream,
            Some(CONTENT),
            "{case}"
        );
        read_text(&bytes, "Hello");
    }

    let mut size = Pdf::new();
    for body in hello_objects() {
        size.object(&body);
    }
    size.object(b"7");
    let objects = size.offsets.len();
    let xref = size.bytes.len();
    size.bytes.extend_from_slice(
        format!(
            "xref
0 {}
0000000000 65535 f 
",
            objects + 1
        )
        .as_bytes(),
    );
    for number in 1..=objects {
        let offset = size.offsets[&number];
        size.bytes.extend_from_slice(
            format!(
                "{offset:010} 00000 n 
"
            )
            .as_bytes(),
        );
    }
    size.bytes.extend_from_slice(
        format!(
            "trailer
<< /Size 6 0 R /Root 1 0 R >>
startxref
{xref}
%%EOF
"
        )
        .as_bytes(),
    );
    read_text(&size.bytes, "Hello");
}

#[test]
fn dangling_and_cyclic_indirect_integers_are_reference_errors() {
    let mut length = Pdf::new();
    for body in hello_objects() {
        length.object(&body);
    }
    length.numbered(4, &stream_with_length("9 0 R", "", CONTENT));
    length.classic_section(5, 6, "/Root 1 0 R");

    let mut count = Pdf::new();
    count.object(CATALOG);
    count.object(b"<< /Type /Pages /Kids [3 0 R] /Count 9 0 R >>");
    count.object(b"<< /Type /Page /Parent 2 0 R >>");
    count.classic_section(3, 4, "/Root 1 0 R");

    let mut size = Pdf::new();
    for body in hello_objects() {
        size.object(&body);
    }
    let objects = size.offsets.len();
    let xref = size.bytes.len();
    size.bytes.extend_from_slice(
        format!(
            "xref
0 {}
0000000000 65535 f 
",
            objects + 1
        )
        .as_bytes(),
    );
    for number in 1..=objects {
        let offset = size.offsets[&number];
        size.bytes.extend_from_slice(
            format!(
                "{offset:010} 00000 n 
"
            )
            .as_bytes(),
        );
    }
    size.bytes.extend_from_slice(
        format!(
            "trailer
<< /Size 9 0 R /Root 1 0 R >>
startxref
{xref}
%%EOF
"
        )
        .as_bytes(),
    );

    let chain = |five: &[u8]| {
        let mut pdf = Pdf::new();
        pdf.object(CATALOG);
        pdf.object(PAGES);
        pdf.object(
            b"<< /Type /Page /Parent 2 0 R /Resources << /Font << /F1 6 0 R >> >> /Contents 4 0 R >>",
        );
        pdf.object(&stream_with_length("5 0 R", "", CONTENT));
        pdf.object(five);
        pdf.object(HELVETICA);
        pdf.classic_section(6, 7, "/Root 1 0 R");
        pdf.bytes
    };

    let mut cyclic_count = Pdf::new();
    cyclic_count.object(CATALOG);
    cyclic_count.object(b"<< /Type /Pages /Kids [3 0 R] /Count 4 0 R >>");
    cyclic_count.object(b"<< /Type /Page /Parent 2 0 R >>");
    cyclic_count.object(b"4 0 R");
    cyclic_count.classic_section(4, 5, "/Root 1 0 R");

    for (case, bytes, needle) in [
        ("dangling /Length", length.bytes, "does not name an active"),
        ("dangling /Count", count.bytes, "does not name an active"),
        ("dangling /Size", size.bytes, "does not name an active"),
        ("cyclic /Length", chain(b"5 0 R"), "Cyclic PDF reference"),
        ("cyclic /Count", cyclic_count.bytes, "Cyclic PDF reference"),
        (
            "non-integer /Length",
            chain(b"(not a number)"),
            "must reference a direct integer",
        ),
    ] {
        let error = read(&bytes, json!({})).expect_err("the reference must fail the read");
        assert_eq!(
            error.code, "DOCUMENT_PARSE_ERROR",
            "{case}: {}",
            error.message
        );
        assert!(error.message.contains(needle), "{case}: {}", error.message);
        assert!(
            !error.message.contains("bounded nonnegative"),
            "{case}: {}",
            error.message
        );
    }
}

#[test]
fn xref_stream_shapes_are_used() {
    let bytes = hello_xref_stream();
    let document = parse(&bytes).expect("a Flate xref stream must parse");
    assert_eq!(document.objects.len(), 6);
    assert_eq!(document.root, (1, 0));
    read_text(&bytes, "Hello");
}

#[test]
fn object_streams_expand_and_merge_with_hybrid_tables() {
    let mut expanded = Pdf::new();
    expanded.object(CATALOG);
    expanded.object(PAGES);
    expanded.object(PAGE);
    let (decoded, first) = object_stream(&[(6, b"<< /Marker 7 >>"), (7, b"(from object stream)")]);
    expanded.stream(&format!("/Type /ObjStm /N 2 /First {first}"), &decoded);
    let stream_offset = expanded.bytes.len();
    let mut rows = vec![row(0, 0, 65_535)];
    for number in 1..=4 {
        rows.push(row(1, expanded.offsets[&number] as u32, 0));
    }
    rows.push(row(1, stream_offset as u32, 0));
    rows.push(row(2, 4, 0));
    rows.push(row(2, 4, 1));
    expanded.stream(
        "/Type /XRef /Size 8 /W [1 4 2] /Index [0 8] /Root 1 0 R",
        &rows.concat(),
    );
    expanded
        .bytes
        .extend_from_slice(format!("startxref\n{stream_offset}\n%%EOF\n").as_bytes());
    let document = parse(&expanded.bytes).expect("an object stream must expand");
    assert_eq!(document.objects.len(), 7);
    let compressed = document.object((6, 0)).expect("compressed object 6");
    assert!(compressed.stream.is_none());
    assert_eq!(
        ops_documents::pdf_container::get(compressed.value.dict().expect("dictionary"), b"Marker")
            .expect("Marker")
            .integer()
            .expect("bounded integer"),
        7
    );
    assert!(
        document
            .object((7, 0))
            .expect("object 7")
            .value
            .dict()
            .is_err()
    );
}

#[test]
fn malformed_xref_sections_are_refused() {
    let type_three = xref_stream_document(
        "/Type /XRef /Size 4 /W [1 4 2] /Index [0 4] /Root 1 0 R",
        &[row(0, 0, 65_535), row(1, 1, 0), row(1, 2, 0), row(3, 0, 0)],
    );
    let short_payload = xref_stream_document(
        "/Type /XRef /Size 4 /W [1 4 2] /Index [0 4] /Root 1 0 R",
        &[row(0, 0, 65_535), row(1, 1, 0)],
    );
    let wide_field = xref_stream_document(
        "/Type /XRef /Size 4 /W [1 5 2] /Index [0 4] /Root 1 0 R",
        &[row(0, 0, 0)],
    );
    let odd_index = xref_stream_document(
        "/Type /XRef /Size 4 /W [1 4 2] /Index [0 1 2] /Root 1 0 R",
        &[row(0, 0, 0)],
    );
    for (case, bytes, code, needle) in [
        (
            "an entry type 3",
            type_three,
            "UNSUPPORTED_DOCUMENT",
            "0, 1 and 2",
        ),
        (
            "a short payload",
            short_payload,
            "DOCUMENT_PARSE_ERROR",
            "Index",
        ),
        (
            "a five-byte field",
            wide_field,
            "OUTPUT_LIMIT",
            "field exceeds 4 bytes",
        ),
        (
            "an odd Index",
            odd_index,
            "DOCUMENT_PARSE_ERROR",
            "offset/count pairs",
        ),
    ] {
        let error = parse(&bytes)
            .err()
            .unwrap_or_else(|| panic!("{case} must fail"));
        assert_eq!(error.code, code, "{case}: {}", error.message);
        assert!(error.message.contains(needle), "{case}: {}", error.message);
    }
}

fn one_object(body: &[u8], declared: usize) -> Vec<u8> {
    let mut pdf = HEADER.to_vec();
    pdf.extend_from_slice(body);
    pdf.extend_from_slice(
        b"
",
    );
    let xref = pdf.len();
    pdf.extend_from_slice(
        format!(
            "xref\n0 2\n0000000000 65535 f \n{declared:010} 00000 n \n\
             trailer\n<< /Size 2 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n"
        )
        .as_bytes(),
    );
    pdf
}

#[test]
fn offset_tolerance_is_bounded_and_comments_are_not_headers() {
    let object = b"1 0 obj\n<<>>\nendobj";
    let accepted = [vec![b' '; 32], object.to_vec()].concat();
    let pdf = one_object(&accepted, 15);
    assert_eq!(
        parse(&pdf)
            .expect("a 32-byte lead is tolerated")
            .objects
            .len(),
        1
    );
    let rejected = [vec![b' '; 33], object.to_vec()].concat();
    parse_error(
        &one_object(&rejected, 15),
        "DOCUMENT_PARSE_ERROR",
        "must point exactly",
    );

    for (body, header) in [
        (b"%   1 0 obj\n1 0 obj\n<<>>\nendobj\n".as_slice(), 4),
        (b"1 0 obj\n<<>>\nendobj % 1 0 obj\n".as_slice(), 22),
        (b"1 0 obj\n<<>>\nendobj\n% 2 0 obj\n".as_slice(), 22),
    ] {
        parse_error(
            &one_object(body, 15 + header),
            "DOCUMENT_PARSE_ERROR",
            "comment cannot introduce an object",
        );
    }

    // The percent stays inside the first object's literal string; a stray string in the gap is
    // unindexed data and must not be recovered.
    let mut two = b"%PDF-1.4\n1 0 obj\n<< /Note (100%) >> endobj 2 0 obj\n<<>>\nendobj\n".to_vec();
    let second = two
        .windows(7)
        .position(|window| window == b"2 0 obj")
        .expect("header");
    let xref = two.len();
    two.extend_from_slice(
        format!(
            "xref\n0 3\n0000000000 65535 f \n{0:010} 00000 n \n{second:010} 00000 n \n\
             trailer\n<< /Size 3 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            9
        )
        .as_bytes(),
    );
    assert_eq!(
        parse(&two)
            .expect("the header follows a string")
            .objects
            .len(),
        2
    );

    parse_error(
        &one_object(b"X1 0 obj\n<<>>\nendobj\n", 15),
        "DOCUMENT_PARSE_ERROR",
        "must point exactly",
    );
    parse_error(
        &one_object(b"X1 0 obj\n<<>>\nendobj\n", 16),
        "DOCUMENT_PARSE_ERROR",
        "must follow whitespace",
    );
}

#[test]
fn truncated_and_mutated_documents_never_panic() {
    let pdf = hello_xref_stream();
    let baseline = parse(&pdf).expect("the baseline document must parse");
    assert_eq!(baseline.objects.len(), 6);
    assert_eq!(&pdf[..15], b"%PDF-1.4\n%\xE2\xE3\xCF\xD3\n");
    for at in 10..14 {
        for byte in [0u8, b'%', b'<', b'9', 0xff] {
            let mut changed = pdf.clone();
            changed[at] = byte;
            let document = parse(&changed).expect("a header comment byte must not decide a parse");
            assert_eq!(document.objects.len(), baseline.objects.len());
            assert_eq!(document.root, baseline.root);
        }
    }
    for end in 0..pdf.len() {
        let _ = parse(&pdf[..end]);
    }
    for at in 0..pdf.len() {
        for byte in [0u8, b'%', b'<', b'9', 0xff] {
            let mut changed = pdf.clone();
            changed[at] = byte;
            let _ = parse(&changed);
        }
    }
    let mut corrupt = pdf.clone();
    let end = corrupt
        .windows(10)
        .rposition(|window| window == b"\nendstream")
        .expect("endstream");
    corrupt[end - 1] ^= 1;
    parse_error(&corrupt, "DOCUMENT_PARSE_ERROR", "");
}

#[test]
fn input_and_structure_limits_are_refused_at_one_over() {
    let oversized = vec![b' '; MAX_FILE + 1];
    let error = read(&oversized, json!({})).expect_err("a file over 16 MiB must fail");
    assert_eq!(error.code, "OUTPUT_LIMIT");
    assert!(
        error.next_action.contains("16 MiB"),
        "{}",
        error.next_action
    );

    let bodies: Vec<Vec<u8>> =
        vec![format!("<< /Length {} >>\nstream\nx\nendstream", MAX_STREAM + 1).into_bytes()];
    for (index, body) in bodies.iter().enumerate() {
        let error = parse(&classic(std::slice::from_ref(body)))
            .expect_err(&format!("limit fixture {index} must be refused"));
        assert_eq!(
            error.code, "OUTPUT_LIMIT",
            "fixture {index}: {}",
            error.message
        );
    }
}
