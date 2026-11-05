//! Container coverage for single-section cross-reference streams and object streams, the two
//! PDF 1.5+ structures that carry a cross-reference table or object bodies in a stream.
//!
//! Every PDF here is assembled from emitted bytes, so each xref offset is the real one. The
//! compressed streams use stored deflate blocks: integration tests cannot reach the crate's
//! `flate2` dependency, and a stored block is still a real zlib stream.
//!
//! Incremental updates are outside this container (#432): an empty classic subsection and a
//! trailer `/Prev` or `/XRefStm` are asserted to be refused here, and the Word 2021 export that
//! relies on them is pinned by `pdf_producer_samples`.
use ops_documents::pdf_container::{get, parse};

const CATALOG: &[u8] = b"<< /Type /Catalog /Pages 2 0 R >>";
const PAGES: &[u8] = b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>";
const PAGE: &[u8] = b"<< /Type /Page /Parent 2 0 R >>";

/// Adler-32 of a zlib trailer.
fn adler32(data: &[u8]) -> u32 {
    let (mut low, mut high) = (1u32, 0u32);
    for byte in data {
        low = (low + u32::from(*byte)) % 65521;
        high = (high + low) % 65521;
    }
    (high << 16) | low
}

/// A zlib stream whose deflate blocks store their input; no compression library is required.
fn zlib(data: &[u8]) -> Vec<u8> {
    let mut output = vec![0x78, 0x01];
    let chunks: Vec<&[u8]> = if data.is_empty() {
        vec![&[]]
    } else {
        data.chunks(65_535).collect()
    };
    for (index, chunk) in chunks.iter().enumerate() {
        let last = index + 1 == chunks.len();
        output.push(u8::from(last));
        let length = chunk.len() as u16;
        output.extend_from_slice(&length.to_le_bytes());
        output.extend_from_slice(&(!length).to_le_bytes());
        output.extend_from_slice(chunk);
    }
    output.extend_from_slice(&adler32(data).to_be_bytes());
    output
}

/// One `/W [1 4 2]` xref-stream row.
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
    offsets: Vec<usize>,
}

impl Pdf {
    fn new() -> Self {
        Self {
            bytes: b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\n".to_vec(),
            offsets: Vec::new(),
        }
    }

    /// Write the next sequential object and record its offset.
    fn object(&mut self, body: &[u8]) -> usize {
        let number = self.offsets.len() + 1;
        self.offsets.push(self.bytes.len());
        self.bytes
            .extend_from_slice(format!("{number} 0 obj\n").as_bytes());
        self.bytes.extend_from_slice(body);
        self.bytes.extend_from_slice(b"\nendobj\n");
        number
    }

    /// Write a Flate-encoded stream object, returning its object number.
    fn stream(&mut self, dict: &str, data: &[u8]) -> usize {
        let compressed = zlib(data);
        let mut body = format!(
            "<< {dict} /Filter /FlateDecode /Length {} >>\nstream\n",
            compressed.len()
        )
        .into_bytes();
        body.extend_from_slice(&compressed);
        body.extend_from_slice(b"\nendstream");
        self.object(&body)
    }

    /// Append a classic xref table covering objects `1..=count`, then its trailer.
    fn classic_section(&mut self, count: usize, size: usize, trailer: &str) -> usize {
        let xref = self.bytes.len();
        self.bytes.extend_from_slice(
            format!("xref\n0 {}\n0000000000 65535 f \n", count + 1).as_bytes(),
        );
        for offset in &self.offsets[..count] {
            self.bytes
                .extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        self.bytes.extend_from_slice(
            format!("trailer\n<< /Size {size} {trailer} >>\nstartxref\n{xref}\n%%EOF\n").as_bytes(),
        );
        xref
    }
}

/// Decoded object-stream bytes and the `/First` offset for the given inner objects.
fn object_stream(objects: &[(u32, &[u8])]) -> (Vec<u8>, usize) {
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

/// A one-page document whose only cross-reference section is a stream of `rows`.
fn xref_stream_document(dict: &str, rows: &[[u8; 7]]) -> Vec<u8> {
    let mut pdf = Pdf::new();
    pdf.object(CATALOG);
    pdf.object(PAGES);
    pdf.object(PAGE);
    let stream_offset = pdf.bytes.len();
    let decoded: Vec<u8> = rows.iter().flatten().copied().collect();
    pdf.stream(dict, &decoded);
    pdf.bytes
        .extend_from_slice(format!("startxref\n{stream_offset}\n%%EOF\n").as_bytes());
    pdf.bytes
}

/// A one-page document whose only cross-reference section is a Flate xref stream covering
/// objects 0..=4, including the stream object itself. `extra` appends trailer keys.
fn flate_xref_stream_document(extra: &str) -> Vec<u8> {
    let mut pdf = Pdf::new();
    pdf.object(CATALOG);
    pdf.object(PAGES);
    pdf.object(PAGE);
    let stream_offset = pdf.bytes.len();
    let mut rows = vec![row(0, 0, 65_535)];
    for offset in &pdf.offsets {
        rows.push(row(1, *offset as u32, 0));
    }
    rows.push(row(1, stream_offset as u32, 0));
    let decoded: Vec<u8> = rows.iter().flatten().copied().collect();
    pdf.stream(
        &format!("/Type /XRef /Size 5 /W [1 4 2] /Index [0 5] /Root 1 0 R {extra}"),
        &decoded,
    );
    pdf.bytes
        .extend_from_slice(format!("startxref\n{stream_offset}\n%%EOF\n").as_bytes());
    pdf.bytes
}

#[test]
fn xref_stream_entry_type_three_is_unsupported() {
    let rows = [
        row(0, 0, 65_535),
        row(1, 1, 0),
        row(1, 2, 0),
        row(3, 0, 0),
    ];
    let pdf = xref_stream_document(
        "/Type /XRef /Size 4 /W [1 4 2] /Index [0 4] /Root 1 0 R",
        &rows,
    );
    let error = parse(&pdf).expect_err("PDF 1.7 has no cross-reference entry type 3");
    assert_eq!(error.code, "UNSUPPORTED_DOCUMENT", "{}", error.message);
    assert!(error.message.contains("0, 1 and 2"), "{}", error.message);
}

#[test]
fn xref_stream_bytes_must_match_its_size_index_and_w() {
    let rows = [row(0, 0, 65_535), row(1, 1, 0)];
    let pdf = xref_stream_document(
        "/Type /XRef /Size 4 /W [1 4 2] /Index [0 4] /Root 1 0 R",
        &rows,
    );
    let error = parse(&pdf).expect_err("a short cross-reference stream must fail");
    assert_eq!(error.code, "DOCUMENT_PARSE_ERROR", "{}", error.message);
    assert!(error.message.contains("Index"), "{}", error.message);
}

#[test]
fn truncated_and_mutated_xref_stream_documents_never_panic() {
    let pdf = flate_xref_stream_document("");
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
}

#[test]
fn classic_xref_with_an_empty_subsection_is_rejected() {
    let mut pdf = Pdf::new();
    pdf.object(CATALOG);
    pdf.object(PAGES);
    pdf.object(PAGE);
    let xref = pdf.bytes.len();
    // An incremental update starts with an empty subsection; this container declares one section
    // only, so the marker itself is a parse error.
    pdf.bytes.extend_from_slice(
        format!("xref\n0 0\ntrailer\n<< /Size 4 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n")
            .as_bytes(),
    );
    let error = parse(&pdf.bytes).expect_err("an empty xref subsection must be refused");
    assert_eq!(error.code, "DOCUMENT_PARSE_ERROR", "{}", error.message);
    assert!(
        error.message.contains("subsection cannot be empty"),
        "{}",
        error.message
    );
}

#[test]
fn trailer_prev_and_xref_stm_are_rejected_as_incremental_updates() {
    for trailer in [
        "/Root 1 0 R /Prev 9",
        "/Root 1 0 R /Pr#65v 9",
        "/Root 1 0 R /XRefStm 9",
    ] {
        let mut pdf = Pdf::new();
        pdf.object(CATALOG);
        pdf.object(PAGES);
        pdf.object(PAGE);
        pdf.classic_section(3, 4, trailer);
        let error = parse(&pdf.bytes).expect_err("an incremental trailer must be refused");
        assert_eq!(error.code, "UNSUPPORTED_DOCUMENT", "{}", error.message);
        assert!(
            error.message.contains("Incremental PDF updates"),
            "{}",
            error.message
        );
    }
}

#[test]
fn xref_stream_trailer_prev_is_rejected_as_an_incremental_update() {
    let pdf = flate_xref_stream_document("/Prev 9");
    let error = parse(&pdf).expect_err("an xref stream /Prev must be refused");
    assert_eq!(error.code, "UNSUPPORTED_DOCUMENT", "{}", error.message);
    assert!(
        error.message.contains("Incremental PDF updates"),
        "{}",
        error.message
    );
}

#[test]
fn xref_stream_with_w_index_and_flate_entries_is_used() {
    let pdf = flate_xref_stream_document("");
    let document = parse(&pdf).expect("a Flate xref stream must parse");
    assert_eq!(document.objects.len(), 4);
    assert_eq!(document.root, (1, 0));
    let xref = document.object((4, 0)).expect("xref stream object");
    assert!(xref.stream.is_some());
    assert_eq!(
        get(xref.value.dict().expect("xref dictionary"), b"Type")
            .expect("Type")
            .name()
            .expect("name"),
        b"XRef"
    );
}

#[test]
fn object_stream_entries_are_expanded_into_the_object_table() {
    let mut pdf = Pdf::new();
    pdf.object(CATALOG);
    pdf.object(PAGES);
    pdf.object(PAGE);
    let bodies: [(u32, &[u8]); 2] = [(6, b"<< /Marker 7 >>"), (7, b"(from object stream)")];
    let (decoded, first) = object_stream(&bodies);
    let container = pdf.stream(&format!("/Type /ObjStm /N 2 /First {first}"), &decoded);
    assert_eq!(container, 4);
    let stream_offset = pdf.bytes.len();
    let mut rows = Vec::new();
    rows.extend_from_slice(&row(0, 0, 65_535));
    for offset in &pdf.offsets {
        rows.extend_from_slice(&row(1, *offset as u32, 0));
    }
    rows.extend_from_slice(&row(1, stream_offset as u32, 0));
    rows.extend_from_slice(&row(2, 4, 0));
    rows.extend_from_slice(&row(2, 4, 1));
    pdf.stream(
        "/Type /XRef /Size 8 /W [1 4 2] /Index [0 8] /Root 1 0 R",
        &rows,
    );
    pdf.bytes
        .extend_from_slice(format!("startxref\n{stream_offset}\n%%EOF\n").as_bytes());
    let document = parse(&pdf.bytes).expect("an object stream must expand");
    assert_eq!(document.objects.len(), 7);
    let compressed = document.object((6, 0)).expect("compressed object 6");
    assert!(compressed.stream.is_none(), "compressed objects own no stream");
    assert_eq!(
        get(
            compressed.value.dict().expect("compressed dictionary"),
            b"Marker"
        )
        .expect("Marker")
        .integer()
        .expect("bounded integer"),
        7
    );
    assert!(
        document
            .object((7, 0))
            .expect("compressed object 7")
            .value
            .dict()
            .is_err()
    );
}
