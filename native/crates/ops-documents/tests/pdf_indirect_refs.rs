//! Regression coverage for issue #646: indirect references used where an integer is required.
//!
//! `ops_read_pdf` rejected a legal LibreOffice export with
//! `DOCUMENT_PARSE_ERROR: Expected a bounded nonnegative PDF integer` because
//! `Value::integer()` reads an indirect reference (`Value::Ref`) as a non-integer.
//! The affected sites are a stream `/Length`, the trailer `/Size`, and a page-tree
//! `/Count`. These tests drive the public `read_pdf` path over PDFs generated here,
//! with xref offsets computed from the emitted bytes, and pin that the bytes on the
//! header's second line never decide the outcome.

use ops_protocol::{Failure, Request};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
};

const PDF_HEADER: &[u8] = b"%PDF-1.4\n";
/// LibreOffice writes this UTF-8 line second: `%äüöß`.
const LIBREOFFICE_MARKER: &[u8] = b"%\xC3\xA4\xC3\xBC\xC3\xB6\xC3\x9F\n";
/// The classic binary marker other writers emit second: `%âãÓ`.
const CLASSIC_MARKER: &[u8] = b"%\xE2\xE3\xD3\n";
const CONTENT: &[u8] = b"BT /F1 12 Tf (Hello) Tj ET";
const EXPECTED_TEXT: &str = "Hello";

/// A minimal PDF with one classic xref section.
///
/// Offsets come from the bytes actually emitted, so an edited object body can
/// never leave the xref silently pointing at the wrong place.
struct Pdf {
    header: Vec<u8>,
    objects: Vec<Vec<u8>>,
}

impl Pdf {
    /// Start a document whose first line is `%PDF-1.4` and whose second line is `marker`.
    fn new(marker: &[u8]) -> Self {
        let mut header = PDF_HEADER.to_vec();
        header.extend_from_slice(marker);
        Self {
            header,
            objects: Vec::new(),
        }
    }

    /// Append one object body and return the 1-based object number assigned to it.
    fn object(&mut self, body: impl Into<Vec<u8>>) -> u32 {
        self.objects.push(body.into());
        self.objects.len() as u32
    }

    fn bytes(&self) -> Vec<u8> {
        let mut pdf = self.header.clone();
        let mut offsets = Vec::with_capacity(self.objects.len());
        for (index, body) in self.objects.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.extend_from_slice(format!("{} 0 obj\n", index + 1).as_bytes());
            pdf.extend_from_slice(body);
            pdf.extend_from_slice(b"\nendobj\n");
        }
        let xref = pdf.len();
        let size = self.objects.len() + 1;
        pdf.extend_from_slice(format!("xref\n0 {size}\n0000000000 65535 f \n").as_bytes());
        for offset in offsets {
            pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        pdf.extend_from_slice(
            format!(
                "trailer\n<< /Size {size} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n"
            )
            .as_bytes(),
        );
        pdf
    }
}

const CATALOG: &[u8] = b"<< /Type /Catalog /Pages 2 0 R >>";
const FONT: &[u8] = b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>";

fn pages(count: &str) -> Vec<u8> {
    format!("<< /Type /Pages /Kids [3 0 R] /Count {count} >>").into_bytes()
}

fn page(font: u32, contents: u32) -> Vec<u8> {
    format!(
        "<< /Type /Page /Parent 2 0 R /Resources << /Font << /F1 {font} 0 R >> >> /Contents {contents} 0 R >>"
    )
    .into_bytes()
}

/// One content stream object. `length` is the literal text of the `/Length` value,
/// so a caller passes either a count or a reference such as `6 0 R`.
fn stream(length: &str, content: &[u8]) -> Vec<u8> {
    let mut body = format!("<< /Length {length} >>\nstream\n").into_bytes();
    body.extend_from_slice(content);
    body.extend_from_slice(b"\nendstream");
    body
}

/// The control document: one page, a direct `/Length`, a direct `/Count`.
fn control_pdf(marker: &[u8]) -> Vec<u8> {
    let mut pdf = Pdf::new(marker);
    pdf.object(CATALOG);
    pdf.object(pages("1"));
    pdf.object(page(5, 4));
    pdf.object(stream(&CONTENT.len().to_string(), CONTENT));
    pdf.object(FONT);
    pdf.bytes()
}

/// Scratch directory Cargo creates for integration tests. The system temp
/// directory is not writable in every environment.
fn scratch_dir() -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    std::fs::create_dir_all(&dir).expect("create scratch directory");
    dir
}

/// One PDF on disk; removed when the test ends, including on panic.
struct TempPdf(PathBuf);

impl TempPdf {
    fn write(bytes: &[u8]) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = scratch_dir().join(format!(
            "pdf-indirect-{}-{}.pdf",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&path, bytes).expect("write temporary PDF");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempPdf {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Run the public `read_pdf` tool over `bytes`, exactly as the worker dispatch does.
fn read_pdf(bytes: &[u8]) -> Result<Value, Failure> {
    let temp = TempPdf::write(bytes);
    let path = temp.path();
    let root = path
        .parent()
        .expect("scratch directory")
        .to_string_lossy()
        .into_owned();
    let request: Request = serde_json::from_value(json!({
        "root": root,
        "scope": "pdf-regression",
        "tool": "read_pdf",
        "args": { "path": path.to_string_lossy() },
    }))
    .expect("worker request");
    ops_documents::dispatch(&request)
}

/// The document parsed and its single page produced the expected text layers.
fn assert_reads_hello(output: &Value) {
    assert_eq!(output["status"], "completed", "reader reported {output}");
    assert_eq!(output["total"], 1, "reader reported {output}");
    assert_eq!(output["pagesObserved"], 1, "reader reported {output}");
    let page = &output["items"][0];
    assert_eq!(page["page"], 1, "reader reported {output}");
    assert_eq!(page["text"], EXPECTED_TEXT, "reader reported {output}");
}

/// A dangling reference is a missing-object failure, never a value-type failure.
fn assert_clear_reference_error(error: &Failure) {
    assert_eq!(error.code, "DOCUMENT_PARSE_ERROR", "{}", error.message);
    assert!(
        !error.message.contains("bounded nonnegative"),
        "a dangling reference must not be reported as an integer type error: {}",
        error.message
    );
    assert!(
        !error.message.trim().is_empty(),
        "the failure must state what is missing"
    );
}

#[test]
fn direct_length_parses_and_preserves_exact_stream_bytes() {
    let pdf = control_pdf(b"");
    let document = ops_documents::pdf_container::parse(&pdf).expect("direct /Length must parse");
    assert_eq!(document.objects.len(), 5);
    assert_eq!(
        document
            .object((4, 0))
            .expect("content object")
            .stream
            .expect("content stream"),
        CONTENT
    );
    assert_reads_hello(&read_pdf(&pdf).expect("direct /Length must read"));
}

#[test]
fn libreoffice_second_line_marker_does_not_change_the_outcome() {
    let pdf = control_pdf(LIBREOFFICE_MARKER);
    let marker_at = PDF_HEADER.len();
    assert_eq!(
        &pdf[marker_at..marker_at + LIBREOFFICE_MARKER.len()],
        LIBREOFFICE_MARKER,
        "fixture must really carry the LibreOffice marker"
    );
    let document =
        ops_documents::pdf_container::parse(&pdf).expect("LibreOffice-style header must parse");
    assert_eq!(
        document
            .object((4, 0))
            .expect("content object")
            .stream
            .expect("content stream"),
        CONTENT
    );
    assert_reads_hello(&read_pdf(&pdf).expect("LibreOffice-style header must read"));
}

#[test]
fn indirect_length_defined_after_the_stream_is_resolved() {
    let mut pdf = Pdf::new(b"");
    pdf.object(CATALOG); // 1
    pdf.object(pages("1")); // 2
    pdf.object(page(5, 4)); // 3
    pdf.object(stream("6 0 R", CONTENT)); // 4
    pdf.object(FONT); // 5
    // LibreOffice writes a stream's length object after the stream that uses it.
    pdf.object(CONTENT.len().to_string()); // 6
    let bytes = pdf.bytes();
    let document =
        ops_documents::pdf_container::parse(&bytes).expect("indirect /Length must parse");
    assert_eq!(
        document
            .object((4, 0))
            .expect("content object")
            .stream
            .expect("content stream"),
        CONTENT
    );
    assert_reads_hello(&read_pdf(&bytes).expect("indirect /Length must read"));
}

#[test]
fn indirect_page_count_is_resolved() {
    let mut pdf = Pdf::new(b"");
    pdf.object(CATALOG); // 1
    pdf.object(pages("6 0 R")); // 2
    pdf.object(page(5, 4)); // 3
    pdf.object(stream(&CONTENT.len().to_string(), CONTENT)); // 4
    pdf.object(FONT); // 5
    pdf.object("1"); // 6
    assert_reads_hello(&read_pdf(&pdf.bytes()).expect("indirect /Count must read"));
}

#[test]
fn dangling_length_reference_is_reported_as_a_reference_error() {
    let mut pdf = Pdf::new(b"");
    pdf.object(CATALOG); // 1
    pdf.object(pages("1")); // 2
    pdf.object(page(5, 4)); // 3
    pdf.object(stream("9 0 R", CONTENT)); // 4: object 9 is never declared
    pdf.object(FONT); // 5
    let error = read_pdf(&pdf.bytes()).expect_err("a dangling /Length must fail");
    assert_clear_reference_error(&error);
}

#[test]
fn classic_binary_marker_header_parses() {
    let pdf = control_pdf(CLASSIC_MARKER);
    let marker_at = PDF_HEADER.len();
    assert_eq!(
        &pdf[marker_at..marker_at + CLASSIC_MARKER.len()],
        CLASSIC_MARKER,
        "fixture must really carry the classic marker"
    );
    let document = ops_documents::pdf_container::parse(&pdf).expect("classic header must parse");
    assert_eq!(
        document
            .object((4, 0))
            .expect("content object")
            .stream
            .expect("content stream"),
        CONTENT
    );
    assert_reads_hello(&read_pdf(&pdf).expect("classic header must read"));
}

#[test]
fn indirect_length_defined_before_the_stream_is_resolved() {
    let mut pdf = Pdf::new(b"");
    pdf.object(CATALOG); // 1
    pdf.object(pages("1")); // 2
    pdf.object(page(6, 5)); // 3
    pdf.object(CONTENT.len().to_string()); // 4: length declared before its user
    pdf.object(stream("4 0 R", CONTENT)); // 5
    pdf.object(FONT); // 6
    let bytes = pdf.bytes();
    let document = ops_documents::pdf_container::parse(&bytes)
        .expect("backward indirect /Length must parse");
    assert_eq!(
        document
            .object((5, 0))
            .expect("content object")
            .stream
            .expect("content stream"),
        CONTENT
    );
    assert_reads_hello(&read_pdf(&bytes).expect("backward indirect /Length must read"));
}

#[test]
fn dangling_page_count_reference_is_reported_as_a_reference_error() {
    let mut pdf = Pdf::new(b"");
    pdf.object(CATALOG); // 1
    pdf.object(pages("9 0 R")); // 2: object 9 is never declared
    pdf.object(page(5, 4)); // 3
    pdf.object(stream(&CONTENT.len().to_string(), CONTENT)); // 4
    pdf.object(FONT); // 5
    let error = read_pdf(&pdf.bytes()).expect_err("a dangling /Count must fail");
    assert_clear_reference_error(&error);
}
