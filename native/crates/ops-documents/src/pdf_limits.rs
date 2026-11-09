//! Output limits shared by the PDF layers that enforce them, with no cross-layer policy import.
use ops_protocol::{Failure, Result};
use std::time::{Duration, Instant};
/// Decoded text bytes accepted for one text run and for one page's runs.
pub const MAX_TEXT: usize = 1024 * 1024;
/// Decoded bytes accepted for a page's content streams, the form XObject content they reach, and
/// the font tables a run charges.
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
