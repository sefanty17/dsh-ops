//! Producer-export fixtures: Firefox print, WPS Writer, Word 2021 and LibreOffice.
mod common;

use common::{read, text};
use serde_json::json;

fn fixture(name: &str) -> Vec<u8> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    std::fs::read(&path).unwrap_or_else(|error| panic!("read {path:?}: {error}"))
}

fn has_indirect_length(bytes: &[u8]) -> bool {
    let needle = b"/Length ";
    let mut index = 0;
    while let Some(found) = bytes[index..].windows(needle.len()).position(|w| w == needle) {
        let mut at = index + found + needle.len();
        let digits = |at: &mut usize| {
            let start = *at;
            while *at < bytes.len() && bytes[*at].is_ascii_digit() {
                *at += 1;
            }
            bytes[start..*at].len()
        };
        if digits(&mut at) > 0
            && bytes.get(at) == Some(&b' ')
            && {
                at += 1;
                digits(&mut at) > 0
            }
            && bytes.get(at) == Some(&b' ')
            && bytes.get(at + 1) == Some(&b'R')
        {
            return true;
        }
        index += found + needle.len();
    }
    false
}

#[test]
fn producer_exports_decode_their_text() {
    for (name, expected) in [
        ("producer-firefox-print.pdf", &["中文测试", "Q3 Report", "Gross margin 37.4"][..]),
        ("producer-wps.pdf", &["中文测试", "季度报告与毛利率", "37.4%"]),
        ("producer-word2021.pdf", &["Q3 Report", "中文测试", "Gross margin 37.4"]),
        ("libreoffice-export.pdf", &["Q3 Report"]),
    ] {
        let output = read(&fixture(name), json!({}))
            .unwrap_or_else(|error| panic!("{name}: {}", error.message));
        assert_eq!(output["status"], "completed", "{name}: {output}");
        assert!(output.get("error").is_none(), "{name}: {output}");
        assert_eq!(output["items"].as_array().map(Vec::len), Some(1), "{name}: {output}");
        let page = text(&output);
        for fragment in expected {
            assert!(page.contains(fragment), "{name}: expected {fragment:?} in {page:?}");
        }
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|window| window == needle)
}

fn contains_type(haystack: &[u8], kind: &str) -> bool {
    contains(haystack, format!("/Type/{kind}").as_bytes())
        || contains(haystack, format!("/Type /{kind}").as_bytes())
}

#[test]
fn fixtures_keep_their_container_structures() {
    let firefox = fixture("producer-firefox-print.pdf");
    assert!(firefox.starts_with(b"%PDF-1.4"), "Firefox print export must stay PDF 1.4");
    assert!(
        !contains_type(&firefox, "ObjStm"),
        "Firefox print export must stay free of object streams"
    );
    let wps = fixture("producer-wps.pdf");
    assert!(wps.starts_with(b"%PDF-1.7"), "WPS export must stay PDF 1.7");
    assert!(
        !contains_type(&wps, "ObjStm"),
        "WPS export must stay free of object streams"
    );
    let word = fixture("producer-word2021.pdf");
    assert!(word.starts_with(b"%PDF-1.7"), "Word 2021 export must stay PDF 1.7");
    assert!(
        contains_type(&word, "ObjStm") || contains_type(&word, "XRef"),
        "Word 2021 export must keep its object stream or xref stream"
    );
    assert!(
        contains(&word, b"/Prev"),
        "Word 2021 export must keep its incremental /Prev trailer key"
    );
    let libreoffice = fixture("libreoffice-export.pdf");
    assert!(libreoffice.starts_with(b"%PDF-1.7\n"), "LibreOffice export must stay PDF 1.7");
    assert_eq!(
        &libreoffice[9..18],
        b"%\xC3\xA4\xC3\xBC\xC3\xB6\xC3\x9F",
        "LibreOffice export must keep the UTF-8 second-line marker"
    );
    assert!(
        has_indirect_length(&libreoffice),
        "the fixture must keep an indirect /Length, or it stops covering #646"
    );
}
