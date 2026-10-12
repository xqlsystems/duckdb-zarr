//! Coordinate-range pruning for `read_zarr(..., bounds := [[dim, lo, hi], ...])`.
//!
//! The DuckDB C extension API has no filter-pushdown hook, so the range is passed
//! explicitly. Each bound is resolved against the cached 1-D coordinate array into
//! a per-index mask. The mask is the single source of truth for both decisions:
//! a chunk is read iff some index in its span is kept, and a row is emitted iff its
//! index is kept in every bounded dimension. Because it goes through the actual
//! coordinate values, descending, non-uniform and non-monotonic axes need no special
//! casing. Bounds are inclusive and compared in `f64` (TIMESTAMPs as microseconds
//! since the epoch).

use chrono::NaiveDate;

use super::types::{ColumnEncoding, CoordArray};

/// One parsed `[dim, lo, hi]` entry. `None` means unbounded on that side.
#[derive(Debug, Clone, PartialEq)]
pub struct RawBound {
    pub dim: String,
    pub lo: Option<String>,
    pub hi: Option<String>,
}

/// 2^53: beyond this an `f64` can no longer hold every integer exactly.
const EXACT_F64: f64 = 9_007_199_254_740_992.0;

/// Parse one bound endpoint for a coordinate with the given encoding.
pub fn parse_endpoint(
    text: &str,
    encoding: &ColumnEncoding,
    dim: &str,
) -> Result<f64, Box<dyn std::error::Error>> {
    let text = text.trim();
    let value = match encoding {
        ColumnEncoding::CfTime(_) => parse_timestamp_micros(text)
            .ok_or_else(|| format!("bounds for time dimension '{dim}': cannot parse '{text}' as a timestamp (use e.g. '2022-07-01 12:00:00')"))?
            as f64,
        _ => text
            .parse::<f64>()
            .map_err(|_| format!("bounds for dimension '{dim}': '{text}' is not a number"))?,
    };
    if value.is_nan() {
        return Err(format!("bounds for dimension '{dim}': NaN is not a valid bound").into());
    }
    if matches!(encoding, ColumnEncoding::CfTime(_)) && value.abs() > EXACT_F64 {
        return Err(format!("bounds for time dimension '{dim}': '{text}' is out of range").into());
    }
    Ok(value)
}

/// `YYYY-MM-DD[( |T)HH:MM[:SS[.ffffff]]]` (optionally ending in `Z`) → microseconds since the epoch.
fn parse_timestamp_micros(text: &str) -> Option<i64> {
    let text = text.strip_suffix('Z').unwrap_or(text);
    let (date, time) = match text.find([' ', 'T']) {
        Some(i) => (&text[..i], Some(&text[i + 1..])),
        None => (text, None),
    };
    let mut d = date.split('-');
    let year: i32 = d.next()?.parse().ok()?;
    let month: u32 = d.next()?.parse().ok()?;
    let day: u32 = d.next()?.parse().ok()?;
    if d.next().is_some() {
        return None;
    }
    let (mut hour, mut minute, mut second, mut micro) = (0u32, 0u32, 0u32, 0u32);
    if let Some(time) = time {
        let (hms, frac) = match time.split_once('.') {
            Some((h, f)) => (h, Some(f)),
            None => (time, None),
        };
        let mut t = hms.split(':');
        hour = t.next()?.parse().ok()?;
        minute = t.next()?.parse().ok()?;
        if let Some(s) = t.next() {
            second = s.parse().ok()?;
        }
        if t.next().is_some() {
            return None;
        }
        if let Some(f) = frac {
            if f.is_empty() || f.len() > 6 || !f.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            micro = format!("{f:0<6}").parse().ok()?;
        }
    }
    let dt = NaiveDate::from_ymd_opt(year, month, day)?
        .and_hms_micro_opt(hour, minute, second, micro)?;
    Some(dt.and_utc().timestamp_micros())
}

/// Per-index keep mask for one coordinate array and an inclusive `[lo, hi]` range.
/// NULL coordinates and non-numeric (string) coordinates are never kept.
pub fn index_mask(coord: &CoordArray, len: usize, lo: Option<f64>, hi: Option<f64>) -> Vec<bool> {
    (0..len)
        .map(|i| match coord.data.key_at(i) {
            Some(k) => lo.is_none_or(|lo| k >= lo) && hi.is_none_or(|hi| k <= hi),
            None => false,
        })
        .collect()
}

/// Chunk indices along one dimension whose span contains at least one kept index.
pub fn chunks_with_kept(mask: &[bool], shape: u64, chunk_size: u64) -> Vec<u64> {
    (0..shape.div_ceil(chunk_size))
        .filter(|&c| {
            let start = (c * chunk_size) as usize;
            let end = (((c + 1) * chunk_size).min(shape) as usize).min(mask.len());
            start < end && mask[start..end].iter().any(|&k| k)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zarr_reader::types::{FixedValues, ZarrDtype};
    use std::sync::Arc;

    fn f64_coord(values: &[f64]) -> CoordArray {
        let bytes: Vec<u8> = values.iter().flat_map(|v| v.to_ne_bytes()).collect();
        CoordArray {
            dtype: ZarrDtype::Float64,
            encoding: ColumnEncoding::Plain,
            sentinel: None,
            data: Arc::new(
                FixedValues::new(bytes, ZarrDtype::Float64, ColumnEncoding::Plain, None).unwrap(),
            ),
        }
    }

    fn kept(mask: &[bool]) -> Vec<usize> {
        mask.iter()
            .enumerate()
            .filter_map(|(i, &k)| k.then_some(i))
            .collect()
    }

    #[test]
    fn inclusive_bounds_on_ascending_axis() {
        let c = f64_coord(&[0.0, 10.0, 20.0, 30.0, 40.0]);
        assert_eq!(kept(&index_mask(&c, 5, Some(10.0), Some(30.0))), [1, 2, 3]);
        // Exactly on a value keeps it once.
        assert_eq!(kept(&index_mask(&c, 5, Some(20.0), Some(20.0))), [2]);
        // Between values keeps nothing.
        assert!(kept(&index_mask(&c, 5, Some(11.0), Some(19.0))).is_empty());
        // One-sided.
        assert_eq!(kept(&index_mask(&c, 5, Some(30.0), None)), [3, 4]);
        assert_eq!(kept(&index_mask(&c, 5, None, Some(10.0))), [0, 1]);
    }

    #[test]
    fn descending_axis() {
        // ERA5-style latitude: 90 → -90.
        let c = f64_coord(&[90.0, 45.0, 0.0, -45.0, -90.0]);
        assert_eq!(kept(&index_mask(&c, 5, Some(-45.0), Some(45.0))), [1, 2, 3]);
    }

    #[test]
    fn non_uniform_axis() {
        let c = f64_coord(&[1000.0, 850.0, 500.0, 200.0, 10.0]);
        assert_eq!(
            kept(&index_mask(&c, 5, Some(200.0), Some(850.0))),
            [1, 2, 3]
        );
    }

    #[test]
    fn empty_and_inverted_ranges() {
        let c = f64_coord(&[-90.0, 0.0, 90.0]);
        assert!(kept(&index_mask(&c, 3, Some(100.0), None)).is_empty());
        assert!(kept(&index_mask(&c, 3, Some(10.0), Some(-10.0))).is_empty());
    }

    #[test]
    fn chunk_selection_with_boundaries_and_ragged_tail() {
        // 7 indices, chunks of 3: [0..3) [3..6) [6..7).
        assert_eq!(chunks_with_kept(&[false; 7], 7, 3), Vec::<u64>::new());
        let only = |i: usize| {
            let mut m = [false; 7];
            m[i] = true;
            m
        };
        // The seam: index 2 is chunk 0, index 3 is chunk 1; each belongs to exactly one.
        assert_eq!(chunks_with_kept(&only(2), 7, 3), [0]);
        assert_eq!(chunks_with_kept(&only(3), 7, 3), [1]);
        assert_eq!(chunks_with_kept(&only(6), 7, 3), [2]);
        // A range spanning a seam touches both.
        let mut m = [false; 7];
        m[2] = true;
        m[3] = true;
        assert_eq!(chunks_with_kept(&m, 7, 3), [0, 1]);
    }

    #[test]
    fn timestamp_parsing() {
        assert_eq!(parse_timestamp_micros("1970-01-01"), Some(0));
        assert_eq!(
            parse_timestamp_micros("1970-01-01 00:00:01"),
            Some(1_000_000)
        );
        assert_eq!(parse_timestamp_micros("1970-01-01T00:01"), Some(60_000_000));
        assert_eq!(
            parse_timestamp_micros("1970-01-01 00:00:00.5"),
            Some(500_000)
        );
        assert_eq!(parse_timestamp_micros("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_timestamp_micros("1969-12-31 23:59:59"),
            Some(-1_000_000)
        );
        assert_eq!(parse_timestamp_micros("2022-13-01"), None);
        assert_eq!(parse_timestamp_micros("yesterday"), None);
        assert_eq!(parse_timestamp_micros("2022-07-01 25:00"), None);
    }

    #[test]
    fn numeric_endpoint_errors() {
        let plain = ColumnEncoding::Plain;
        assert_eq!(parse_endpoint(" 12.5 ", &plain, "lat").unwrap(), 12.5);
        assert!(parse_endpoint("abc", &plain, "lat").is_err());
        assert!(parse_endpoint("NaN", &plain, "lat").is_err());
    }
}
