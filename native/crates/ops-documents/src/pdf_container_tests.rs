//! Private container details that no public read path reaches: the string/comment scanner and the
//! literal-string lexer.
use super::*;

#[test]
fn literal_strings_escapes_line_endings_and_odd_hex_are_exact() {
    let mut lexer = Lexer::new(b"(a\\n\\r\\t\\b\\f\\(b\\)\\134\\101\\\r\nc\r\nd)");
    assert!(
        matches!(lexer.value(0).unwrap(), Value::String(v) if v == b"a\n\r\t\x08\x0c(b)\\Ac\nd")
    );
    let mut lexer = Lexer::new(b"<123>");
    assert!(matches!(lexer.value(0).unwrap(), Value::String(v) if v == [0x12, 0x30]));
    assert!(Lexer::new(b"1.0").value(0).unwrap().integer().is_err());
    assert!(Lexer::new(b"+1").value(0).unwrap().integer().is_ok());
}

#[test]
fn comment_before_tracks_only_strings_opened_on_its_line() {
    // A string opened and closed on the line owns its `%`, so the header after it is readable.
    assert!(!comment_before(b"(100%) 2 0 obj", 0, 7));
    // A `%` outside a string comments the header.
    assert!(comment_before(b"endobj % 1 0 obj", 0, 9));
    // A string continued from an earlier line is classified as outside a string, so the `%` on
    // the header's line comments the header.
    assert!(comment_before(b"(open\n% 1 0 obj", 6, 8));
}
