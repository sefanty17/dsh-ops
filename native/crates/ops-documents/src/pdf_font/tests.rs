//! Public font-decoder shape that the reader uses internally: kind, code width and charged bytes.
use super::*;
use crate::pdf_container::parse;

const TWO: &str = "<0000> <FFFF>";
const IDENTITY: &str = "<< /Type /Font /Subtype /Type0 /BaseFont /X \
                        /Encoding /Identity-H /ToUnicode 5 0 R >>";

fn cmap(codespace: &str, body: &str) -> Vec<u8> {
    format!(
        "/CIDInit /ProcSet findresource begin\n12 dict begin\nbegincmap\n\
         /CMapType 2 def\n1 begincodespacerange\n{codespace}\nendcodespacerange\n{body}\n\
         endcmap CMapName currentdict /CMap defineresource pop end end\n"
    )
    .into_bytes()
}

fn document(font: &str, to_unicode: Option<&[u8]>) -> Vec<u8> {
    let mut objects = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /Resources << /Font << /F1 4 0 R >> >> >>".to_vec(),
        font.as_bytes().to_vec(),
    ];
    if let Some(body) = to_unicode {
        objects.push(stream_object(body));
    }
    let mut pdf = b"%PDF-1.4\n%\xE2\xE3\xCF\xD3\n".to_vec();
    let mut offsets = Vec::new();
    for (index, body) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.extend_from_slice(format!("{} 0 obj\n", index + 1).as_bytes());
        pdf.extend_from_slice(body);
        pdf.extend_from_slice(b"\nendobj\n");
    }
    let xref = pdf.len();
    let size = objects.len() + 1;
    pdf.extend_from_slice(format!("xref\n0 {size}\n0000000000 65535 f \n").as_bytes());
    for offset in offsets {
        pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(
        format!("trailer\n<< /Size {size} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n").as_bytes(),
    );
    pdf
}

fn stream_object(body: &[u8]) -> Vec<u8> {
    let mut object = format!("<< /Length {} >>\nstream\n", body.len()).into_bytes();
    object.extend_from_slice(body);
    object.extend_from_slice(b"\nendstream");
    object
}

#[test]
fn font_decoders_report_kind_width_and_charged_bytes() {
    let table = cmap(TWO, "1 beginbfchar\n<0001> <0041>\nendbfchar");
    let bytes = document(IDENTITY, Some(&table));
    let pdf = parse(&bytes).expect("the fixture document must parse");
    let decoder = FontDecoder::from_value(&pdf, &Value::Ref(4, 0)).expect("identity font");
    assert_eq!(decoder.kind(), Kind::Identity);
    assert_eq!(decoder.code_width(), 2);
    assert_eq!(decoder.decoded_bytes(), table.len());
    assert_eq!(decoder.decode(&[0x00, 0x01]).expect("mapped code"), "A");

}
