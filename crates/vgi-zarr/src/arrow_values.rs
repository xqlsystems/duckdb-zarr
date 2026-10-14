//! Convert decoded Zarr values to Arrow without using floating-point pruning keys.

use std::sync::Arc;

use arrow_array::*;
use arrow_schema::{DataType, TimeUnit};
use vgi_rpc::{Result, RpcError};
use zarr_core::decode::{
    matches_float_sentinel, matches_int_sentinel, read_as_f64_pub, read_int_as_i64_pub,
};
use zarr_core::types::{ColumnDef, ColumnEncoding, ColumnValues, ZarrDtype};

pub fn data_type(col: &ColumnDef) -> DataType {
    match &col.encoding {
        ColumnEncoding::CfTime(_) => DataType::Timestamp(TimeUnit::Microsecond, None),
        ColumnEncoding::PackedInt { .. } => DataType::Float64,
        ColumnEncoding::Plain => match col.on_disk_dtype {
            ZarrDtype::Bool => DataType::Boolean,
            ZarrDtype::Int8 => DataType::Int8,
            ZarrDtype::Int16 => DataType::Int16,
            ZarrDtype::Int32 => DataType::Int32,
            ZarrDtype::Int64 => DataType::Int64,
            ZarrDtype::UInt8 => DataType::UInt8,
            ZarrDtype::UInt16 => DataType::UInt16,
            ZarrDtype::UInt32 => DataType::UInt32,
            ZarrDtype::UInt64 => DataType::UInt64,
            ZarrDtype::Float32 => DataType::Float32,
            ZarrDtype::Float64 => DataType::Float64,
            ZarrDtype::String => DataType::Utf8,
        },
    }
}

pub fn to_array(
    values: &dyn ColumnValues,
    indices: impl Iterator<Item = usize>,
) -> Result<ArrayRef> {
    if let Some(values) = values.as_strings() {
        return Ok(Arc::new(StringArray::from_iter_values(
            indices.map(|i| values.strings[i].as_str()),
        )));
    }
    let v = values
        .as_fixed()
        .ok_or_else(|| RpcError::type_error("unsupported Zarr column values"))?;
    let integer = |i| {
        let raw = read_int_as_i64_pub(&v.bytes, &v.dtype, i * v.elem_size());
        (!matches_int_sentinel(raw, &v.sentinel)).then_some(raw)
    };
    let float = |i| {
        let raw = read_as_f64_pub(&v.bytes, &v.dtype, i * v.elem_size());
        (!matches_float_sentinel(raw, &v.sentinel)).then_some(raw)
    };
    match &v.encoding {
        ColumnEncoding::CfTime(cf) => Ok(Arc::new(TimestampMicrosecondArray::from_iter(
            indices.map(|i| {
                if v.dtype.is_integer() {
                    integer(i).and_then(|raw| {
                        // UInt64 offsets above i64::MAX cannot be represented as timestamps.
                        if v.dtype == ZarrDtype::UInt64 && raw < 0 {
                            None
                        } else {
                            cf.decode_int(raw)
                        }
                    })
                } else {
                    float(i).and_then(|raw| cf.decode_float(raw))
                }
            }),
        ))),
        ColumnEncoding::PackedInt {
            scale_factor,
            add_offset,
        } => Ok(Arc::new(Float64Array::from_iter(indices.map(|i| {
            integer(i).map(|raw| {
                let number = if v.dtype == ZarrDtype::UInt64 {
                    raw as u64 as f64
                } else {
                    raw as f64
                };
                number * scale_factor + add_offset
            })
        })))),
        ColumnEncoding::Plain => {
            macro_rules! integers {
                ($array:ty, $native:ty) => {
                    Arc::new(<$array>::from_iter(
                        indices.map(|i| integer(i).map(|v| v as $native)),
                    ))
                };
            }
            Ok(match v.dtype {
                ZarrDtype::Bool => Arc::new(BooleanArray::from_iter(
                    indices.map(|i| Some(v.bytes[i] != 0)),
                )),
                ZarrDtype::Int8 => integers!(Int8Array, i8),
                ZarrDtype::Int16 => integers!(Int16Array, i16),
                ZarrDtype::Int32 => integers!(Int32Array, i32),
                ZarrDtype::Int64 => integers!(Int64Array, i64),
                ZarrDtype::UInt8 => integers!(UInt8Array, u8),
                ZarrDtype::UInt16 => integers!(UInt16Array, u16),
                ZarrDtype::UInt32 => integers!(UInt32Array, u32),
                ZarrDtype::UInt64 => integers!(UInt64Array, u64),
                ZarrDtype::Float32 => Arc::new(Float32Array::from_iter(
                    indices.map(|i| float(i).map(|v| v as f32)),
                )),
                ZarrDtype::Float64 => Arc::new(Float64Array::from_iter(indices.map(float))),
                ZarrDtype::String => return Err(RpcError::type_error("fixed-width string values")),
            })
        }
    }
}
