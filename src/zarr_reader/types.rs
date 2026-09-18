use duckdb::core::{LogicalTypeHandle, LogicalTypeId};

/// On-disk Zarr numeric dtype as reported by zarrs `DataType::to_string()`.
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

    pub fn byte_size(&self) -> usize {
        match self {
            Self::Bool | Self::Int8 | Self::UInt8 => 1,
            Self::Int16 | Self::UInt16 => 2,
            Self::Int32 | Self::UInt32 | Self::Float32 => 4,
            Self::Int64 | Self::UInt64 | Self::Float64 => 8,
        }
    }

    /// DuckDB output type for this column, accounting for packed-int decoding.
    pub fn to_duckdb_type(&self, encoding: &ColumnEncoding) -> LogicalTypeHandle {
        match encoding {
            ColumnEncoding::PackedInt { .. } => LogicalTypeId::Double.into(),
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
            },
        }
    }
}

/// How an on-disk array column is decoded into DuckDB output values.
#[derive(Debug, Clone)]
pub enum ColumnEncoding {
    Plain,
    PackedInt { scale_factor: f64, add_offset: f64 },
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
    pub name: String,
    pub on_disk_dtype: ZarrDtype,
    pub encoding: ColumnEncoding,
    pub sentinel: Option<FillSentinel>,
    pub is_coord: bool,
    /// For coord columns: the dimension index this coord maps to in group.dims.
    /// None for data variable columns.
    pub dim_idx: Option<usize>,
}

/// One dim group: arrays sharing an identical ordered dimension set.
#[derive(Debug)]
pub struct DimGroup {
    pub dims: Vec<String>,
    pub shape: Vec<u64>,
    pub chunk_shape: Vec<u64>,
    pub data_var_names: Vec<String>,
    pub coord_var_names: Vec<String>,
}

/// Raw bytes of a pre-loaded coordinate array (shape is 1-D: `[n]`).
#[derive(Debug, Clone)]
pub struct CoordArray {
    pub dtype: ZarrDtype,
    pub encoding: ColumnEncoding,
    pub sentinel: Option<FillSentinel>,
    /// Row-major bytes, length = `n * dtype.byte_size()`.
    pub bytes: Vec<u8>,
}

/// One unit of parallel work: a chunk index tuple for all data variables.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkUnit {
    pub chunk_indices: Vec<u64>,
}
