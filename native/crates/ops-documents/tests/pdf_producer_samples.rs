//! Producer-export fixtures: Chrome print-to-PDF, Word 2021 and WPS Writer.
//!
//! Chrome and WPS place graphics-state and marked-content operators where the
//! classic operator whitelist refused them: Chrome failed with
//! `Unsupported PDF content operator gs` and WPS with `Non-text PDF operator
//! inside a text object`. Once the operator whitelist accepts those operators,
//! both exports reach the font layer instead, where the embedded Type0 fonts are
//! still unsupported: the scan reports `status: "partial"`, an error that names
//! fonts, and no page text. These two assertions are therefore red on the
//! pre-extension reader and green after the extension; they are the signal that
//! the operator gap moved down to fonts.
//!
//! The Word 2021 export is a hybrid-reference document with an incremental
//! update: its final classic xref table is an empty `0 0` subsection, and the
//! trailer's `/Prev` (previous classic xref) and `/XRefStm` (xref stream) hold the
//! real object entries. The container rejects that empty incremental subsection
//! today, so the Word pin is green now and goes red once the container accepts the
//! document; it pins the rejected empty incremental subsection, not xref-stream
//! detection.
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

fn message(output: &Value) -> &str {
    output["error"]["message"].as_str().unwrap_or("")
}

/// The export got past every content operator and stopped at the font layer.
///
/// `pre_extension` is the operator error this fixture produced before the
/// whitelist covered its graphics-state/marked-content operators; naming it keeps
/// a regression that re-rejects those operators self-explanatory.
///
/// The named font error and the empty page text are current-stage pins: they hold
/// while Type0 fonts are unsupported. When embedded fonts become readable the scan
/// may complete with text, so the empty-text assertion below must be relaxed then;
/// the status check already accepts `completed`.
fn assert_reaches_the_font_layer(name: &str, pre_extension: &str, output: &Value) {
    let status = output["status"].as_str().unwrap_or("");
    assert!(
        status == "partial" || status == "completed",
        "{name}: expected a partial or completed scan once the operator whitelist covers this \
         export, got status {status:?}: {output}"
    );
    let message = message(output);
    assert!(
        !message.contains("operator"),
        "{name}: the reader still fails on a non-text operator ({message:?}), which is exactly the \
         {pre_extension:?} failure the operator extension is meant to remove: {output}"
    );
    if status == "partial" {
        assert!(
            message.contains("font"),
            "{name}: a partial scan must stop at the font layer (embedded Type0 fonts); got \
             {message:?}: {output}"
        );
    }
    // Current-stage pin, not an endorsement: Type0 fonts are unsupported, so the
    // scan must not report page text it cannot decode. A completed scan with text
    // is the expected shape once embedded fonts are readable; relax this then.
    let items = output["items"]
        .as_array()
        .unwrap_or_else(|| panic!("{name}: items must be an array: {output}"));
    for item in items {
        let text = item["text"]
            .as_str()
            .unwrap_or_else(|| panic!("{name}: item text must be a string: {output}"));
        assert!(
            text.is_empty(),
            "{name}: no page text is decodable while Type0 fonts are unsupported; got {text:?}: {output}"
        );
    }
}

#[test]
fn chrome_print_export_reaches_the_font_layer() {
    let output = read_pdf(CHROME).expect("Chrome print export must still parse its classic xref");
    assert_reaches_the_font_layer(CHROME, "Unsupported PDF content operator gs", &output);
}

#[test]
fn wps_export_reaches_the_font_layer() {
    let output = read_pdf(WPS).expect("WPS export must still parse its classic xref");
    assert_reaches_the_font_layer(WPS, "Non-text PDF operator inside a text object", &output);
}

/// The Word 2021 export is a hybrid-reference PDF with an incremental update.
///
/// `startxref` points at a classic xref table whose only subsection is the empty
/// `0 0`; the trailer's `/Prev 136746` (previous classic xref) and `/XRefStm
/// 136356` (xref stream) hold the actual object entries. The container refuses
/// that empty incremental subsection today, so this test pins "empty incremental
/// subsection rejected", not "xref stream rejected". It goes red as soon as the
/// container accepts the document, which is when the pin must become a reading
/// assertion for the produced text.
#[test]
fn word2021_export_pins_the_empty_incremental_xref_subsection() {
    let error = read_pdf(WORD).expect_err(
        "the Word 2021 export now parses: the container accepts the hybrid-reference incremental \
         update, so this pin must be replaced with the reading assertion for the produced text",
    );
    assert_eq!(
        error.code, "DOCUMENT_PARSE_ERROR",
        "the Word 2021 export must keep failing at the container layer: {error:?}"
    );
    assert!(
        error.message.contains("subsection"),
        "the container failure must name the empty incremental xref subsection it rejects, got {:?}",
        error.message
    );
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
        "Word 2021 export must keep its object stream or xref stream; without either it is no \
         longer the fixture that pins the container gap"
    );
}
