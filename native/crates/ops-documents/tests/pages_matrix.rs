//! Page windows, the 32-page bound, per-page failures and `nextOffset` continuation.
mod common;

use common::{
    CATALOG, TempPdf, classic, failed_pages, read, stream, text, text_document,
};
use serde_json::json;

const FONT: &[u8] =
    b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>";
const TYPE0: &[u8] =
    b"<< /Type /Font /Subtype /Type0 /BaseFont /Bare /Encoding /Identity-H /ToUnicode 5 0 R >>";
const TO_UNICODE: &str = "/CIDInit /ProcSet findresource begin\n\
    12 dict begin\nbegincmap\n\
    /CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def\n\
    /CMapName /Adobe-Identity-UCS def\n/CMapType 2 def\n\
    1 begincodespacerange\n<0000> <FFFF>\nendcodespacerange\n\
    1 beginbfchar\n<0001> <D800>\nendbfchar\n\
    endcmap\nCMapName currentdict /CMap defineresource pop\nend\nend\n";

#[derive(Clone)]
enum Page {
    Text(String),
    BrokenUnicode,
}

impl Page {
    fn content(&self) -> String {
        match self {
            Self::Text(text) => format!("BT /F1 12 Tf ({text}) Tj ET"),
            Self::BrokenUnicode => "BT /F2 12 Tf <0001> Tj ET".to_string(),
        }
    }
}

fn pages(count: usize) -> Vec<Page> {
    (1..=count).map(|number| Page::Text(format!("Page {number}"))).collect()
}

/// One page per entry, sharing the WinAnsi and the broken Type0 font resources.
fn document(pages: &[Page]) -> Vec<u8> {
    let first_page = 6;
    let first_content = first_page + pages.len();
    let kids = (0..pages.len())
        .map(|index| format!("{} 0 R", first_page + index))
        .collect::<Vec<_>>()
        .join(" ");
    let mut objects = vec![
        CATALOG.to_vec(),
        format!("<< /Type /Pages /Kids [{kids}] /Count {} >>", pages.len()).into_bytes(),
        FONT.to_vec(),
        TYPE0.to_vec(),
        stream("", TO_UNICODE.as_bytes()),
    ];
    for index in 0..pages.len() {
        objects.push(
            format!(
                "<< /Type /Page /Parent 2 0 R /Resources << /Font << /F1 3 0 R /F2 4 0 R >> >> /Contents {} 0 R >>",
                first_content + index
            )
            .into_bytes(),
        );
    }
    for page in pages {
        objects.push(stream("", page.content().as_bytes()));
    }
    classic(&objects)
}

fn page_numbers(output: &serde_json::Value) -> Vec<u64> {
    output["items"]
        .as_array()
        .expect("items array")
        .iter()
        .map(|item| item["page"].as_u64().expect("page number"))
        .collect()
}

#[test]
fn page_bound_truncates_and_exactly_32_pages_complete() {
    let over = TempPdf::write(&text_document(
        &(1..=40).map(|n| format!("Page {n}")).collect::<Vec<_>>().iter().map(String::as_str).collect::<Vec<_>>(),
    ));
    let output = over.read(json!({ "limit": 10 })).expect("leading pages read");
    assert_eq!(output["status"], "completed", "{output}");
    assert_eq!(output["scanComplete"], false, "{output}");
    assert_eq!(output["truncated"], true, "{output}");
    assert_eq!(output["truncatedBy"], "page-limit", "{output}");
    assert_eq!(output["pageComplete"], true, "{output}");
    assert_eq!(output["complete"], false, "{output}");
    assert_eq!(page_numbers(&output), (1..=10).collect::<Vec<_>>(), "{output}");
    assert_eq!(text(&output), "Page 1", "{output}");
    assert_eq!(output["pagesObserved"], 10, "{output}");
    assert!(output.get("total").is_none() && output.get("omitted").is_none(), "{output}");
    assert!(output.get("error").is_none(), "{output}");
    let action = output["nextAction"].as_str().expect("a repair hint");
    assert!(action.contains("32") && action.contains("dedicated PDF reader"), "{action}");

    let index: Vec<String> = (1..=32).map(|n| format!("Page {n}")).collect();
    let exact = TempPdf::write(&text_document(
        &index.iter().map(String::as_str).collect::<Vec<_>>(),
    ));
    let output = exact.read(json!({ "limit": 32 })).expect("32 pages read");
    assert_eq!(output["status"], "completed", "{output}");
    assert_eq!(output["scanComplete"], true, "{output}");
    assert_eq!(output["truncated"], false, "{output}");
    assert_eq!(output["truncatedBy"], "none", "{output}");
    assert_eq!(output["complete"], true, "{output}");
    assert_eq!(output["total"], 32, "{output}");
    assert!(output.get("nextOffset").is_none(), "{output}");
    assert_eq!(page_numbers(&output).len(), 32, "{output}");
    assert_eq!(output["items"][31]["text"], "Page 32", "{output}");
    assert!(output.get("nextAction").is_none(), "{output}");
}

#[test]
fn a_window_inside_the_page_bound_is_served() {
    let index: Vec<String> = (1..=58).map(|n| format!("Page {n}")).collect();
    let pdf = TempPdf::write(&text_document(
        &index.iter().map(String::as_str).collect::<Vec<_>>(),
    ));
    let output = pdf.read(json!({ "offset": 32, "limit": 10 })).expect("page 32 is observable");
    assert_eq!(output["status"], "completed", "{output}");
    assert_eq!(output["truncatedBy"], "page-limit", "{output}");
    assert_eq!(output["pagesObserved"], 1, "{output}");
    assert_eq!(page_numbers(&output), vec![32], "{output}");
    assert_eq!(text(&output), "Page 32", "{output}");
    assert!(output.get("error").is_none(), "{output}");

}

fn empty_document() -> Vec<u8> {
    classic(&[
        CATALOG.to_vec(),
        b"<< /Type /Pages /Kids [] /Count 0 >>".to_vec(),
    ])
}

#[test]
fn offsets_past_the_document_are_argument_errors() {
    let one = TempPdf::write(&text_document(&["Page 1"]));
    let error = one.read(json!({ "offset": 2 })).expect_err("offset 2 on a one-page document");
    assert_eq!(error.code, "INVALID_ARGUMENT");
    assert_eq!(error.message, "offset 2 exceeds the document's 1 pages");
    assert!(error.next_action.contains("Use an offset from 1 to 1"), "{}", error.next_action);

    let empty = TempPdf::write(&empty_document());
    let output = empty.read(json!({})).expect("zero pages are an observation");
    assert_eq!(output["total"], 0, "{output}");
    assert_eq!(output["complete"], true, "{output}");
    let error = empty.read(json!({ "offset": 2 })).expect_err("offset 2 on a zero-page document");
    assert_eq!(error.code, "INVALID_ARGUMENT");
    assert_eq!(error.message, "offset 2 exceeds the document's 0 pages");
    assert!(error.next_action.contains("no pages"), "{}", error.next_action);

    for extra in [json!({ "offset": 0 }), json!({ "limit": 33 })] {
        let error = one.read(extra.clone()).expect_err("out-of-range arguments must fail");
        assert_eq!(error.code, "INVALID_ARGUMENT", "{extra}");
        assert!(error.next_action.contains("1-32"), "{}", error.next_action);
    }
}

#[test]
fn failed_pages_stay_inside_the_requested_window() {
    let mut entries = pages(15);
    entries[11] = Page::BrokenUnicode;
    let pdf = TempPdf::write(&document(&entries));

    let head = pdf.read(json!({ "limit": 3 })).expect("readable window");
    assert_eq!(head["status"], "completed", "{head}");
    assert_eq!(head["pagesObserved"], 3, "{head}");
    assert_eq!(head["scanComplete"], true, "{head}");
    assert_eq!(head["nextOffset"], 4, "{head}");
    assert_eq!(head["total"], 15, "{head}");
    assert_eq!(page_numbers(&head), vec![1, 2, 3], "{head}");
    assert!(head.get("failedPages").is_none(), "a failure after the window must not leak: {head}");

    let failed = pdf.read(json!({ "offset": 12, "limit": 1 })).expect("one failed page is a value");
    assert_eq!(failed["status"], "partial", "{failed}");
    assert_eq!(failed["pagesObserved"], 1, "{failed}");
    assert_eq!(failed["pageComplete"], true, "{failed}");
    assert_eq!(failed["scanComplete"], false, "{failed}");
    assert_eq!(failed["nextOffset"], 13, "{failed}");
    assert!(failed["items"].as_array().is_some_and(Vec::is_empty), "{failed}");
    assert_eq!(failed_pages(&failed), vec![(12, "DOCUMENT_PARSE_ERROR".to_string())], "{failed}");
    assert!(failed.get("error").is_none(), "{failed}");

    let tail = pdf.read(json!({ "offset": 13, "limit": 3 })).expect("pages after the failure read");
    assert_eq!(tail["status"], "completed", "{tail}");
    assert_eq!(tail["pagesObserved"], 3, "{tail}");
    assert_eq!(tail["scanComplete"], true, "{tail}");
    assert_eq!(tail["nextOffset"], 16, "{tail}");
    assert_eq!(tail["total"], 15, "{tail}");
    assert_eq!(tail["complete"], true, "{tail}");
    assert_eq!(page_numbers(&tail), vec![13, 14, 15], "{tail}");
    assert!(tail.get("failedPages").is_none(), "a failure before the window must not leak: {tail}");
}

#[test]
fn trailing_failed_pages_keep_the_readable_prefix() {
    let mut entries = pages(6);
    entries.extend((7..=15).map(|_| Page::BrokenUnicode));
    let output = read(&document(&entries), json!({ "limit": 15 })).expect("partial read");
    assert_eq!(output["status"], "partial", "{output}");
    assert_eq!(output["pagesObserved"], 15, "{output}");
    assert_eq!(output["pageComplete"], true, "{output}");
    assert_eq!(output["scanComplete"], false, "{output}");
    assert_eq!(output["complete"], false, "{output}");
    assert_eq!(output["truncated"], true, "{output}");
    assert_eq!(output["truncatedBy"], "parse-error", "{output}");
    assert_eq!(output["nextOffset"], 16, "{output}");
    assert!(output.get("error").is_none(), "a partial read has no top-level error: {output}");
    assert!(output.get("total").is_none() && output.get("revision").is_none(), "{output}");
    assert_eq!(page_numbers(&output), vec![1, 2, 3, 4, 5, 6], "{output}");
    assert_eq!(
        failed_pages(&output),
        (7..=15).map(|page| (page, "DOCUMENT_PARSE_ERROR".to_string())).collect::<Vec<_>>(),
        "{output}"
    );
    for entry in output["failedPages"].as_array().expect("failedPages array") {
        assert_eq!(entry.as_object().map(serde_json::Map::len), Some(3), "{entry}");
        let message = entry["message"].as_str().expect("message");
        assert!(message.contains("surrogate"), "{message}");
        assert!(message.chars().count() <= 200, "{message}");
    }
    let action = output["nextAction"].as_str().expect("a repair hint");
    assert!(action.contains("file structure"), "{action}");
}

#[test]
fn every_requested_page_failing_is_a_top_level_error() {
    let entries = vec![Page::BrokenUnicode; 5];
    let output = read(&document(&entries), json!({ "limit": 5 })).expect("observation");
    assert_eq!(output["status"], "error", "{output}");
    assert_eq!(output["pagesObserved"], 5, "{output}");
    assert_eq!(output["pageComplete"], true, "{output}");
    assert_eq!(output["scanComplete"], false, "{output}");
    assert_eq!(output["nextOffset"], 6, "{output}");
    assert!(output["items"].as_array().is_some_and(Vec::is_empty), "{output}");
    assert_eq!(output["error"]["code"], "DOCUMENT_PARSE_ERROR", "{output}");
    let error = output["error"]["message"].as_str().expect("first failure message");
    assert!(error.contains("surrogate"), "{error}");
    assert_eq!(failed_pages(&output).len(), 5, "{output}");
    assert_eq!(output["failedPages"][0]["message"].as_str(), Some(error), "{output}");
}

#[test]
fn next_offset_reads_the_rest_of_the_document() {
    let mut entries = pages(10);
    entries[3] = Page::BrokenUnicode;
    let pdf = TempPdf::write(&document(&entries));

    let first = pdf.read(json!({ "limit": 3 })).expect("leading window");
    assert_eq!(first["nextOffset"], 4, "{first}");
    let second = pdf.read(json!({ "offset": 4, "limit": 3 })).expect("follow nextOffset");
    assert_eq!(second["status"], "partial", "{second}");
    assert_eq!(second["nextOffset"], 7, "{second}");
    assert_eq!(page_numbers(&second), vec![5, 6], "{second}");
    assert_eq!(failed_pages(&second), vec![(4, "DOCUMENT_PARSE_ERROR".to_string())], "{second}");
    let third = pdf.read(json!({ "offset": 7, "limit": 4 })).expect("follow nextOffset again");
    assert_eq!(third["status"], "completed", "{third}");
    assert_eq!(page_numbers(&third), vec![7, 8, 9, 10], "{third}");
    assert_eq!(third["scanComplete"], true, "{third}");
    assert_eq!(third["complete"], true, "{third}");
}

#[test]
fn an_output_cut_stops_at_the_first_unreturned_page() {
    let entries = (1..=32)
        .map(|number| Page::Text(format!("{number}:{}", "x".repeat(8000))))
        .collect::<Vec<_>>();
    let output = read(&document(&entries), json!({ "limit": 32 })).expect("large readable document");
    let items = output["items"].as_array().expect("items array");
    assert!(items.len() < 32, "the inline page list must be cut: {}", items.len());
    assert_eq!(output["status"], "completed", "{output}");
    assert_eq!(output["pageComplete"], false, "{output}");
    assert_eq!(output["scanComplete"], false, "{output}");
    assert_eq!(output["complete"], false, "{output}");
    assert_eq!(output["truncatedBy"], "output-budget", "{output}");
    assert!(output.get("total").is_none() && output.get("revision").is_none(), "{output}");
    let first_cut = items.len() as u64 + 1;
    assert_eq!(output["nextOffset"], first_cut, "{output}");
    assert_eq!(page_numbers(&output), (1..=first_cut - 1).collect::<Vec<_>>(), "{output}");
    assert!(output.get("failedPages").is_none(), "{output}");
    assert!(
        output["nextAction"].as_str().is_some_and(|action| action.contains("reduce the page limit")),
        "{output}"
    );
}

#[test]
fn a_next_offset_beyond_the_page_range_is_omitted() {
    let mut entries = pages(31);
    entries.push(Page::BrokenUnicode);
    let output = read(&document(&entries), json!({ "offset": 31, "limit": 2 })).expect("observation");
    assert_eq!(output["status"], "partial", "{output}");
    assert_eq!(output["truncatedBy"], "parse-error", "{output}");
    assert!(output.get("nextOffset").is_none(), "a cursor past 32 cannot be requested: {output}");
    assert_eq!(page_numbers(&output), vec![31], "{output}");
    assert_eq!(failed_pages(&output), vec![(32, "DOCUMENT_PARSE_ERROR".to_string())], "{output}");
    assert!(
        output["nextAction"].as_str().is_some_and(|action| action.contains("file structure")),
        "{output}"
    );
}

#[test]
fn page_counts_are_resolved_or_refused() {
    let mut indirect = vec![
        CATALOG.to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 4 0 R >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R >>".to_vec(),
        b"1".to_vec(),
    ];
    indirect.truncate(4);
    let bytes = classic(&indirect);
    let output = read(&bytes, json!({})).expect("indirect /Count reads");
    assert_eq!(output["total"], 1, "{output}");

    let count = |value: &str| {
        classic(&[
            CATALOG.to_vec(),
            format!("<< /Type /Pages /Kids [3 0 R] /Count {value} >>").into_bytes(),
            b"<< /Type /Page /Parent 2 0 R >>".to_vec(),
            b"1".to_vec(),
        ])
    };
    for (case, value, needle) in [
        ("dangling", "9 0 R", "does not name an active object"),
        ("string", "(one)", "must be a bounded nonnegative integer"),
        ("negative", "-1", "must be a bounded nonnegative integer"),
        ("mismatch", "33", "page Count disagrees with observed tree"),
    ] {
        let error = read(&count(value), json!({})).expect_err(case);
        assert_eq!(error.code, "DOCUMENT_PARSE_ERROR", "{case}: {}", error.message);
        assert!(error.message.contains(needle), "{case}: {}", error.message);
    }
    let cyclic = vec![
        CATALOG.to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 4 0 R >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R >>".to_vec(),
        b"4 0 R".to_vec(),
    ];
    let error = read(&classic(&cyclic), json!({})).expect_err("self-referencing count");
    assert_eq!(error.code, "DOCUMENT_PARSE_ERROR", "{}", error.message);
    assert!(error.message.contains("Cyclic"), "{}", error.message);
}
