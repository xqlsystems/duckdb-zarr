use duckdb::core::FlatVector;

use zarr_core::decode::{
    matches_float_sentinel, matches_int_sentinel, read_as_f64_pub, read_int_as_i64_pub,
};

use super::types::{ColumnEncoding, FillSentinel, WriteElement, ZarrDtype};

/// Copy one scalar element from raw native-endian bytes into a DuckDB vector slot,
/// applying NULL masking via the sentinel.
/// zarrs normalises all chunk bytes to the host's native byte order before returning
/// them, so from_ne_bytes is always correct here.
pub fn fill_scalar_element_pub(
    vector: &mut FlatVector<'_>,
    bytes: &[u8],
    dtype: &ZarrDtype,
    sentinel: &Option<FillSentinel>,
    src_idx: usize,
    _elem_size: usize,
    dst_idx: usize,
) {
    match dtype {
        ZarrDtype::Bool => {
            let val = bytes[src_idx] != 0;
            unsafe {
                let slot = vector.as_mut_ptr::<bool>();
                *slot.add(dst_idx) = val;
            }
        }
        ZarrDtype::Int8 => copy_scalar!(vector, bytes, i8, src_idx, dst_idx, sentinel),
        ZarrDtype::Int16 => copy_scalar!(vector, bytes, i16, src_idx, dst_idx, sentinel),
        ZarrDtype::Int32 => copy_scalar!(vector, bytes, i32, src_idx, dst_idx, sentinel),
        ZarrDtype::Int64 => copy_scalar!(vector, bytes, i64, src_idx, dst_idx, sentinel),
        ZarrDtype::UInt8 => copy_scalar!(vector, bytes, u8, src_idx, dst_idx, sentinel),
        ZarrDtype::UInt16 => copy_scalar!(vector, bytes, u16, src_idx, dst_idx, sentinel),
        ZarrDtype::UInt32 => copy_scalar!(vector, bytes, u32, src_idx, dst_idx, sentinel),
        ZarrDtype::UInt64 => copy_scalar!(vector, bytes, u64, src_idx, dst_idx, sentinel),
        ZarrDtype::Float32 => copy_scalar!(vector, bytes, f32, src_idx, dst_idx, sentinel),
        ZarrDtype::Float64 => copy_scalar!(vector, bytes, f64, src_idx, dst_idx, sentinel),
        ZarrDtype::String => {
            unreachable!("String is variable-length; use fill_string_element_pub instead")
        }
    }
}

/// Copy one decoded string element into a DuckDB VARCHAR vector slot.
pub fn fill_string_element_pub(
    vector: &mut FlatVector<'_>,
    strings: &[String],
    src_idx: usize,
    dst_idx: usize,
) {
    use duckdb::core::Inserter;
    vector.insert(dst_idx, strings[src_idx].as_str());
}

// ── Null-check helpers ────────────────────────────────────────────────────────

macro_rules! copy_scalar {
    ($vector:expr, $bytes:expr, $T:ty, $src_idx:expr, $dst_idx:expr, $sentinel:expr) => {{
        let elem_size = std::mem::size_of::<$T>();
        let start = $src_idx * elem_size;
        let arr: [u8; std::mem::size_of::<$T>()] =
            $bytes[start..start + elem_size].try_into().unwrap();
        let val = <$T>::from_ne_bytes(arr);
        if is_fill(val, $sentinel) {
            $vector.set_null($dst_idx);
        } else {
            unsafe {
                let slot = $vector.as_mut_ptr::<$T>();
                *slot.add($dst_idx) = val;
            }
        }
    }};
}

use copy_scalar;

fn is_fill<T: NullCheck>(val: T, sentinel: &Option<FillSentinel>) -> bool {
    val.check_fill(sentinel)
}

trait NullCheck: Copy {
    fn check_fill(self, sentinel: &Option<FillSentinel>) -> bool;
}

macro_rules! impl_null_check_int {
    ($T:ty, $Variant:ident, $cast:ty) => {
        impl NullCheck for $T {
            fn check_fill(self, s: &Option<FillSentinel>) -> bool {
                matches!(s, Some(FillSentinel::$Variant(v)) if *v == self as $cast)
            }
        }
    };
}

impl_null_check_int!(i8, Int, i64);
impl_null_check_int!(i16, Int, i64);
impl_null_check_int!(i32, Int, i64);
impl_null_check_int!(i64, Int, i64);
impl_null_check_int!(u8, UInt, u64);
impl_null_check_int!(u16, UInt, u64);
impl_null_check_int!(u32, UInt, u64);
impl_null_check_int!(u64, UInt, u64);

impl NullCheck for f32 {
    fn check_fill(self, s: &Option<FillSentinel>) -> bool {
        match s {
            // CF §2.5.1: a datum is missing iff its value equals _FillValue exactly.
            // The sentinel is stored as f64; cast self to f64 for the comparison so the
            // widths match, then use exact equality — not an epsilon band, which would
            // incorrectly mask non-sentinel values near zero.
            Some(FillSentinel::Float(v)) => {
                if v.is_nan() {
                    self.is_nan()
                } else {
                    (self as f64) == *v
                }
            }
            _ => false,
        }
    }
}

impl NullCheck for f64 {
    fn check_fill(self, s: &Option<FillSentinel>) -> bool {
        match s {
            Some(FillSentinel::Float(v)) => {
                if v.is_nan() {
                    self.is_nan()
                } else {
                    self == *v
                }
            }
            _ => false,
        }
    }
}

/// Write one fixed-width element into the DuckDB output vector,
/// applying whatever CF decoding the column's bind-time encoding calls for.
#[allow(clippy::too_many_arguments)]
pub fn fill_element(
    vector: &mut FlatVector<'_>,
    bytes: &[u8],
    dtype: &ZarrDtype,
    encoding: &ColumnEncoding,
    sentinel: &Option<FillSentinel>,
    flat_row: usize,
    elem_size: usize,
    dst: usize,
) {
    match encoding {
        ColumnEncoding::Plain => {
            fill_scalar_element_pub(vector, bytes, dtype, sentinel, flat_row, elem_size, dst);
        }
        ColumnEncoding::Categorical(categories) => {
            let code = read_int_as_i64_pub(bytes, dtype, flat_row * elem_size);
            // A negative code is a missing value (pandas writes -1).
            if code < 0 || code as usize >= categories.len() {
                vector.set_null(dst);
                return;
            }
            let idx = code as usize;
            // An ENUM vector holds the member index, in the narrowest unsigned
            // type that fits the member count (DuckDB's EnumTypeInfo::DictType).
            if categories.enum_members().is_some() {
                let n = categories.len();
                unsafe {
                    if n <= u8::MAX as usize {
                        *vector.as_mut_ptr::<u8>().add(dst) = idx as u8;
                    } else if n <= u16::MAX as usize {
                        *vector.as_mut_ptr::<u16>().add(dst) = idx as u16;
                    } else {
                        *vector.as_mut_ptr::<u32>().add(dst) = idx as u32;
                    }
                }
                return;
            }
            categories.values.data.write_element(vector, idx, dst);
        }
        ColumnEncoding::PackedInt {
            scale_factor,
            add_offset,
        } => {
            let src = flat_row * elem_size;
            let raw = read_int_as_i64_pub(bytes, dtype, src);
            // CF §8.1: mask on the raw integer, before scaling shifts the sentinel.
            if matches_int_sentinel(raw, sentinel) {
                vector.set_null(dst);
            } else {
                // read_int_as_i64_pub bit-reinterprets UInt64, so a value above
                // i64::MAX comes back negative here; recover the true magnitude
                // before scaling instead of scaling a sign-flipped one.
                let numeric = if *dtype == ZarrDtype::UInt64 {
                    raw as u64 as f64
                } else {
                    raw as f64
                };
                unsafe {
                    let slot = vector.as_mut_ptr::<f64>();
                    *slot.add(dst) = numeric * scale_factor + add_offset;
                }
            }
        }
        ColumnEncoding::CfTime(cf) => {
            let src = flat_row * elem_size;
            // Integers go through the exact i128 path; only float offsets need f64.
            // Either way an unrepresentable instant (NaN, or past DuckDB's range)
            // becomes NULL rather than a bogus date.
            let decoded = if dtype.is_integer() {
                let raw = read_int_as_i64_pub(bytes, dtype, src);
                // A UInt64 raw above i64::MAX comes back negative from the same
                // bit-reinterpret — no legitimate CF-time offset is that large
                // anyway (see out_of_range_values_decode_to_none), so treat it as
                // out-of-range rather than decoding a bogus negative-offset date.
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
            match decoded {
                // DuckDB TIMESTAMP is physically an i64 of microseconds since epoch.
                Some(micros) => unsafe {
                    let slot = vector.as_mut_ptr::<i64>();
                    *slot.add(dst) = micros;
                },
                None => vector.set_null(dst),
            }
        }
    }
}
