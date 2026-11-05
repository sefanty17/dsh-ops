//! An indirect `/Length` may name another stream object, and parsing that object resolves its own
//! `/Length` first, so every link nests one object read. Such a chain can never parse: each link
//! resolves to a stream dictionary rather than an integer. The loader bounds that nesting, so a
//! chain past the bound fails with `OUTPUT_LIMIT` instead of a worker stack overflow
//! (`0xC00000FD`).
//!
//! The fixture is a classic-xref document with one stream object per link, past the 64-read bound.
use ops_documents::pdf_container::parse;

/// A one-page PDF whose objects `4..=3 + links` are empty streams: each `/Length` names the next
/// stream and the last one is direct.
fn length_chain(links: usize) -> Vec<u8> {
    let last = 3 + links;
    let mut bodies = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
        "<< /Type /Page /Parent 2 0 R >>".to_string(),
    ];
    for number in 4..=last {
        let length = if number == last {
            "0".to_string()
        } else {
            format!("{} 0 R", number + 1)
        };
        bodies.push(format!("<< /Length {length} >>\nstream\n\nendstream"));
    }
    let mut pdf = b"%PDF-1.4\n%\xE2\xE3\xCF\xD3\n".to_vec();
    let mut offsets = Vec::new();
    for (index, body) in bodies.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", index + 1).as_bytes());
    }
    let xref = pdf.len();
    let size = bodies.len() + 1;
    pdf.extend_from_slice(format!("xref\n0 {size}\n0000000000 65535 f \n").as_bytes());
    for offset in &offsets {
        pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(
        format!("trailer\n<< /Size {size} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n").as_bytes(),
    );
    pdf
}

#[test]
fn indirect_length_chain_past_the_read_bound_is_refused() {
    let pdf = length_chain(100);

    let error = parse(&pdf).expect_err("streams whose lengths name each other must fail");
    assert_eq!(error.code, "OUTPUT_LIMIT", "{}", error.message);
    assert!(
        error.message.contains("nest more than 64"),
        "{}",
        error.message
    );
}
