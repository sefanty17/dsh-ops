//! Container coverage for xref streams, object streams, incremental `/Prev` chains and hybrid
//! `/XRefStm` tables, including the Word 2021 sample the container used to reject.
//!
//! Every PDF here is assembled from emitted bytes, so each xref offset is the real one. The
//! compressed streams use stored deflate blocks: integration tests cannot reach the crate's
//! `flate2` dependency, and a stored block is still a real zlib stream.
use ops_documents::pdf_container::{MAX_SECTIONS, get, parse};
use std::path::{Path, PathBuf};

const WORD: &str = "producer-word2021.pdf";
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

    /// Write an object with an explicit number, for a copy a later revision supersedes.
    fn numbered(&mut self, number: usize, body: &[u8]) {
        self.bytes
            .extend_from_slice(format!("{number} 0 obj\n").as_bytes());
        self.bytes.extend_from_slice(body);
        self.bytes.extend_from_slice(b"\nendobj\n");
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

    /// Append an empty incremental revision whose trailer only follows `/Prev`.
    fn empty_revision(&mut self, size: usize, trailer: &str) -> usize {
        let current = self.bytes.len();
        self.bytes.extend_from_slice(
            format!(
                "xref\n0 0\ntrailer\n<< /Size {size} {trailer} >>\nstartxref\n{current}\n%%EOF\n"
            )
            .as_bytes(),
        );
        current
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

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// Three base objects plus an object stream holding objects 6 and 7, a cross-reference stream
/// for object 5, and a classic table. Returns the PDF with the hybrid `/XRefStm` linked.
fn hybrid_document() -> Pdf {
    let mut pdf = Pdf::new();
    pdf.object(CATALOG);
    pdf.object(PAGES);
    pdf.object(PAGE);
    let bodies: [(u32, &[u8]); 2] = [
        (6, b"<< /Kind /Compressed >>"),
        (7, b"(object stream text)"),
    ];
    let (decoded, first) = object_stream(&bodies);
    let container = pdf.stream(&format!("/Type /ObjStm /N 2 /First {first}"), &decoded);
    assert_eq!(container, 4);
    let stream_offset = pdf.bytes.len();
    let mut rows = Vec::new();
    rows.extend_from_slice(&row(2, 4, 0));
    rows.extend_from_slice(&row(2, 4, 1));
    pdf.stream(
        "/Type /XRef /Size 8 /W [1 4 2] /Index [6 2] /Root 1 0 R",
        &rows,
    );
    pdf.classic_section(5, 8, &format!("/Root 1 0 R /XRefStm {stream_offset}"));
    pdf
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
    let pdf = hybrid_document().bytes;
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
fn empty_incremental_subsection_follows_prev_into_the_previous_revision() {
    let mut pdf = Pdf::new();
    pdf.object(CATALOG);
    pdf.object(PAGES);
    pdf.object(PAGE);
    let previous = pdf.classic_section(3, 4, "/Root 1 0 R");
    // Word 2021's last revision: an empty `0 0` subsection and no entries of its own.
    pdf.empty_revision(4, &format!("/Root 1 0 R /Prev {previous}"));
    let document = parse(&pdf.bytes).expect("an empty incremental subsection is not an error");
    assert_eq!(document.objects.len(), 3);
    assert_eq!(document.root, (1, 0));
    let catalog = document
        .object(document.root)
        .expect("root")
        .value
        .dict()
        .expect("catalog dictionary");
    assert_eq!(
        get(catalog, b"Type").expect("Type").name().expect("name"),
        b"Catalog"
    );
}

#[test]
fn incremental_revision_supersedes_an_object_and_adds_another() {
    let mut pdf = Pdf::new();
    pdf.object(CATALOG);
    pdf.object(PAGES);
    pdf.object(PAGE);
    let previous = pdf.classic_section(3, 4, "/Root 1 0 R");
    // Revision two rewrites object 3 and appends object 4; the superseded copy stays in place.
    let updated = pdf.bytes.len();
    pdf.numbered(3, b"<< /Type /Page /Parent 2 0 R /Updated true >>");
    let added = pdf.bytes.len();
    pdf.numbered(4, b"<< /Revision 2 >>");
    let current = pdf.bytes.len();
    pdf.bytes.extend_from_slice(
        format!(
            "xref\r\n3 1\r\n{updated:010} 00000 n\r\n4 1\r\n{added:010} 00000 n\r\ntrailer\r\n\
             << /Size 5 /Root 1 0 R /Prev {previous} >>\r\nstartxref\r\n{current}\r\n%%EOF\r\n"
        )
        .as_bytes(),
    );
    let document = parse(&pdf.bytes).expect("incremental revision must merge");
    assert_eq!(document.objects.len(), 4);
    let page = document
        .object((3, 0))
        .expect("rewritten page")
        .value
        .dict()
        .expect("page dictionary");
    assert!(
        page.contains_key(b"Updated".as_slice()),
        "the newer entry must override the superseded object"
    );
    let added = document.object((4, 0)).expect("added object");
    assert_eq!(
        get(
            added.value.dict().expect("added dictionary"),
            b"Revision"
        )
        .expect("Revision")
        .integer()
        .expect("bounded integer"),
        2
    );
}

#[test]
fn xref_stream_with_w_index_and_flate_entries_is_used() {
    let mut pdf = Pdf::new();
    pdf.object(CATALOG);
    pdf.object(PAGES);
    pdf.object(PAGE);
    let stream_offset = pdf.bytes.len();
    let mut decoded = Vec::new();
    decoded.extend_from_slice(&row(0, 0, 65_535));
    for offset in &pdf.offsets {
        decoded.extend_from_slice(&row(1, *offset as u32, 0));
    }
    decoded.extend_from_slice(&row(1, stream_offset as u32, 0));
    pdf.stream(
        "/Type /XRef /Size 5 /W [1 4 2] /Index [0 5] /Root 1 0 R",
        &decoded,
    );
    pdf.bytes
        .extend_from_slice(format!("startxref\n{stream_offset}\n%%EOF\n").as_bytes());
    let document = parse(&pdf.bytes).expect("a Flate xref stream must parse");
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

#[test]
fn hybrid_xref_stream_supplements_a_classic_table() {
    let pdf = hybrid_document();
    let document = parse(&pdf.bytes).expect("a hybrid-reference file must merge both tables");
    assert_eq!(document.objects.len(), 7);
    let compressed = document.object((6, 0)).expect("compressed object 6");
    assert_eq!(
        get(
            compressed.value.dict().expect("compressed dictionary"),
            b"Kind"
        )
        .expect("Kind")
        .name()
        .expect("name"),
        b"Compressed"
    );
    assert!(document.object((7, 0)).is_ok());
    let catalog = document
        .object(document.root)
        .expect("catalog")
        .value
        .dict()
        .expect("catalog dictionary");
    let pages = document
        .resolve(get(catalog, b"Pages").expect("Pages"))
        .expect("page tree reference");
    assert_eq!(
        get(pages.dict().expect("pages dictionary"), b"Type")
            .expect("Type")
            .name()
            .expect("name"),
        b"Pages"
    );
}

#[test]
fn prev_self_reference_is_a_clear_cycle_error() {
    let mut pdf = Pdf::new();
    pdf.object(CATALOG);
    pdf.object(PAGES);
    pdf.object(PAGE);
    let _ = pdf.classic_section(3, 4, "/Root 1 0 R");
    let current = pdf.bytes.len();
    pdf.bytes.extend_from_slice(
        format!(
            "xref\n0 0\ntrailer\n<< /Size 4 /Root 1 0 R /Prev {current} >>\nstartxref\n{current}\n%%EOF\n"
        )
        .as_bytes(),
    );
    let error = parse(&pdf.bytes).expect_err("a self-referencing /Prev must fail");
    assert_eq!(error.code, "DOCUMENT_PARSE_ERROR", "{}", error.message);
    assert!(
        error.message.contains("Cyclic"),
        "the cycle must name itself: {}",
        error.message
    );
}

#[test]
fn prev_chain_hop_limit_is_bounded() {
    let mut pdf = Pdf::new();
    pdf.object(CATALOG);
    pdf.object(PAGES);
    pdf.object(PAGE);
    let mut previous = pdf.classic_section(3, 4, "/Root 1 0 R");
    for _ in 0..MAX_SECTIONS {
        previous = pdf.empty_revision(4, &format!("/Root 1 0 R /Prev {previous}"));
    }
    let _ = previous;
    let error = parse(&pdf.bytes).expect_err("a chain longer than the hop limit must fail");
    assert_eq!(error.code, "OUTPUT_LIMIT", "{}", error.message);
    assert!(error.message.contains("64"), "{}", error.message);
}

#[test]
fn word2021_hybrid_incremental_sample_parses_with_expanded_object_stream() {
    let bytes = std::fs::read(fixture(WORD)).expect("read the Word 2021 fixture");
    assert!(
        bytes.starts_with(b"%PDF-1.7"),
        "fixture must stay the Word 2021 export"
    );
    let document = parse(&bytes).expect("the Word 2021 hybrid-reference incremental PDF must parse");
    // 69 xref entries below Size 69, of which object 0 is free: 68 active objects.
    assert_eq!(document.objects.len(), 68);
    assert_eq!(document.root, (1, 0));
    let catalog = document
        .object(document.root)
        .expect("catalog")
        .value
        .dict()
        .expect("catalog dictionary");
    assert_eq!(
        get(catalog, b"Type").expect("Type").name().expect("name"),
        b"Catalog"
    );
    // Object 29 is the /Type /ObjStm container; objects 21..=54 are type 2 entries inside it.
    let container = document.object((29, 0)).expect("object stream container");
    assert!(container.stream.is_some());
    assert_eq!(
        get(
            container.value.dict().expect("object stream dictionary"),
            b"Type"
        )
        .expect("Type")
        .name()
        .expect("name"),
        b"ObjStm"
    );
    // Object 29 stays the uncompressed container; every other object in 21..=54 is type 2.
    for number in (21..=54u32).filter(|number| *number != 29) {
        let object = document
            .object((number, 0))
            .unwrap_or_else(|error| panic!("compressed object {number}: {}", error.message));
        assert!(
            object.stream.is_none(),
            "compressed object {number} cannot own a stream"
        );
    }
}
