//! An object stream's `/N` and `/First` may be indirect references, including references to an
//! object the same stream holds. Expanding such a stream re-enters `expand` for a stream that is
//! not yet in the expanded table; without a bound that recursion ends in a worker stack overflow
//! (`0xC00000FD`) instead of a `Failure`. These fixtures pin the structured refusal for a
//! self-reference, for two streams that reference each other, and for a chain longer than the
//! expansion depth bound.
//!
//! These PDFs are assembled from emitted bytes, so each cross-reference offset is the real one.
use ops_documents::pdf_container::parse;
use std::collections::BTreeMap;

const CATALOG: &[u8] = b"<< /Type /Catalog /Pages 2 0 R >>";
const PAGES: &[u8] = b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>";
const PAGE: &[u8] = b"<< /Type /Page /Parent 2 0 R >>";

/// One `/W [1 4 2]` cross-reference stream row.
fn row(kind: u8, second: u32, third: u16) -> [u8; 7] {
    let mut bytes = [0u8; 7];
    bytes[0] = kind;
    bytes[1..5].copy_from_slice(&second.to_be_bytes());
    bytes[5..].copy_from_slice(&third.to_be_bytes());
    bytes
}

/// A PDF under construction that records the offset of each numbered object it writes.
struct Pdf {
    bytes: Vec<u8>,
    offsets: BTreeMap<usize, usize>,
}

impl Pdf {
    fn new() -> Self {
        Self {
            bytes: b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\n".to_vec(),
            offsets: BTreeMap::new(),
        }
    }

    /// Write the object with the given number and record its offset.
    fn object(&mut self, number: usize, body: &[u8]) {
        assert!(
            self.offsets.insert(number, self.bytes.len()).is_none(),
            "object {number} was written twice"
        );
        self.bytes
            .extend_from_slice(format!("{number} 0 obj\n").as_bytes());
        self.bytes.extend_from_slice(body);
        self.bytes.extend_from_slice(b"\nendobj\n");
    }
}

/// An unfiltered stream object body with a direct `/Length`.
fn stream_object(dict: &str, data: &[u8]) -> Vec<u8> {
    let mut body = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
    body.extend_from_slice(data);
    body.extend_from_slice(b"\nendstream");
    body
}

/// Decoded object-stream bytes and the `/First` offset for the given inner objects.
fn object_stream(objects: &[(usize, &[u8])]) -> (Vec<u8>, usize) {
    let mut header = String::new();
    let mut offset = 0;
    for (number, body) in objects {
        header.push_str(&format!("{number} {offset} "));
        offset += body.len() + 1;
    }
    let first = header.len();
    let mut decoded = header.into_bytes();
    for (_, body) in objects {
        decoded.extend_from_slice(body);
        decoded.push(b' ');
    }
    (decoded, first)
}

/// Close a document whose only cross-reference section is the stream object `xref`, covering
/// object numbers `0..size`. `compressed` names each object held inside an object stream as
/// `(number, stream, index)`; every other number was written at its own offset.
fn finish(mut pdf: Pdf, size: usize, xref: usize, compressed: &[(usize, u32, u16)]) -> Vec<u8> {
    let xref_offset = pdf.bytes.len();
    let mut rows = vec![row(0, 0, u16::MAX)];
    for number in 1..size {
        let entry = match compressed.iter().find(|(id, _, _)| *id == number) {
            Some((_, stream, index)) => row(2, *stream, *index),
            None if number == xref => row(1, xref_offset as u32, 0),
            None => row(1, pdf.offsets[&number] as u32, 0),
        };
        rows.push(entry);
    }
    let decoded: Vec<u8> = rows.iter().flatten().copied().collect();
    pdf.object(
        xref,
        &stream_object(
            &format!("/Type /XRef /Size {size} /W [1 4 2] /Index [0 {size}] /Root 1 0 R"),
            &decoded,
        ),
    );
    assert_eq!(
        pdf.offsets.len() + compressed.len(),
        size - 1,
        "every object number below Size must be accounted for"
    );
    pdf.bytes
        .extend_from_slice(format!("startxref\n{xref_offset}\n%%EOF\n").as_bytes());
    pdf.bytes
}

#[test]
fn object_stream_whose_n_names_its_own_compressed_object_is_refused() {
    let mut pdf = Pdf::new();
    pdf.object(1, CATALOG);
    pdf.object(2, PAGES);
    pdf.object(3, PAGE);
    // Object 5 is the first object inside the object stream 4, and stream 4 names it as `/N`.
    let (decoded, first) = object_stream(&[(5, b"<< /X 1 >>")]);
    pdf.object(
        4,
        &stream_object(&format!("/Type /ObjStm /N 5 0 R /First {first}"), &decoded),
    );
    let bytes = finish(pdf, 7, 6, &[(5, 4, 0)]);

    let error = parse(&bytes).expect_err("a stream that names its own compressed object must fail");
    assert_eq!(error.code, "DOCUMENT_PARSE_ERROR", "{}", error.message);
    assert!(
        error.message.contains("Cyclic PDF object stream"),
        "{}",
        error.message
    );
}

#[test]
fn indirect_n_that_names_an_ordinary_object_still_expands() {
    // The guard must only stop cycles and runaway nesting: a legal indirect `/N` that resolves to
    // a plain integer object outside any object stream still expands the stream.
    let mut pdf = Pdf::new();
    pdf.object(1, CATALOG);
    pdf.object(2, PAGES);
    pdf.object(3, PAGE);
    let (decoded, first) = object_stream(&[(5, b"<< /X 1 >>")]);
    pdf.object(
        4,
        &stream_object(&format!("/Type /ObjStm /N 7 0 R /First {first}"), &decoded),
    );
    pdf.object(7, b"1");
    let bytes = finish(pdf, 8, 6, &[(5, 4, 0)]);

    let document = parse(&bytes).expect("an indirect /N to a plain integer must expand");
    assert!(document.object((5, 0)).is_ok(), "object 5 must be expanded");
}

#[test]
fn two_object_streams_that_expand_each_other_are_refused() {
    let mut pdf = Pdf::new();
    pdf.object(1, CATALOG);
    pdf.object(2, PAGES);
    pdf.object(3, PAGE);
    // Stream 4 holds object 8 and names object 7, which lives in stream 5, as `/N`.
    let (decoded, first) = object_stream(&[(8, b"<< /A 1 >>")]);
    pdf.object(
        4,
        &stream_object(&format!("/Type /ObjStm /N 7 0 R /First {first}"), &decoded),
    );
    // Stream 5 holds object 7 and names object 8, which lives in stream 4, as `/First`.
    let (decoded, _) = object_stream(&[(7, b"<< /B 1 >>")]);
    pdf.object(
        5,
        &stream_object("/Type /ObjStm /N 1 /First 8 0 R", &decoded),
    );
    let bytes = finish(pdf, 9, 6, &[(7, 5, 0), (8, 4, 0)]);

    let error = parse(&bytes).expect_err("two streams that expand each other must fail");
    assert_eq!(error.code, "DOCUMENT_PARSE_ERROR", "{}", error.message);
    assert!(
        error.message.contains("Cyclic PDF object stream"),
        "{}",
        error.message
    );
}

#[test]
fn object_stream_chain_past_the_expansion_depth_bound_is_refused() {
    // Nine object streams, each naming through `/N` an object held by the next one. The chain is
    // finite, so only the explicit depth bound can stop it.
    const STREAMS: usize = 9;
    let mut pdf = Pdf::new();
    pdf.object(1, CATALOG);
    pdf.object(2, PAGES);
    pdf.object(3, PAGE);
    let mut compressed = Vec::new();
    for index in 0..STREAMS {
        let stream = 4 + index;
        let held = 14 + index;
        let count = if index + 1 == STREAMS {
            "1".to_string()
        } else {
            format!("{} 0 R", held + 1)
        };
        let (decoded, first) = object_stream(&[(held, b"1")]);
        pdf.object(
            stream,
            &stream_object(&format!("/Type /ObjStm /N {count} /First {first}"), &decoded),
        );
        compressed.push((held, stream as u32, 0u16));
    }
    let bytes = finish(pdf, 23, 13, &compressed);

    let error = parse(&bytes).expect_err("a chain past the expansion depth bound must fail");
    assert_eq!(error.code, "OUTPUT_LIMIT", "{}", error.message);
    assert!(
        error.message.contains("object stream expansion"),
        "{}",
        error.message
    );
}
