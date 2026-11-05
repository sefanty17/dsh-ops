//! Producer-export fixtures: Chrome print-to-PDF, WPS Writer and Word 2021.
//!
//! Chrome and WPS read end to end. Both embed Type0 Identity-H fonts whose
//! `ToUnicode` CMaps carry the Unicode text, and they show a few codes per `Tj`,
//! so a line arrives as many runs. These tests pin the decoded text: a regression
//! in the operator whitelist, the font layer or the container turns the text
//! empty, partial or garbled rather than silently passing.
//!
//! The Word 2021 export is a hybrid-reference document with an incremental
//! update: its final classic xref table is an empty `0 0` subsection, and the
//! trailer's `/Prev` (previous classic xref) and `/XRefStm` (xref stream) hold the
//! real object entries, one of which is a `/Type /ObjStm` container. #432 keeps
//! incremental PDFs unsupported, so the container refuses this export; the
//! rejection test pins that as a product decision and its fonts stay unread.
//!
//! The final test guards the fixture bytes themselves, so a replaced sample can
//! never silently change what the reader assertions mean.

use ops_protocol::{Failure, Request};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

const FIXTURES: &str = "tests/fixtures";
const CHROME: &str = "producer-chrome-print.pdf";
const WORD: &str = "producer-word2021.pdf";
const WPS: &str = "producer-wps.pdf";

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(FIXTURES)
        .join(name)
}

fn raw(name: &str) -> Vec<u8> {
    std::fs::read(fixture(name)).unwrap_or_else(|error| panic!("read fixture {name}: {error}"))
}

/// Run the public `read_pdf` tool over one fixture, exactly as worker dispatch does.
fn read_pdf(name: &str) -> Result<Value, Failure> {
    let path = fixture(name);
    let root = path
        .parent()
        .expect("fixtures directory")
        .to_string_lossy()
        .into_owned();
    let request: Request = serde_json::from_value(json!({
        "root": root,
        "scope": "pdf-producer-samples",
        "tool": "read_pdf",
        "args": { "path": path.to_string_lossy() },
    }))
    .expect("worker request");
    ops_documents::dispatch(&request)
}

/// The single page's text with the run separators removed.
///
/// Chrome and WPS show a few codes per `Tj`, so the reader reports many runs per
/// line; joining them is the only way to compare the produced text with the
/// document's words.
fn page_text(name: &str, output: &Value) -> String {
    let items = output["items"]
        .as_array()
        .unwrap_or_else(|| panic!("{name}: items must be an array: {output}"));
    assert_eq!(items.len(), 1, "{name}: one page expected: {output}");
    items[0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("{name}: item text must be a string: {output}"))
        .replace('\n', "")
}

/// The export completed and its text contains every `expected` fragment.
fn assert_reads(name: &str, output: &Value, expected: &[&str]) {
    assert_eq!(
        output["status"], "completed",
        "{name}: the export must read completely once its fonts decode: {output}"
    );
    assert!(
        output.get("error").is_none(),
        "{name}: a completed scan must not report a failure: {output}"
    );
    let text = page_text(name, output);
    for fragment in expected {
        assert!(
            text.contains(fragment),
            "{name}: expected {fragment:?} in the decoded text, got {text:?}"
        );
    }
}

#[test]
fn chrome_print_export_decodes_its_type0_fonts() {
    let output = read_pdf(CHROME).expect("Chrome print export must still parse its classic xref");
    // Type0 Identity-H fonts with a ToUnicode CMap, shown a few codes per `Tj`.
    assert_reads(CHROME, &output, &["中文测试", "Q3 Report", "Gross margin 37.4"]);
}

#[test]
fn wps_export_decodes_its_type0_fonts() {
    let output = read_pdf(WPS).expect("WPS export must still parse its classic xref");
    // WPS positions words instead of showing space glyphs, so the joined text has no
    // spaces and the assertions stay on fragments that survive the run join.
    assert_reads(WPS, &output, &["中文测试", "季度报告与毛利率", "37.4%"]);
}

/// The Word 2021 export is a hybrid-reference PDF with an incremental update.
///
/// `startxref` points at a classic xref table whose only subsection is the empty `0 0`; the
/// trailer's `/Prev 136746` (previous classic xref) and `/XRefStm 136356` (xref stream) hold the
/// actual entries, and object 29 is a `/Type /ObjStm` container for objects 21..=54.
///
/// #432 keeps incremental updates unsupported, so the container stops at the empty subsection.
/// This test pins the current gap as a product decision, not a bug to repair here: reading the
/// export needs the incremental support that a separate change owns.
#[test]
fn word2021_incremental_export_is_rejected() {
    let bytes = raw(WORD);
    let error = ops_documents::pdf_container::parse(&bytes)
        .expect_err("an incremental PDF must be refused");
    assert_eq!(error.code, "DOCUMENT_PARSE_ERROR", "{}", error.message);
    assert!(
        error.message.contains("subsection cannot be empty"),
        "{}",
        error.message
    );
    // The tool path reports the same container failure instead of a partial read.
    let error = read_pdf(WORD).expect_err("the reader must not return a partial Word result");
    assert_eq!(error.code, "DOCUMENT_PARSE_ERROR", "{}", error.message);
}

/// True when `haystack` contains `needle`; PDF dictionary keys may be written
/// with any whitespace, or none, before a name value.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

fn contains_type(haystack: &[u8], kind: &str) -> bool {
    contains(haystack, format!("/Type/{kind}").as_bytes())
        || contains(haystack, format!("/Type /{kind}").as_bytes())
}

#[test]
fn fixtures_keep_the_producer_container_structure() {
    let chrome = raw(CHROME);
    assert!(
        chrome.starts_with(b"%PDF-1.4"),
        "Chrome print export must stay PDF 1.4 with a classic xref; header is {:?}",
        String::from_utf8_lossy(&chrome[..16.min(chrome.len())])
    );
    assert!(
        !contains_type(&chrome, "ObjStm"),
        "Chrome print export must stay free of object streams; a replaced fixture would change \
         what the reader assertions cover"
    );

    let wps = raw(WPS);
    assert!(
        wps.starts_with(b"%PDF-1.7"),
        "WPS export must stay PDF 1.7 with a classic xref; header is {:?}",
        String::from_utf8_lossy(&wps[..16.min(wps.len())])
    );
    assert!(
        !contains_type(&wps, "ObjStm"),
        "WPS export must stay free of object streams; a replaced fixture would change what the \
         reader assertions cover"
    );

    let word = raw(WORD);
    assert!(
        word.starts_with(b"%PDF-1.7"),
        "Word 2021 export must stay PDF 1.7; header is {:?}",
        String::from_utf8_lossy(&word[..16.min(word.len())])
    );
    assert!(
        contains_type(&word, "ObjStm") || contains_type(&word, "XRef"),
        "Word 2021 export must keep its object stream or xref stream; without either it no longer \
         exercises the hybrid-reference container that the rejection test pins"
    );
    assert!(
        contains(&word, b"/Prev"),
        "Word 2021 export must keep its incremental /Prev trailer key"
    );
}
