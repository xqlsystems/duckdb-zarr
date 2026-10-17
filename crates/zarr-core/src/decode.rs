//! Element decoding shared by every engine adapter: raw native-endian bytes
//! plus the bind-time CF decoding → a comparable numeric key.
//! zarrs normalises chunk bytes to the host's byte order, so `from_ne_bytes`
//! is always correct here.

use crate::types::{ColumnEncoding, FillSentinel, ZarrDtype};

/// Read any integer dtype from raw bytes at `start` as i64 (for packed-int decoding).
pub fn read_int_as_i64_pub(bytes: &[u8], dtype: &ZarrDtype, start: usize) -> i64 {
    match dtype {
        ZarrDtype::Int8 => bytes[start] as i8 as i64,
        ZarrDtype::Int16 => i16::from_ne_bytes(bytes[start..start + 2].try_into().unwrap()) as i64,
        ZarrDtype::Int32 => i32::from_ne_bytes(bytes[start..start + 4].try_into().unwrap()) as i64,
        ZarrDtype::Int64 => i64::from_ne_bytes(bytes[start..start + 8].try_into().unwrap()),
        ZarrDtype::UInt8 => bytes[start] as i64,
        ZarrDtype::UInt16 => u16::from_ne_bytes(bytes[start..start + 2].try_into().unwrap()) as i64,
        ZarrDtype::UInt32 => u32::from_ne_bytes(bytes[start..start + 4].try_into().unwrap()) as i64,
        // Bit-preserving reinterpret, not a numeric conversion: values above i64::MAX
        // land negative here. Callers that do arithmetic on the result (fill_element)
        // must special-case dtype == UInt64 and recover the true magnitude via
        // `raw as u64` before using it — this cast alone would silently flip the sign.
        ZarrDtype::UInt64 => u64::from_ne_bytes(bytes[start..start + 8].try_into().unwrap()) as i64,
        _ => 0,
    }
}

/// Read any numeric dtype from raw bytes at `start` as f64 (for CF-time decoding).
/// Integers widen losslessly up to 2^53; beyond that the caller should use the
/// integer path, which CF-time decoding does for every integer dtype.
pub fn read_as_f64_pub(bytes: &[u8], dtype: &ZarrDtype, start: usize) -> f64 {
    match dtype {
        ZarrDtype::Float32 => {
            f32::from_ne_bytes(bytes[start..start + 4].try_into().unwrap()) as f64
        }
        ZarrDtype::Float64 => f64::from_ne_bytes(bytes[start..start + 8].try_into().unwrap()),
        _ => read_int_as_i64_pub(bytes, dtype, start) as f64,
    }
}

/// Whether a raw integer value equals the column's active NULL sentinel.
pub fn matches_int_sentinel(raw: i64, sentinel: &Option<FillSentinel>) -> bool {
    match sentinel {
        Some(FillSentinel::Int(v)) => raw == *v,
        Some(FillSentinel::UInt(v)) => (raw as u64) == *v,
        _ => false,
    }
}

/// Whether a raw float value equals the column's active NULL sentinel (CF §2.5.1:
/// exact equality, with `NaN` compared by `is_nan`).
pub fn matches_float_sentinel(raw: f64, sentinel: &Option<FillSentinel>) -> bool {
    match sentinel {
        Some(FillSentinel::Float(v)) => {
            if v.is_nan() {
                raw.is_nan()
            } else {
                raw == *v
            }
        }
        _ => false,
    }
}

/// Comparable numeric value of one element after the same decoding
/// the adapters' writers applies, or `None` where an adapter would write NULL.
pub fn element_key(
    bytes: &[u8],
    dtype: &ZarrDtype,
    encoding: &ColumnEncoding,
    sentinel: &Option<FillSentinel>,
    idx: usize,
    elem_size: usize,
) -> Option<f64> {
    let size = dtype.byte_size()?;
    if elem_size != size {
        return None;
    }
    let src = idx.checked_mul(elem_size)?;
    bytes.get(src..src.checked_add(size)?)?;
    if let ColumnEncoding::Categorical(categories) = encoding {
        if !dtype.is_integer() {
            return None;
        }
        let raw = read_int_as_i64_pub(bytes, dtype, src);
        let code = if dtype.is_unsigned() {
            usize::try_from(raw as u64).ok()?
        } else {
            usize::try_from(raw).ok()?
        };
        return (code < categories.len())
            .then(|| categories.values.data.key_at(code))
            .flatten();
    }
    if *dtype == ZarrDtype::Bool {
        return Some((bytes[src] != 0) as u8 as f64);
    }
    match encoding {
        ColumnEncoding::Categorical(_) => unreachable!("handled above"),
        ColumnEncoding::Plain | ColumnEncoding::PackedInt { .. } => {
            let numeric = if dtype.is_integer() {
                let raw = read_int_as_i64_pub(bytes, dtype, src);
                if matches_int_sentinel(raw, sentinel) {
                    return None;
                }
                if *dtype == ZarrDtype::UInt64 {
                    raw as u64 as f64
                } else {
                    raw as f64
                }
            } else {
                let raw = read_as_f64_pub(bytes, dtype, src);
                if matches_float_sentinel(raw, sentinel) || raw.is_nan() {
                    return None;
                }
                raw
            };
            match encoding {
                ColumnEncoding::PackedInt {
                    scale_factor,
                    add_offset,
                } => Some(numeric * scale_factor + add_offset),
                _ => Some(numeric),
            }
        }
        ColumnEncoding::CfTime(cf) => {
            let decoded = if dtype.is_integer() {
                let raw = read_int_as_i64_pub(bytes, dtype, src);
                let overflowed = *dtype == ZarrDtype::UInt64 && raw < 0;
                (!overflowed && !matches_int_sentinel(raw, sentinel))
                    .then(|| cf.decode_int(raw))
                    .flatten()
            } else {
                let raw = read_as_f64_pub(bytes, dtype, src);
                (!matches_float_sentinel(raw, sentinel))
                    .then(|| cf.decode_float(raw))
                    .flatten()
            };
            decoded.map(|micros| micros as f64)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn categorical_keys_are_category_values_not_codes() {
        use crate::types::{Categories, ColumnValues, CoordArray, FixedValues, StringValues};
        use std::sync::Arc;
        let values = CoordArray {
            dtype: ZarrDtype::Float64,
            encoding: ColumnEncoding::PackedInt {
                scale_factor: 2.0,
                add_offset: 1.0,
            },
            sentinel: None,
            data: Arc::new(
                FixedValues::new(
                    [10f64, 20.0]
                        .into_iter()
                        .flat_map(f64::to_ne_bytes)
                        .collect(),
                    ZarrDtype::Float64,
                    ColumnEncoding::PackedInt {
                        scale_factor: 2.0,
                        add_offset: 1.0,
                    },
                    None,
                )
                .unwrap(),
            ),
        };
        // Even inconsistent declared category lengths must not index past values.
        let encoding = ColumnEncoding::Categorical(Arc::new(Categories::new(values, 3, None)));
        let codes = FixedValues::new(
            vec![1, 0, 255, 2, 3],
            ZarrDtype::Int8,
            encoding.clone(),
            None,
        )
        .unwrap();
        assert_eq!(
            (0..6).map(|i| codes.key_at(i)).collect::<Vec<_>>(),
            vec![Some(41.0), Some(21.0), None, None, None, None]
        );
        let unsigned = FixedValues::new(
            u64::MAX.to_ne_bytes().to_vec(),
            ZarrDtype::UInt64,
            encoding,
            None,
        )
        .unwrap();
        assert_eq!(unsigned.key_at(0), None);
        let strings = CoordArray {
            dtype: ZarrDtype::String,
            encoding: ColumnEncoding::Plain,
            sentinel: None,
            data: Arc::new(StringValues {
                strings: vec!["a".into()],
            }),
        };
        let encoding = ColumnEncoding::Categorical(Arc::new(Categories::new(
            strings,
            1,
            Some(vec!["a".into()]),
        )));
        let codes = FixedValues::new(vec![0], ZarrDtype::Int8, encoding.clone(), None).unwrap();
        assert_eq!(codes.key_at(0), None);
        assert_eq!(
            element_key(&[0], &ZarrDtype::Bool, &encoding, &None, 0, 1),
            None
        );
    }

    #[test]
    fn key_lookup_rejects_out_of_range_and_malformed_buffers() {
        for (bytes, idx, size) in [
            (&[1u8][..], 1, 1),
            (&[1u8][..], usize::MAX, 8),
            (&[1u8][..], 0, 1),
        ] {
            assert_eq!(
                element_key(
                    bytes,
                    &ZarrDtype::Int64,
                    &ColumnEncoding::Plain,
                    &None,
                    idx,
                    size
                ),
                None
            );
        }
        assert_eq!(
            element_key(&[], &ZarrDtype::String, &ColumnEncoding::Plain, &None, 0, 0),
            None
        );
    }
}
