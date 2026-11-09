//! /DecodeParms stream predictors: the row filters a Flate payload may carry.
use crate::pdf_container::MAX_STREAM;
use crate::pdf_syntax::{Dict, Value, bad, limit, unsupported};
use ops_protocol::Result;

/// One /DecodeParms predictor: the row it restores and how encoded rows invert it; row is
/// /Columns * /Colors * /BitsPerComponent / 8, and only the 8-bit component depth is accepted.
pub(crate) enum Predictor {
    /// Predictor 2: each byte is the difference from the same component one pixel left.
    Tiff { row: usize, pixel: usize },
    /// Predictors 10..=15: PNG filters; fixed is the filter a 10..=14 predictor names.
    Png {
        row: usize,
        pixel: usize,
        fixed: Option<u8>,
    },
}

/// Read the predictor keys of /DecodeParms for the one supported filter; a missing or null value
/// means no predictor, and the parameters of filters this container lacks are refused by name.
pub(crate) fn decode_parms(dict: &Dict) -> Result<Option<Predictor>> {
    let Some(value) = dict.get(b"DecodeParms".as_slice()) else {
        return Ok(None);
    };
    let parms = match value {
        Value::Null => return Ok(None),
        Value::Dict(parms) => parms,
        Value::Array(_) => {
            return Err(unsupported(
                "PDF stream /DecodeParms arrays are unsupported",
            ));
        }
        _ => return Err(bad("PDF stream /DecodeParms must be a dictionary")),
    };
    for key in parms.keys() {
        if !matches!(
            key.as_slice(),
            b"Predictor" | b"Columns" | b"Colors" | b"BitsPerComponent"
        ) {
            return Err(unsupported(format!(
                "PDF stream decode parameter /{} is unsupported",
                String::from_utf8_lossy(key)
            )));
        }
    }
    let parameter = |key: &[u8], default: usize| -> Result<usize> {
        parms.get(key).map_or(Ok(default), Value::integer)
    };
    let kind = parameter(b"Predictor", 1)?;
    if kind == 1 {
        return Ok(None);
    }
    let columns = parameter(b"Columns", 1)?;
    let colors = parameter(b"Colors", 1)?;
    let bits = parameter(b"BitsPerComponent", 8)?;
    if bits != 8 {
        return Err(unsupported(format!(
            "PDF stream predictor bit depth {bits} is unsupported"
        )));
    }
    // Reject a row that cannot fit in a decoded stream before any row is restored.
    let row = columns
        .checked_mul(colors)
        .filter(|row| *row >= 1 && *row <= MAX_STREAM)
        .ok_or_else(|| limit("PDF predictor row exceeds 1 MiB"))?;
    match kind {
        2 => Ok(Some(Predictor::Tiff { row, pixel: colors })),
        10..=15 => Ok(Some(Predictor::Png {
            row,
            pixel: colors,
            fixed: (kind != 15).then_some((kind - 10) as u8),
        })),
        _ => Err(unsupported(format!(
            "PDF stream predictor {kind} is unsupported"
        ))),
    }
}

impl Predictor {
    /// Undo the predictor on one decoded stream.
    pub(crate) fn apply(self, decoded: Vec<u8>) -> Result<Vec<u8>> {
        match self {
            Self::Tiff { row, pixel } => {
                if !decoded.len().is_multiple_of(row) {
                    return Err(bad(
                        "PDF TIFF predictor stream is not a whole number of rows",
                    ));
                }
                let mut output = decoded;
                for line in output.chunks_exact_mut(row) {
                    for at in pixel..row {
                        line[at] = line[at].wrapping_add(line[at - pixel]);
                    }
                }
                Ok(output)
            }
            Self::Png { row, pixel, fixed } => {
                if decoded.is_empty() {
                    return Ok(decoded);
                }
                // Predictor 15 always tags its rows, but some writers tag the rows of a 10..=14
                // stream too; only a payload that fits both readings uses the leading tag byte.
                let tagged_rows = decoded.len().is_multiple_of(row + 1);
                let fixed_rows = decoded.len().is_multiple_of(row);
                let tagged = match fixed {
                    None => {
                        if !tagged_rows {
                            return Err(bad(
                                "PDF PNG predictor stream is not a whole number of rows",
                            ));
                        }
                        true
                    }
                    Some(_) if tagged_rows && fixed_rows => decoded[0] <= 4,
                    Some(_) => {
                        if !tagged_rows && !fixed_rows {
                            return Err(bad(
                                "PDF PNG predictor stream is not a whole number of rows",
                            ));
                        }
                        tagged_rows
                    }
                };
                let stride = if tagged { row + 1 } else { row };
                let rows = decoded.len() / stride;
                let unfiltered = rows
                    .checked_mul(row)
                    .filter(|bytes| *bytes <= MAX_STREAM)
                    .ok_or_else(|| limit("PDF decoded stream exceeds 1 MiB"))?;
                let mut output = Vec::with_capacity(unfiltered);
                for index in 0..rows {
                    let at = index * stride;
                    let (tag, from) = if tagged {
                        (decoded[at], at + 1)
                    } else {
                        let tag = fixed.ok_or_else(|| {
                            bad("PDF PNG predictor stream is not a whole number of rows")
                        })?;
                        (tag, at)
                    };
                    let start = output.len();
                    output.extend_from_slice(&decoded[from..from + row]);
                    let (prior, line) = output.split_at_mut(start);
                    let prior = if index == 0 {
                        None
                    } else {
                        prior.get(start - row..)
                    };
                    png_row(tag, line, prior, pixel)?;
                }
                Ok(output)
            }
        }
    }
}

/// Undo one PNG predictor row in place; prior is the preceding restored row.
fn png_row(tag: u8, line: &mut [u8], prior: Option<&[u8]>, pixel: usize) -> Result<()> {
    match tag {
        0 => {}
        1 => {
            for at in pixel..line.len() {
                line[at] = line[at].wrapping_add(line[at - pixel]);
            }
        }
        2 => {
            if let Some(prior) = prior {
                for (byte, up) in line.iter_mut().zip(prior) {
                    *byte = byte.wrapping_add(*up);
                }
            }
        }
        3 => {
            for at in 0..line.len() {
                let left = if at >= pixel { line[at - pixel] } else { 0 };
                let up = prior.map_or(0, |prior| prior[at]);
                line[at] = line[at].wrapping_add(((u16::from(left) + u16::from(up)) / 2) as u8);
            }
        }
        4 => {
            for at in 0..line.len() {
                let left = if at >= pixel { line[at - pixel] } else { 0 };
                let up = prior.map_or(0, |prior| prior[at]);
                let corner = if at >= pixel {
                    prior.map_or(0, |prior| prior[at - pixel])
                } else {
                    0
                };
                line[at] = line[at].wrapping_add(paeth(left, up, corner));
            }
        }
        _ => return Err(bad("PDF PNG predictor row filter type is invalid")),
    }
    Ok(())
}

/// PNG's Paeth predictor: the neighbour closest to left + up - corner.
fn paeth(left: u8, up: u8, corner: u8) -> u8 {
    let estimate = i16::from(left) + i16::from(up) - i16::from(corner);
    let left_distance = (estimate - i16::from(left)).abs();
    let up_distance = (estimate - i16::from(up)).abs();
    let corner_distance = (estimate - i16::from(corner)).abs();
    if left_distance <= up_distance && left_distance <= corner_distance {
        left
    } else if up_distance <= corner_distance {
        up
    } else {
        corner
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    /// A stream dictionary whose `/DecodeParms` names one predictor and the row parameters it
    /// reads.
    fn with_predictor(predictor: i64, columns: i64, colors: i64) -> Dict {
        let mut parms = Dict::new();
        parms.insert(b"Predictor".to_vec(), Value::Integer(predictor));
        parms.insert(b"Columns".to_vec(), Value::Integer(columns));
        parms.insert(b"Colors".to_vec(), Value::Integer(colors));
        let mut stream = Dict::new();
        stream.insert(b"DecodeParms".to_vec(), Value::Dict(parms));
        stream
    }
    /// The failure code of parameters this reader must reject.
    fn rejected(dict: &Dict) -> String {
        decode_parms(dict)
            .err()
            .expect("decode parameters must be rejected")
            .code
    }
    #[test]
    fn tiff_predictor_restores_two_rows_from_left_pixel_differences() {
        let predictor = decode_parms(&with_predictor(2, 2, 2))
            .unwrap()
            .expect("Predictor 2 restores rows");
        // Two 4-byte rows of two 2-byte pixels; each byte is the difference from the same
        // component one pixel left, and every row restarts from its own first pixel.
        let decoded = predictor.apply(vec![10, 20, 1, 2, 5, 6, 1, 1]).unwrap();
        assert_eq!(decoded, vec![10, 20, 11, 22, 5, 6, 6, 7]);
    }
    #[test]
    fn illegal_columns_is_rejected_before_any_row_is_restored() {
        assert_eq!(
            rejected(&with_predictor(2, 0, 1)),
            "OUTPUT_LIMIT",
            "a zero /Columns has no row to restore"
        );
        assert_eq!(
            rejected(&with_predictor(2, 2_000_000, 1)),
            "OUTPUT_LIMIT",
            "a row over the 1 MiB stream bound is not an allocation size"
        );
    }
    #[test]
    fn unknown_predictor_is_unsupported() {
        assert_eq!(rejected(&with_predictor(99, 1, 1)), "UNSUPPORTED_DOCUMENT");
    }
}
