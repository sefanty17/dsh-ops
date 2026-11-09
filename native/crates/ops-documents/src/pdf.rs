//! Read-only, stable-scope PDF page observations. Unsupported content never becomes complete text.
use crate::{
    pdf_container::{self, MAX_FILE},
    pdf_pages::{self, MAX_PAGES, Streams},
    pdf_text,
};
use ops_protocol::{Failure, Request, Result, capture::json_size};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{io::Read, path::Path, time::Instant};
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    path: String,
    #[serde(default = "super::one")]
    offset: usize,
    #[serde(default = "ten")]
    limit: usize,
}
/// True when the request window was served: no global failure or output cut stopped it, and the
/// 32-page bound did not end the tree before the window. A page whose extraction failed still
/// counts as attempted, and a tree shorter than the request window still covers it.
fn page_complete(
    global: Option<&Failure>,
    cut: bool,
    limited: bool,
    window_end: usize,
    total: usize,
) -> bool {
    global.is_none() && !cut && !(limited && window_end > total)
}
fn ten() -> usize {
    10
}
/// Next step for a file whose physical page count exceeds the 32-page scan bound.
const PAGE_LIMIT_ACTION: &str = "This file has more than 32 physical pages and only the first 32 are observable; page offsets cannot bypass that physical bound. Use a dedicated PDF reader to read the whole file.";
/// Next step for a window stopped by the inline output budget.
const CUT_ACTION: &str = "Read the stored reference when available; otherwise reduce the page limit or use a dedicated reader for a large page.";
/// Next step for an encrypted document, which no page argument can decrypt.
const ENCRYPT_ACTION: &str = "This PDF is encrypted. Remove the open password and re-export it without encryption, or open it in a dedicated PDF reader; page offsets and limits cannot decrypt a document.";
/// Next step for an unsupported-content failure, whose repair depends on the cause in the message.
fn unsupported_next_action(message: &str) -> &'static str {
    // Encryption and font markers name different repairs; the generic branch covers the rest.
    let message = message.to_ascii_lowercase();
    if message.contains("encrypt") || message.contains("加密") {
        ENCRYPT_ACTION
    } else if message.contains("font") || message.contains("tounicode") {
        "This PDF's fonts carry no usable Unicode mapping. Re-export with fonts embedded and a /ToUnicode CMap for every font, or use a dedicated PDF reader; page offsets cannot recover text whose glyphs have no Unicode mapping."
    } else {
        "The file contains PDF content this reader does not support. Use a dedicated PDF reader, or re-export it so fonts are embedded; page offsets do not bypass unsupported content. OCR and rendered reading order are not provided."
    }
}
/// Replace the generic hint with the repair this failure code requires.
fn recovery(error: &mut Failure) {
    error.next_action = match error.code.as_str() {
        "ENCRYPTED" => ENCRYPT_ACTION,
        "UNSUPPORTED_DOCUMENT" => unsupported_next_action(&error.message),
        "DOCUMENT_PARSE_ERROR" => {
            "The file structure does not match the expected PDF syntax. Confirm the file is intact and not truncated, or use a dedicated PDF reader; pages before the failure remain observations."
        }
        "OUTPUT_LIMIT" => {
            "This page or file exceeds a decode or output budget. Reduce the page limit, read the pages in smaller ranges, or use a dedicated PDF reader; ordinary page offsets do not bypass a physical decode limit."
        }
        "DEADLINE_EXCEEDED" => {
            "Reading reached its 30-second deadline. Reduce the page limit or use a dedicated PDF reader; pages that were not read are unknown, not empty."
        }
        "FILE_CHANGED" => {
            "The file changed while it was being read, so these page observations are not a stable snapshot. Read it again to obtain a new revision; do not treat the observed pages as complete."
        }
        _ => {
            "Use a dedicated PDF reader for unsupported content or physical scan limits; page offsets do not bypass those gaps. OCR and rendered reading order are not provided."
        }
    }
    .into();
}
/// The truncation cause a failure code implies.
fn cause_of(code: &str) -> &'static str {
    match code {
        "DEADLINE_EXCEEDED" => "time",
        "FILE_CHANGED" => "changed",
        "OUTPUT_LIMIT" => "scan-budget",
        "UNSUPPORTED_DOCUMENT" => "unsupported-content",
        _ => "parse-error",
    }
}
/// The reported stopping cause and repair hint, most significant first: a global failure
/// (deadline or changed file) invalidates every observation, an output cut withholds pages, and a
/// page failure is local to that page. The scan bounds rank last. A budget hint under an unstable
/// snapshot would send the caller to a smaller window that is equally unstable.
fn stopping(
    global: Option<&Failure>,
    cut: bool,
    first_failed: Option<&(usize, Failure)>,
    page_limited: bool,
    more: bool,
) -> (&'static str, Option<String>) {
    if let Some(error) = global {
        let mut error = error.clone();
        recovery(&mut error);
        return (cause_of(&error.code), Some(error.next_action.to_string()));
    }
    if cut {
        return ("output-budget", Some(CUT_ACTION.to_owned()));
    }
    if let Some((_, error)) = first_failed {
        let mut error = error.clone();
        recovery(&mut error);
        return (cause_of(&error.code), Some(error.next_action.to_string()));
    }
    if page_limited {
        return ("page-limit", Some(PAGE_LIMIT_ACTION.to_owned()));
    }
    (if more { "row-limit" } else { "none" }, None)
}
/// One failed page's message within the per-entry report bound; the cut never splits a character.
fn page_message(message: &str) -> String {
    const MAX: usize = 200;
    if message.chars().count() <= MAX {
        return message.to_owned();
    }
    let mut truncated: String = message.chars().take(MAX - 1).collect();
    truncated.push('…');
    truncated
}
/// Recovery for a failure raised before any output exists; the call site names the step.
fn fail(mut error: Failure, next_action: &'static str) -> Failure {
    error.next_action = next_action.into();
    error
}
/// The argument error for a window that starts after the last page. The page tree is walked first,
/// so this names the document's own page count instead of reporting an empty completed read.
fn offset_error(offset: usize, total_pages: usize) -> Failure {
    let mut error = Failure::new(
        "INVALID_ARGUMENT",
        format!("offset {offset} exceeds the document's {total_pages} pages"),
    );
    error.next_action = if total_pages == 0 {
        "This document has no pages, so only the default window at offset 1 describes it. Confirm the file is intact or use a dedicated PDF reader; offset and limit cannot create pages.".to_owned()
    } else {
        format!("read_pdf reads pages by 1-based number and this document has {total_pages} pages. Use an offset from 1 to {total_pages}; a larger offset cannot read a page the document does not have, and limit cannot change the page count.")
    }
    .into();
    error
}
/// Read page text from one PDF file; pages read before a failure remain observations.
pub fn read(request: &Request) -> Result<Value> {
    let args: Args = serde_json::from_value(request.args.clone())?;
    if args.offset == 0 || args.offset > MAX_PAGES || args.limit == 0 || args.limit > MAX_PAGES {
        return Err(fail(
            Failure::new(
                "INVALID_ARGUMENT",
                "read_pdf offset and limit are 1-32 pages (defaults 1 and 10)",
            ),
            "read_pdf takes a 1-based offset in 1-32 and a limit in 1-32 pages; this request read no page. Correct the arguments rather than paging around the physical 32-page limit.",
        ));
    }
    request.authorize_path(Path::new(&args.path))?;
    let target = ops_files::file_path::checked(&args.path)?;
    request.authorize_path(&target.display)?;
    let mut file = ops_files::file_path::read_file(&target)?.into_std();
    let before = file.metadata()?;
    let started = Instant::now();
    if before.len() > MAX_FILE as u64 {
        return Err(fail(
            Failure::new("OUTPUT_LIMIT", "PDF input exceeds 16 MiB"),
            "This PDF is larger than the 16 MiB input limit, which bounds the whole file: reducing the page limit or offset cannot make it readable. Use a dedicated PDF reader for this file; no page text was read.",
        ));
    }
    let mut bytes = Vec::new();
    file.by_ref()
        .take(MAX_FILE as u64 + 1)
        .read_to_end(&mut bytes)?;
    // Container failures happen before the page loop, so they need the same repair hint the loop
    // gives a failed page; otherwise an encrypted file keeps the protocol's generic advice.
    let document = pdf_container::parse(&bytes).map_err(|mut error| {
        recovery(&mut error);
        error
    })?;
    if bytes[7] < b'3' {
        return Err(fail(
            Failure::new(
                "UNSUPPORTED_DOCUMENT",
                "PDF text extraction supports PDF 1.3-1.7 encoding semantics",
            ),
            "This PDF declares a version below 1.3, and text extraction needs PDF 1.3-1.7 encoding semantics. Use a dedicated PDF reader, or re-export the file as PDF 1.4 or later; page offsets and limits cannot change the version. No page text was read.",
        ));
    }
    let tree = pdf_pages::pages(&document, started)?;
    let page_limited = tree.truncated;
    let pages = tree.pages;
    let total_pages = pages.len();
    // A window starting past the last page would otherwise report a completed read of nothing. An
    // empty document still answers the default window at offset 1.
    if args.offset > total_pages.max(1) {
        return Err(offset_error(args.offset, total_pages));
    }
    let mut streams = Streams::new();
    // Font tables share the page streams' aggregate decoded budget.
    let mut fonts = pdf_text::Fonts::new();
    // Only window pages decode; a failed page keeps its own entry and the read continues; a deadline aborts.
    let (mut observed, mut inline_bytes, mut last_attempted) = (0usize, 0usize, 0usize);
    let (mut items, mut global_failure, mut cut, mut cut_at) = (Vec::new(), None, false, None);
    let mut failed_pages: Vec<(usize, Failure)> = Vec::new();
    let window_end = args.offset + args.limit - 1;
    for (index, page) in pages.iter().enumerate() {
        let number = index + 1;
        if number < args.offset {
            continue;
        }
        if number > window_end {
            break;
        }
        let result = streams.content(&document, page, started).and_then(|content| {
            pdf_text::extract(
                &document,
                page.resources,
                &content,
                started,
                &mut fonts,
                &mut streams.decoded,
            )
        });
        let runs = match result {
            Ok(runs) => runs,
            Err(error) if error.code == "DEADLINE_EXCEEDED" => {
                global_failure = Some(error);
                break;
            }
            Err(error) => {
                observed += 1;
                last_attempted = number;
                failed_pages.push((number, error));
                continue;
            }
        };
        observed += 1;
        last_attempted = number;
        let item = json!({"page":number,"text":runs.concat(),"runs":runs});
        if let Some(sink) = request.output_sink.as_deref() {
            sink.push(&item)
        }
        let cost = json_size(&item) + 1;
        if cut || inline_bytes + cost > 80 * 1024 {
            cut = true;
            if cut_at.is_none() {
                cut_at = Some(number);
            }
            if let Some(sink) = request.output_sink.as_deref() {
                sink.clipped()
            }
        } else {
            inline_bytes += cost;
            items.push(item)
        }
    }
    let after = file.metadata()?;
    if before.len() != after.len() || before.modified().ok() != after.modified().ok() {
        global_failure = Some(Failure::new(
            "FILE_CHANGED",
            "PDF metadata changed while reading; page observations are not a stable snapshot",
        ));
    }
    let scan = global_failure.is_none() && failed_pages.is_empty() && !page_limited && !cut;
    let next = if let Some(number) = cut_at {
        number
    } else if last_attempted >= args.offset {
        last_attempted + 1
    } else {
        args.offset
    };
    let more = !scan || next <= total_pages;
    // scan needs the unbounded walk and no cut; status is error only when every bounded page failed.
    let status = if global_failure.is_some() {
        "partial"
    } else if failed_pages.is_empty() {
        "completed"
    } else if total_pages > 0 && failed_pages.len() == total_pages {
        "error"
    } else {
        "partial"
    };
    let (cause, next_action) = stopping(
        global_failure.as_ref(),
        cut,
        failed_pages.first(),
        page_limited,
        more,
    );
    let page_complete = page_complete(
        global_failure.as_ref(),
        cut,
        page_limited,
        window_end,
        total_pages,
    );
    let mut output = json!({"status":status,"phase":"verify","changed":0,"effectState":"none","path":target.display,"format":"pdf",
        "scope":"page-text-layer-content-order","excludedContent":["rendered layout and reading order","images and OCR","annotations and form fields","attachments and document metadata","structure-tree alternate text","text painted by tiling patterns"],
        "bytes":before.len(),"decodedBytes":streams.decoded,"processedBytes":streams.processed,"pagesObserved":observed,"skipped":(args.offset-1).min(total_pages),
        "scanComplete":scan,"pageComplete":page_complete,"complete":scan&&!more,"truncated":more,"truncatedBy":cause,"items":[]});
    // A nextOffset beyond the tool's own 1-32 offset range cannot be requested, so the failure
    // list and nextAction carry the recovery instead.
    if next <= MAX_PAGES {
        output["nextOffset"] = json!(next);
    }
    if scan {
        output["total"] = json!(total_pages);
        output["omitted"] = json!(total_pages.saturating_sub(next - 1));
        output["revision"] = json!(format!("{:x}", Sha256::digest(&bytes)))
    }
    if let Some(mut error) = global_failure {
        recovery(&mut error);
        output["error"] = json!(error);
    } else if let Some((_, first)) = failed_pages.first().filter(|_| status == "error") {
        let mut primary = first.clone();
        recovery(&mut primary);
        output["error"] = json!(primary);
    }
    if !failed_pages.is_empty() {
        output["failedPages"] = json!(failed_pages.iter().map(|(page, error)| json!({
            "page": page, "code": error.code, "message": page_message(&error.message)
        })).collect::<Vec<_>>());
    }
    // The repair hint follows the same priority as the cause: a failed entry stays in
    // failedPages, but it cannot displace a changed file, a deadline or an output cut.
    if let Some(action) = next_action {
        output["nextAction"] = json!(action);
    }
    if let Some(sink) = request.output_sink.as_deref() {
        sink.source(output.clone())
    }
    output["items"] = json!(items);
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_complete_requires_a_served_window() {
        let changed = Failure::new("FILE_CHANGED", "changed");
        assert!(page_complete(None, false, false, 3, 15));
        assert!(page_complete(None, false, false, 40, 15));
        assert!(page_complete(None, false, true, 10, 32));
        assert!(!page_complete(None, false, true, 40, 32));
        assert!(!page_complete(Some(&changed), false, false, 15, 15));
        assert!(!page_complete(None, true, false, 15, 15));
    }

    #[test]
    fn stopping_prioritizes_global_then_cut_then_page_failure() {
        let changed = Failure::new("FILE_CHANGED", "changed");
        let deadline = Failure::new("DEADLINE_EXCEEDED", "late");
        let (cause, action) = stopping(Some(&changed), true, None, false, true);
        assert_eq!(cause, "changed");
        assert!(action.expect("global repair").contains("stable snapshot"));
        let (cause, _) = stopping(Some(&deadline), true, None, false, true);
        assert_eq!(cause, "time");
        let failed = (2, Failure::new("UNSUPPORTED_DOCUMENT", "unsupported"));
        let (cause, action) = stopping(None, true, Some(&failed), false, true);
        assert_eq!(cause, "output-budget");
        assert_eq!(action.as_deref(), Some(CUT_ACTION));
        let (cause, action) = stopping(None, false, Some(&failed), false, true);
        assert_eq!(cause, "unsupported-content");
        assert!(action.is_some_and(|action| action.contains("does not support")));
        assert_eq!(
            stopping(None, false, None, true, true),
            ("page-limit", Some(PAGE_LIMIT_ACTION.to_owned()))
        );
        assert_eq!(stopping(None, false, None, false, false), ("none", None));
    }

    #[test]
    fn page_messages_are_cut_without_splitting_a_character() {
        assert_eq!(page_message("short"), "short");
        let cut = page_message(&"é".repeat(250));
        assert_eq!(cut.chars().count(), 200);
        assert!(cut.ends_with('…') && cut.starts_with('é'));
    }
}
