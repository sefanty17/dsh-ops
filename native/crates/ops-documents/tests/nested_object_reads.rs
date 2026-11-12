//! An indirect `/Length` may name another stream object; parsing that object resolves its own
//! `/Length` first, so every link nests one object read. The loader bounds that nesting, so a chain
//! past 64 reads fails with `OUTPUT_LIMIT` instead of a worker stack overflow.
mod common;

use common::{CATALOG, PAGE, PAGES, Pdf};
use ops_documents::pdf_container::parse;

fn length_chain(links: usize) -> Vec<u8> {
    let last = 3 + links;
    let mut pdf = Pdf::new();
    pdf.object(CATALOG);
    pdf.object(PAGES);
    pdf.object(PAGE);
    for number in 4..=last {
        let length = if number == last {
            "0".to_string()
        } else {
            format!("{} 0 R", number + 1)
        };
        pdf.numbered(
            number,
            format!("<< /Length {length} >>\nstream\n\nendstream").as_bytes(),
        );
    }
    pdf.classic_section(last, last + 1, "/Root 1 0 R");
    pdf.bytes
}

#[test]
fn indirect_length_chain_past_the_read_bound_is_refused() {
    let error = parse(&length_chain(100)).expect_err("lengths that name each other must fail");
    assert_eq!(error.code, "OUTPUT_LIMIT", "{}", error.message);
    assert!(
        error.message.contains("nest more than 64"),
        "{}",
        error.message
    );
}
