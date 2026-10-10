use std::collections::HashMap;
use std::sync::Arc;

use duckdb::core::{FlatVector, LogicalTypeHandle, LogicalTypeId};

use super::cftime::CfTimeEncoding;

/// On-disk Zarr dtype as reported by zarrs `DataType::to_string()`.
#[derive(Debug, Clone, PartialEq)]
pub enum ZarrDtype {
    Bool,
    Int8,
    Int16,
    Int32,
    Int64,
    UInt8,
    UInt16,
    UInt32,
    UInt64,
    Float32,
    Float64,
    String,
}

impl ZarrDtype {
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "bool" => Some(Self::Bool),
            "int8" => Some(Self::Int8),
            "int16" => Some(Self::Int16),
            "int32" => Some(Self::Int32),
            "int64" => Some(Self::Int64),
            "uint8" => Some(Self::UInt8),
            "uint16" => Some(Self::UInt16),
            "uint32" => Some(Self::UInt32),
            "uint64" => Some(Self::UInt64),
            "float32" | "float" => Some(Self::Float32),
            "float64" | "double" => Some(Self::Float64),
            "string" => Some(Self::String),
            _ => None,
        }
    }

    pub fn is_integer(&self) -> bool {
        matches!(
            self,
            Self::Int8
                | Self::Int16
                | Self::Int32
                | Self::Int64
                | Self::UInt8
                | Self::UInt16
                | Self::UInt32
                | Self::UInt64
        )
    }

    pub fn is_unsigned(&self) -> bool {
        matches!(
            self,
            Self::UInt8 | Self::UInt16 | Self::UInt32 | Self::UInt64
        )
    }

    /// `None` for [`Self::String`], which is variable-length and has no fixed byte size.
    pub fn byte_size(&self) -> Option<usize> {
        match self {
            Self::Bool | Self::Int8 | Self::UInt8 => Some(1),
            Self::Int16 | Self::UInt16 => Some(2),
            Self::Int32 | Self::UInt32 | Self::Float32 => Some(4),
            Self::Int64 | Self::UInt64 | Self::Float64 => Some(8),
            Self::String => None,
        }
    }

    /// DuckDB output type for this column, accounting for packed-int decoding.
    pub fn to_duckdb_type(&self, encoding: &ColumnEncoding) -> LogicalTypeHandle {
        match encoding {
            ColumnEncoding::Categorical(categories) => match categories.enum_members() {
                Some(members) => enum_type(members),
                None => categories
                    .values
                    .dtype
                    .to_duckdb_type(&categories.values.encoding),
            },
            ColumnEncoding::PackedInt { .. } => LogicalTypeId::Double.into(),
            // Microseconds since the Unix epoch — DuckDB TIMESTAMP's physical layout.
            ColumnEncoding::CfTime(_) => LogicalTypeId::Timestamp.into(),
            ColumnEncoding::Plain => match self {
                Self::Bool => LogicalTypeId::Boolean.into(),
                Self::Int8 => LogicalTypeId::Tinyint.into(),
                Self::Int16 => LogicalTypeId::Smallint.into(),
                Self::Int32 => LogicalTypeId::Integer.into(),
                Self::Int64 => LogicalTypeId::Bigint.into(),
                Self::UInt8 => LogicalTypeId::UTinyint.into(),
                Self::UInt16 => LogicalTypeId::USmallint.into(),
                Self::UInt32 => LogicalTypeId::UInteger.into(),
                Self::UInt64 => LogicalTypeId::UBigint.into(),
                Self::Float32 => LogicalTypeId::Float.into(),
                Self::Float64 => LogicalTypeId::Double.into(),
                Self::String => LogicalTypeId::Varchar.into(),
            },
        }
    }
}

/// How an on-disk array column is decoded into DuckDB output values.
#[derive(Debug, Clone)]
pub enum ColumnEncoding {
    Plain,
    /// AnnData `categorical`: the array holds integer codes into `categories`;
    /// a negative code is a missing value.
    Categorical(Arc<Categories>),
    PackedInt {
        scale_factor: f64,
        add_offset: f64,
    },
    /// CF-encoded time (`units = "<step> since <reference>"`) → `TIMESTAMP`.
    CfTime(CfTimeEncoding),
}

/// The categories of an AnnData `categorical`.
#[derive(Debug)]
pub struct Categories {
    /// The categories, written element by element for a non-`ENUM` column.
    pub values: CoordArray,
    len: usize,
    /// The `ENUM` members when the column reads as a DuckDB `ENUM`: the
    /// categories are strings, distinct, and free of NUL bytes. Other
    /// categories (integers, say) read as their own type, because an `ENUM`
    /// holds strings only.
    enum_members: Option<Vec<String>>,
}

impl Categories {
    /// `strings` are the categories when they are strings.
    pub fn new(values: CoordArray, len: usize, strings: Option<Vec<String>>) -> Self {
        let enum_members = strings.filter(|s| {
            let distinct: std::collections::HashSet<&String> = s.iter().collect();
            !s.is_empty()
                && distinct.len() == s.len()
                && u32::try_from(s.len()).is_ok()
                && s.iter().all(|m| !m.contains('\0'))
        });
        Self {
            values,
            len,
            enum_members,
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    /// The `ENUM` members, in code order, if the column reads as an `ENUM`.
    pub fn enum_members(&self) -> Option<&[String]> {
        self.enum_members.as_deref()
    }
}

/// A DuckDB `ENUM` type with `members` in order, so that member `i` is code `i`.
fn enum_type(members: &[String]) -> LogicalTypeHandle {
    use duckdb::ffi::{duckdb_create_enum_type, duckdb_logical_type, idx_t};
    // duckdb-rs has no public ENUM constructor. LogicalTypeHandle is one
    // `duckdb_logical_type` field and destroys it on drop, so a handle made
    // here is owned and freed like any other. The assert catches a future
    // duckdb-rs that changes the layout.
    const _: () = assert!(
        std::mem::size_of::<LogicalTypeHandle>() == std::mem::size_of::<duckdb_logical_type>()
    );
    let names: Vec<std::ffi::CString> = members
        .iter()
        .map(|m| std::ffi::CString::new(m.as_str()).expect("checked for NUL in Categories::new"))
        .collect();
    let mut ptrs: Vec<*const std::os::raw::c_char> = names.iter().map(|n| n.as_ptr()).collect();
    unsafe {
        let raw = duckdb_create_enum_type(ptrs.as_mut_ptr(), ptrs.len() as idx_t);
        std::mem::transmute::<duckdb_logical_type, LogicalTypeHandle>(raw)
    }
}

/// Parsed NULL-masking sentinel from CF attrs (`_FillValue` or `missing_value`).
#[derive(Debug, Clone)]
pub enum FillSentinel {
    /// Covers float32/float64 on-disk types. `f64::NAN` means check `is_nan()`.
    Float(f64),
    /// Covers signed integer on-disk types.
    Int(i64),
    /// Covers unsigned integer on-disk types.
    UInt(u64),
}

/// Describes one output column (either a dim coord or a data variable).
#[derive(Debug, Clone)]
pub struct ColumnDef {
    /// The variable's store-relative path. For an AnnData-encoded variable this
    /// is the group's path (`obs/cell_type`), not the path of the array that
    /// holds its values.
    pub name: String,
    /// The array that holds the values (or codes), when it differs from `name`.
    pub source: Option<String>,
    /// An array of the same shape and chunks whose `true` entries are missing
    /// values (AnnData `nullable-*`).
    pub mask: Option<String>,
    /// For a sparse variable read into a dense table: how to scatter it.
    pub sparse: Option<SparseMatrix>,
    pub on_disk_dtype: ZarrDtype,
    pub encoding: ColumnEncoding,
    pub sentinel: Option<FillSentinel>,
    pub is_coord: bool,
    /// For coord columns: the dimension index this coord maps to in group.dims.
    /// None for data variable columns.
    pub dim_idx: Option<usize>,
}

/// One dim group: arrays sharing an identical ordered dimension set.
#[derive(Debug, Clone)]
pub struct DimGroup {
    pub dims: Vec<String>,
    pub shape: Vec<u64>,
    pub chunk_shape: Vec<u64>,
    pub data_var_names: Vec<String>,
    /// `(dim, array path)` for each dimension that has a coordinate.
    pub coords: Vec<(String, String)>,
    /// Data variables stored as a group of arrays (AnnData encodings), keyed by
    /// path. Variables not listed here are plain arrays.
    pub encodings: HashMap<String, VarEncoding>,
    /// Arrays of this table that zarrs cannot open (unsupported data type or
    /// codec), with the error. `read_zarr` fails on such a table rather than
    /// return it without them.
    pub unreadable: Vec<(String, String)>,
}

impl DimGroup {
    /// Whether every data variable is a sparse matrix. Such a table has one row
    /// per stored entry instead of one per cell.
    pub fn is_sparse(&self) -> bool {
        !self.data_var_names.is_empty()
            && self
                .data_var_names
                .iter()
                .all(|name| matches!(self.encodings.get(name), Some(VarEncoding::Sparse(_))))
    }
}

/// A variable stored as a group of arrays, as AnnData writes them
/// (<https://anndata.readthedocs.io/en/stable/fileformat-prose.html>).
#[derive(Debug, Clone)]
pub enum VarEncoding {
    /// `categorical`: integer `codes` into a 1-D `categories` array.
    Categorical { codes: String, categories: String },
    /// `nullable-integer`, `nullable-boolean`, `nullable-string-array`:
    /// `values` plus a boolean `mask` that is `true` where a value is missing.
    Nullable { values: String, mask: String },
    /// `csr_matrix` / `csc_matrix`.
    Sparse(SparseMatrix),
}

/// A compressed sparse matrix: `data` and `indices` hold the stored entries,
/// and `indptr[i]..indptr[i + 1]` are the entries of row `i` (CSR) or column
/// `i` (CSC).
#[derive(Debug, Clone)]
pub struct SparseMatrix {
    /// 0 for CSR (`indptr` runs over rows), 1 for CSC (over columns).
    pub major_axis: usize,
    pub data: String,
    pub indices: String,
    pub indptr: String,
    pub shape: Vec<u64>,
}

/// Decoded element values for one array segment (strategy interface).
///
/// Each implementation owns its in-memory representation and knows how to
/// write one element into a DuckDB output vector, so callers never branch on
/// the dtype family.
pub trait ColumnValues: std::fmt::Debug + Send + Sync {
    /// Write the element at row-major index `src_idx` into slot `dst` of `vector`.
    fn write_element(&self, vector: &mut FlatVector<'_>, src_idx: usize, dst: usize);
}

/// Shared handle to decoded values; cheap to clone.
pub type SharedColumnValues = Arc<dyn ColumnValues>;

/// Fixed-width dtypes: one contiguous native-endian byte buffer plus the
/// bind-time decoding (CF packing / time, fill sentinel) applied per element.
#[derive(Debug)]
pub struct FixedValues {
    /// Row-major native-endian bytes, length = `n * dtype.byte_size()`.
    pub bytes: Vec<u8>,
    pub dtype: ZarrDtype,
    pub encoding: ColumnEncoding,
    pub sentinel: Option<FillSentinel>,
    elem_size: usize,
}

impl FixedValues {
    /// Returns `None` if `dtype` is not fixed-width.
    pub fn new(
        bytes: Vec<u8>,
        dtype: ZarrDtype,
        encoding: ColumnEncoding,
        sentinel: Option<FillSentinel>,
    ) -> Option<Self> {
        let elem_size = dtype.byte_size()?;
        Some(Self {
            bytes,
            dtype,
            encoding,
            sentinel,
            elem_size,
        })
    }
}

impl ColumnValues for FixedValues {
    fn write_element(&self, vector: &mut FlatVector<'_>, src_idx: usize, dst: usize) {
        crate::zarr_reader::scan::fill_element(
            vector,
            &self.bytes,
            &self.dtype,
            &self.encoding,
            &self.sentinel,
            src_idx,
            self.elem_size,
            dst,
        );
    }
}

/// Variable-length UTF-8 strings: one decoded `String` per element.
#[derive(Debug)]
pub struct StringValues {
    pub strings: Vec<String>,
}

impl ColumnValues for StringValues {
    fn write_element(&self, vector: &mut FlatVector<'_>, src_idx: usize, dst: usize) {
        crate::zarr_reader::scan::fill_string_element_pub(vector, &self.strings, src_idx, dst);
    }
}

/// Values with a mask: an element whose mask byte is nonzero is NULL (AnnData
/// `nullable-*`). `mask` has the same layout as the values.
#[derive(Debug)]
pub struct MaskedValues {
    pub values: SharedColumnValues,
    pub mask: Vec<u8>,
}

impl ColumnValues for MaskedValues {
    fn write_element(&self, vector: &mut FlatVector<'_>, src_idx: usize, dst: usize) {
        if self.mask[src_idx] != 0 {
            vector.set_null(dst);
        } else {
            self.values.write_element(vector, src_idx, dst);
        }
    }
}

/// A pre-loaded coordinate array (shape is 1-D: `[n]`).
#[derive(Debug, Clone)]
pub struct CoordArray {
    pub dtype: ZarrDtype,
    pub encoding: ColumnEncoding,
    pub sentinel: Option<FillSentinel>,
    pub data: SharedColumnValues,
}

/// One unit of parallel work: a chunk index tuple for all data variables.
#[derive(Debug, Clone)]
pub struct WorkUnit {
    pub chunk_indices: Vec<u64>,
}
