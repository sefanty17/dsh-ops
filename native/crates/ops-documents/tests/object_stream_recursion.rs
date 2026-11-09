//! Object-stream recursion: a self-reference, two streams that expand each other, and a chain
//! past the expansion depth bound are structured failures, never a worker stack overflow.
mod common;

use common::{CATALOG, PAGE, PAGES, Pdf, object_stream, row, stream};
use ops_documents::pdf_container::parse;

fn finish(pdf: Pdf, size: usize, xref: usize, compressed: &[(usize, u32, u16)]) -> Vec<u8> {
    let mut pdf = pdf;
    let xref_offset = pdf.bytes.len();
    let mut rows = vec![row(0, 0, u16::MAX)];
    for number in 1..size {
        let entry = match compressed.iter().find(|(id, _, _)| *id == number) {
            Some((_, container, index)) => row(2, *container, *index),
            None if number == xref => row(1, xref_offset as u32, 0),
            None => row(1, pdf.offsets[&number] as u32, 0),
        };
        rows.push(entry);
    }
    let dict = format!("/Type /XRef /Size {size} /W [1 4 2] /Index [0 {size}] /Root 1 0 R");
    pdf.numbered(xref, &stream(&dict, &rows.concat()));
    assert_eq!(
        pdf.offsets.len() + compressed.len(),
        size - 1,
        "every object number below Size must be accounted for"
    );
    pdf.bytes
        .extend_from_slice(format!("startxref\n{xref_offset}\n%%EOF\n").as_bytes());
    pdf.bytes
}

fn base() -> Pdf {
    let mut pdf = Pdf::new();
    pdf.object(CATALOG);
    pdf.object(PAGES);
    pdf.object(PAGE);
    pdf
}

#[test]
fn a_stream_that_names_its_own_compressed_object_is_refused() {
    let mut pdf = base();
    let (decoded, first) = object_stream(&[(5, b"<< /X 1 >>")]);
    pdf.numbered(
        4,
        &stream(&format!("/Type /ObjStm /N 5 0 R /First {first}"), &decoded),
    );
    let bytes = finish(pdf, 7, 6, &[(5, 4, 0)]);
    let error = parse(&bytes).expect_err("a stream that names its own compressed object must fail");
    assert_eq!(error.code, "DOCUMENT_PARSE_ERROR", "{}", error.message);
    assert!(error.message.contains("Cyclic PDF object stream"), "{}", error.message);
}

#[test]
fn an_indirect_n_that_names_a_plain_integer_still_expands() {
    let mut pdf = base();
    let (decoded, first) = object_stream(&[(5, b"<< /X 1 >>")]);
    pdf.numbered(
        4,
        &stream(&format!("/Type /ObjStm /N 7 0 R /First {first}"), &decoded),
    );
    pdf.numbered(7, b"1");
    let bytes = finish(pdf, 8, 6, &[(5, 4, 0)]);
    let document = parse(&bytes).expect("an indirect /N to a plain integer must expand");
    assert!(document.object((5, 0)).is_ok(), "object 5 must be expanded");
}

#[test]
fn two_streams_that_expand_each_other_are_refused() {
    let mut pdf = base();
    let (decoded, first) = object_stream(&[(8, b"<< /A 1 >>")]);
    pdf.numbered(
        4,
        &stream(&format!("/Type /ObjStm /N 7 0 R /First {first}"), &decoded),
    );
    let (decoded, _) = object_stream(&[(7, b"<< /B 1 >>")]);
    pdf.numbered(5, &stream("/Type /ObjStm /N 1 /First 8 0 R", &decoded));
    let bytes = finish(pdf, 9, 6, &[(7, 5, 0), (8, 4, 0)]);
    let error = parse(&bytes).expect_err("two streams that expand each other must fail");
    assert_eq!(error.code, "DOCUMENT_PARSE_ERROR", "{}", error.message);
    assert!(error.message.contains("Cyclic PDF object stream"), "{}", error.message);
}

#[test]
fn a_chain_past_the_expansion_depth_bound_is_refused() {
    const STREAMS: usize = 9;
    let mut pdf = base();
    let mut compressed = Vec::new();
    for index in 0..STREAMS {
        let container = 4 + index;
        let held = 14 + index;
        let count = if index + 1 == STREAMS {
            "1".to_string()
        } else {
            format!("{} 0 R", held + 1)
        };
        let (decoded, first) = object_stream(&[(held as u32, b"1")]);
        pdf.numbered(
            container,
            &stream(&format!("/Type /ObjStm /N {count} /First {first}"), &decoded),
        );
        compressed.push((held, container as u32, 0u16));
    }
    let bytes = finish(pdf, 23, 13, &compressed);
    let error = parse(&bytes).expect_err("a chain past the expansion depth bound must fail");
    assert_eq!(error.code, "OUTPUT_LIMIT", "{}", error.message);
    assert!(error.message.contains("object stream expansion"), "{}", error.message);
}
