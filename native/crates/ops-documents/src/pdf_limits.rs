//! Output limits shared below the PDF layers that enforce them, so no layer imports another's policy.

/// Decoded text bytes accepted for one text run and for one page's runs.
pub const MAX_TEXT: usize = 1024 * 1024;
