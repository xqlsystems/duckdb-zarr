use std::sync::Arc;

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
    #[allow(clippy::should_implement_trait)]
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
}

/// How an on-disk array column is decoded into output values.
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

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The `ENUM` members, in code order, if the column reads as an `ENUM`.
    pub fn enum_members(&self) -> Option<&[String]> {
        self.enum_members.as_deref()
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
    /// The variable's store-relative path, or the dimension's name for a
    /// dimension column.
    pub name: String,
    /// How the variable's values are stored ([`Storage::Array`] for a
    /// dimension column, whose values are pre-loaded).
    pub storage: Storage,
    pub on_disk_dtype: ZarrDtype,
    pub encoding: ColumnEncoding,
    pub sentinel: Option<FillSentinel>,
    pub is_coord: bool,
    /// For coord columns: the dimension index this coord maps to in the
    /// table's dims. None for data variable columns.
    pub dim_idx: Option<usize>,
}

/// A variable of a node, as xarray reads it: values over named dimensions.
#[derive(Debug, Clone)]
pub struct Variable {
    /// Store-relative path, for example `simulation/fine/foo`. For a variable
    /// stored as a group of arrays (AnnData), the group's path.
    pub path: String,
    /// The node whose tables hold the variable: the group that holds it,
    /// except for AnnData's `obs`, `var` and `raw/var` columns, which belong to
    /// the parent group (see [`super::anndata::logical_node`]).
    pub node: String,
    /// Dimension names the store records or the AnnData layout gives, or
    /// `None` when there are none.
    pub dims: Option<Vec<String>>,
    pub shape: Vec<u64>,
    /// The chunk shape of the array that holds the values. A sparse matrix's
    /// is its shape.
    pub chunk_shape: Vec<u64>,
    pub attrs: serde_json::Map<String, serde_json::Value>,
    pub storage: Storage,
    /// Whether the variable is a column of an AnnData data frame. The frame's
    /// index is the coordinate of its axis, so no column is one, even a column
    /// named like the axis.
    pub in_dataframe: bool,
}

/// How a variable's values are stored: one Zarr array, or a group of arrays
/// as AnnData writes them
/// (<https://anndata.readthedocs.io/en/stable/fileformat-prose.html>).
#[derive(Debug, Clone)]
pub enum Storage {
    /// One Zarr array at the variable's path.
    Array,
    /// One Zarr array that zarrs cannot open (unsupported data type or codec),
    /// with the error. Its shape and names come from its raw metadata document,
    /// so that its table can say it is missing instead of leaving it out.
    Unreadable { error: String },
    /// `categorical`: integer `codes` into a 1-D `categories` array.
    Categorical { codes: String, categories: String },
    /// `nullable-integer`, `nullable-boolean`, `nullable-string-array`:
    /// `values` plus a boolean `mask` that is `true` where a value is missing.
    Nullable { values: String, mask: String },
    /// `csr_matrix` / `csc_matrix`.
    Sparse(SparseMatrix),
}

impl Storage {
    /// The array the scan reads chunk by chunk for a variable at `path`: the
    /// array itself, a categorical's codes or a nullable column's values.
    /// `None` for a sparse matrix, which is read through its own arrays, and
    /// for an unreadable array.
    pub fn values_array<'a>(&'a self, path: &'a str) -> Option<&'a str> {
        match self {
            Storage::Array => Some(path),
            Storage::Categorical { codes, .. } => Some(codes),
            Storage::Nullable { values, .. } => Some(values),
            Storage::Unreadable { .. } | Storage::Sparse(_) => None,
        }
    }

    /// A nullable column's mask.
    pub fn mask(&self) -> Option<&str> {
        match self {
            Storage::Nullable { mask, .. } => Some(mask),
            _ => None,
        }
    }

    pub fn sparse(&self) -> Option<&SparseMatrix> {
        match self {
            Storage::Sparse(matrix) => Some(matrix),
            _ => None,
        }
    }
}

/// A table: the variables of one node that share dimension names and shape,
/// read as one DuckDB table (decision 8; older sections of the design call it
/// a dimension group). It has one column per dimension and one per variable.
/// Its rows are the cells its variables store: every cell of `shape`, or, for
/// a table of sparse matrices only, their stored entries (decision 9).
#[derive(Debug, Clone)]
pub struct Table {
    pub dims: Vec<String>,
    pub shape: Vec<u64>,
    /// The grid work units follow: the largest chunk length in each dimension
    /// over the variables read chunk by chunk (decision 6).
    pub chunk_shape: Vec<u64>,
    /// `(dimension, path)` of the coordinate array of each dimension that has
    /// one, possibly inherited from an ancestor node.
    pub coords: Vec<(String, String)>,
    pub variables: Vec<Variable>,
}

impl Table {
    /// The paths of the table's variables, in column order.
    pub fn variable_paths(&self) -> Vec<&str> {
        self.variables.iter().map(|v| v.path.as_str()).collect()
    }

    /// `(path, error)` for each variable zarrs cannot open. `read_zarr` fails
    /// on a table that has any.
    pub fn unreadable(&self) -> Vec<(&str, &str)> {
        self.variables
            .iter()
            .filter_map(|v| match &v.storage {
                Storage::Unreadable { error } => Some((v.path.as_str(), error.as_str())),
                _ => None,
            })
            .collect()
    }

    /// Whether every variable is a sparse matrix. Such a table has one row per
    /// stored entry instead of one per cell.
    pub fn is_sparse(&self) -> bool {
        !self.variables.is_empty()
            && self
                .variables
                .iter()
                .all(|v| matches!(v.storage, Storage::Sparse(_)))
    }

    /// Whether this is a table of sparse matrices only, some stored as CSR and
    /// some as CSC. Its stored entries cannot be read in blocks of one major
    /// axis, so `read_zarr` cannot read it as one table.
    pub fn mixes_sparse_storage(&self) -> bool {
        let mut axes = self
            .variables
            .iter()
            .filter_map(|v| v.storage.sparse().map(|m| m.major_axis));
        let first = axes.next();
        self.is_sparse() && axes.any(|a| Some(a) != first)
    }
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

/// Decoded values of one array chunk (or a whole coordinate array).
///
/// Engine adapters write elements out by downcasting with [`Self::as_fixed`] /
/// [`Self::as_strings`] / [`Self::as_masked`]; the core only needs [`Self::key_at`] (pruning).
pub trait ColumnValues: std::fmt::Debug + Send + Sync {
    /// Comparable numeric value of element `idx` after CF decoding (TIMESTAMPs are
    /// microseconds since the epoch). `None` for NULL elements and for dtypes with
    /// no numeric order (strings), which therefore never match a range bound.
    fn key_at(&self, _idx: usize) -> Option<f64> {
        None
    }

    fn as_masked(&self) -> Option<&MaskedValues> {
        None
    }

    fn as_fixed(&self) -> Option<&FixedValues> {
        None
    }

    fn as_strings(&self) -> Option<&StringValues> {
        None
    }
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

    pub fn elem_size(&self) -> usize {
        self.elem_size
    }
}

impl ColumnValues for FixedValues {
    fn key_at(&self, idx: usize) -> Option<f64> {
        crate::decode::element_key(
            &self.bytes,
            &self.dtype,
            &self.encoding,
            &self.sentinel,
            idx,
            self.elem_size,
        )
    }

    fn as_fixed(&self) -> Option<&FixedValues> {
        Some(self)
    }
}

/// Variable-length UTF-8 strings: one decoded `String` per element.
#[derive(Debug)]
pub struct StringValues {
    pub strings: Vec<String>,
}

impl ColumnValues for StringValues {
    fn as_strings(&self) -> Option<&StringValues> {
        Some(self)
    }
}

/// Values with a mask: nonzero mask bytes represent missing values.
#[derive(Debug)]
pub struct MaskedValues {
    pub values: SharedColumnValues,
    pub mask: Vec<u8>,
}

impl ColumnValues for MaskedValues {
    fn key_at(&self, idx: usize) -> Option<f64> {
        if *self.mask.get(idx)? != 0 {
            None
        } else {
            self.values.key_at(idx)
        }
    }

    fn as_masked(&self) -> Option<&MaskedValues> {
        Some(self)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masked_keys_ignore_missing_and_out_of_range_values() {
        let fixed: SharedColumnValues = Arc::new(
            FixedValues::new(
                vec![10, 20, 30],
                ZarrDtype::UInt8,
                ColumnEncoding::Plain,
                None,
            )
            .unwrap(),
        );
        let masked: SharedColumnValues = Arc::new(MaskedValues {
            values: fixed,
            mask: vec![0, 1, 255, 0],
        });
        assert_eq!(masked.key_at(0), Some(10.0));
        for idx in [1, 2, 3, 4, usize::MAX] {
            assert_eq!(masked.key_at(idx), None);
        }
        assert!(masked.as_masked().is_some());
        assert!(masked.as_fixed().is_none());
        assert!(masked.as_strings().is_none());
        let coord = CoordArray {
            dtype: ZarrDtype::UInt8,
            encoding: ColumnEncoding::Plain,
            sentinel: None,
            data: masked,
        };
        assert_eq!(
            crate::bounds::index_mask(&coord, 5, None, None),
            [true, false, false, false, false]
        );
    }
}
