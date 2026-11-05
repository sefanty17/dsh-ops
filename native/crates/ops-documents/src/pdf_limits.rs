//! Output limits shared below the PDF layers that enforce them, so no layer imports another's policy.
//!
//! Only the text budget is here before the font layer that charges it; the content budget and
//! deadline move here with the layer that replaces `pdf_text`'s local copies.
#![allow(dead_code)]
/// Decoded text bytes accepted for one text run and for one page's runs.
pub const MAX_TEXT: usize = 1024 * 1024;
