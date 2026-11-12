use super::*;
use ops_protocol::Result;

const TWO: &str = "<0000> <FFFF>";

fn parse(body: &str) -> Result<CMap> {
    let bytes = format!(
        "/CIDInit /ProcSet findresource begin\n12 dict begin\nbegincmap\n\
         /CMapType 2 def\n1 begincodespacerange\n{TWO}\nendcodespacerange\n{body}\n\
         endcmap CMapName currentdict /CMap defineresource pop end end\n"
    )
    .into_bytes();
    CMap::parse(&bytes)
}

#[test]
fn cmap_mappings_cover_bfchar_bfrange_arrays_and_surrogates() {
    let table = parse(
        "4 beginbfchar\n<0E3C> <4E2D>\n<0004> <D83DDE00>\n<0005> <00660066>\n<0006> <0000>\n\
         endbfchar\n1 beginbfrange\n<0010> <0012> <0041>\nendbfrange\n\
         1 beginbfrange\n<0020> <0022> [<4E2D> <D83DDE00> <0066>]\nendbfrange",
    )
    .expect("the CMap must parse");
    assert_eq!(table.width(), 2);
    assert_eq!(table.entries(), 10);
    assert_eq!(table.target(0x0E3C), Some("中"));
    assert_eq!(table.target(0x0004), Some("😀"));
    assert_eq!(table.target(0x0005), Some("ff"));
    assert_eq!(table.target(0x0006), Some("\0"));
    assert_eq!(
        (0x10..=0x12)
            .map(|code| table.target(code))
            .collect::<Vec<_>>(),
        [Some("A"), Some("B"), Some("C")]
    );
    assert_eq!(table.target(0x20), Some("中"));
    assert_eq!(table.target(0x21), Some("😀"));
    assert_eq!(table.target(0x22), Some("f"));
}
