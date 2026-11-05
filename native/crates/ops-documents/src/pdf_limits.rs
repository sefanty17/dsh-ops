//! Output limits shared below the PDF layers that enforce them, so no layer imports another's policy.
use ops_protocol::{Failure, Result};
use std::time::{Duration, Instant};
/// Decoded text bytes accepted for one text run and for one page's runs.
pub const MAX_TEXT: usize = 1024 * 1024;
/// Decoded bytes accepted for a page's content streams and for the font tables one text run
/// charges to the same budget.
///
/// The 16 MiB here is page-content work; `MAX_XREF_DECODED` in `pdf_container` separately bounds
/// cross-reference and object stream decoding at 8 MiB.
pub const MAX_CONTENT_DECODED: usize = 16 * 1024 * 1024;
/// Fail once the PDF text layer has spent 30 seconds, whichever layer checks next.
pub fn deadline(started: Instant) -> Result<()> {
    if started.elapsed() >= Duration::from_secs(30) {
        return Err(Failure::new(
            "DEADLINE_EXCEEDED",
            "PDF reading reached its 30-second deadline",
        ));
    }
    Ok(())
}
