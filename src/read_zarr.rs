use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use duckdb::core::{DataChunkHandle, LogicalTypeId};
use duckdb::vtab::{BindInfo, InitInfo, TableFunctionInfo, VTab};

use crate::zarr_reader::meta::{
    build_column_defs, build_work_units, extract_file_system, infer_dim_groups, load_coord_array,
    open_array, open_store, ZarrArray, ZarrStore,
};
use crate::zarr_reader::types::{ColumnDef, CoordArray, DimGroup, WorkUnit, ZarrDtype};

// ---------------------------------------------------------------------------
// BindData — shared, immutable, produced once per query.
// ---------------------------------------------------------------------------

pub struct ReadZarrBind {
    pub group_shape: Vec<u64>,
    pub group_chunk_shape: Vec<u64>,
    pub columns: Vec<ColumnDef>,
    pub coord_arrays: HashMap<String, CoordArray>,
    /// Pre-opened data-variable arrays; avoids O(n_chunks × n_vars) metadata reads.
    pub arrays: HashMap<String, ZarrArray>,
    pub work_units: Vec<WorkUnit>,
    pub next_unit: AtomicUsize,
}

// SAFETY: All fields are Send+Sync: AtomicUsize, HashMap with Send values, Vec.
// DuckDB calls bind once; the resulting data is read-only during scan.
unsafe impl Send for ReadZarrBind {}
unsafe impl Sync for ReadZarrBind {}

// ---------------------------------------------------------------------------
// InitData — mutable per-thread state.
// ---------------------------------------------------------------------------

pub struct ReadZarrInit {
    /// Maps schema column index → output-vector index (sorted by schema index).
    /// Explicit mapping avoids any assumption about the order DuckDB returns projected indices.
    pub projected_cols: HashMap<usize, usize>,
    pub inner: Mutex<LocalState>,
}

pub struct LocalState {
    /// Index of the current work unit being streamed out row-by-row.
    pub current_unit_idx: usize,
    /// Decoded bytes for the current work unit, one entry per data variable.
    pub current_chunk_bytes: HashMap<String, Vec<u8>>,
    /// Row cursor within the current chunk (how many rows have been emitted).
    pub row_cursor: usize,
    /// Total rows in the current chunk.
    pub chunk_rows: usize,
    pub done: bool,
}

// SAFETY: projected_cols is written once at init and read-only thereafter.
// inner is a Mutex<LocalState>, so concurrent access is synchronized.
unsafe impl Send for ReadZarrInit {}
unsafe impl Sync for ReadZarrInit {}

// ---------------------------------------------------------------------------
// VTab implementation.
// ---------------------------------------------------------------------------

pub struct ReadZarrVTab;

impl VTab for ReadZarrVTab {
    type BindData = ReadZarrBind;
    type InitData = ReadZarrInit;

    fn bind(bind: &BindInfo) -> Result<Self::BindData, Box<dyn std::error::Error>> {
        let path_val = bind.get_parameter(0);
        let store_path = path_val.to_string();

        // Optional dims= named parameter: JSON array string e.g. '["time","lat","lon"]'
        let requested_dims: Option<Vec<String>> = bind
            .get_named_parameter("dims")
            .map(|v| parse_dims_param(&v.to_string()))
            .transpose()?;

        let fs = unsafe { extract_file_system(bind) };
        let store = open_store(&store_path, Some(fs))?;
        let array_names = crate::zarr_reader::meta::list_array_names(&store_path, &store)?;

        if array_names.is_empty() {
            return Err(format!("no Zarr arrays found in '{store_path}'").into());
        }

        let (dim_groups, _coord_names) = infer_dim_groups(&store, &array_names)?;

        if dim_groups.is_empty() {
            return Err(format!("no data variables found in '{store_path}'").into());
        }

        let group = match requested_dims {
            Some(ref dims) => {
                dim_groups.iter().find(|g| g.dims == *dims).ok_or_else(|| {
                    format!(
                        "'{store_path}': no dimension group matches dims={dims:?}; available: {:?}",
                        dim_groups.iter().map(|g| &g.dims).collect::<Vec<_>>()
                    )
                })?
            }
            None => {
                if dim_groups.len() > 1 {
                    return Err(format!(
                        "'{store_path}' contains multiple dimension groups ({}) {:?}; use read_zarr(path, dims='[\"time\",\"lat\",\"lon\"]') to select one",
                        dim_groups.len(),
                        dim_groups.iter().map(|g| &g.dims).collect::<Vec<_>>()
                    ).into());
                }
                &dim_groups[0]
            }
        };

        finish_bind(bind, store, group)
    }

    fn supports_pushdown() -> bool {
        true
    }

    fn init(init: &InitInfo) -> Result<Self::InitData, Box<dyn std::error::Error>> {
        let bind = unsafe { &*init.get_bind_data::<ReadZarrBind>() };
        init.set_max_threads(bind.work_units.len().max(1) as u64);

        // DuckDB guarantees output.flat_vector(i) in scan() corresponds to
        // get_column_indices()[i] from init(). Do NOT sort — sorting destroys
        // the positional relationship and scrambles output in JOIN context.
        let projected_cols: HashMap<usize, usize> = init.get_column_indices()
            .into_iter()
            .enumerate()
            .map(|(out_idx, col_idx)| (col_idx as usize, out_idx))
            .collect();
        Ok(ReadZarrInit {
            projected_cols,
            inner: Mutex::new(LocalState {
                current_unit_idx: usize::MAX,
                current_chunk_bytes: HashMap::new(),
                row_cursor: 0,
                chunk_rows: 0,
                done: false,
            }),
        })
    }

    fn func(
        func: &TableFunctionInfo<Self>,
        output: &mut DataChunkHandle,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let bind = func.get_bind_data();
        let init = func.get_init_data();
        let projected = &init.projected_cols;
        let mut state = init.inner.lock().unwrap();

        if state.done {
            output.set_len(0);
            return Ok(());
        }

        let vector_size = unsafe { duckdb::ffi::duckdb_vector_size() as usize };
        let mut rows_written = 0usize;

        while rows_written < vector_size {
            // If we've exhausted the current chunk, move to the next work unit.
            if state.row_cursor >= state.chunk_rows {
                // Claim the next work unit atomically (supports parallel morsel theft).
                let unit_idx = bind.next_unit.fetch_add(1, Ordering::Relaxed);
                if unit_idx >= bind.work_units.len() {
                    state.done = true;
                    break;
                }
                let wu = &bind.work_units[unit_idx];
                // Decode chunk for each data variable.
                let chunk_bytes = decode_work_unit(bind, wu, projected)?;
                let chunk_rows = compute_chunk_rows(wu, &bind.group_shape, &bind.group_chunk_shape);
                state.current_unit_idx = unit_idx;
                state.current_chunk_bytes = chunk_bytes;
                state.row_cursor = 0;
                state.chunk_rows = chunk_rows;
            }

            let wu = &bind.work_units[state.current_unit_idx];
            let remaining_in_chunk = state.chunk_rows - state.row_cursor;
            let can_write = (vector_size - rows_written).min(remaining_in_chunk);

            if can_write == 0 {
                break;
            }

            // fill_output_chunk writes into output starting at rows_written.
            // It reads from the chunk starting at row_cursor.
            let written = fill_chunk_slice(
                &bind.columns,
                &bind.coord_arrays,
                wu,
                &bind.group_shape,
                &bind.group_chunk_shape,
                &state.current_chunk_bytes,
                output,
                rows_written,
                state.row_cursor,
                can_write,
                projected,
            );

            state.row_cursor += written;
            rows_written += written;
        }

        output.set_len(rows_written);
        Ok(())
    }

    fn parameters() -> Option<Vec<duckdb::core::LogicalTypeHandle>> {
        Some(vec![LogicalTypeId::Varchar.into()])
    }

    fn named_parameters() -> Option<Vec<(String, duckdb::core::LogicalTypeHandle)>> {
        Some(vec![("dims".to_string(), LogicalTypeId::Varchar.into())])
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Parse a `dims` named-parameter value into an ordered list of dimension names.
///
/// Accepts either a JSON array (`'["time","lat","lon"]'`) or a plain
/// comma-separated string (`'time,lat,lon'`).
fn parse_dims_param(s: &str) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let trimmed = s.trim();
    if trimmed.starts_with('[') {
        let arr: Vec<String> = serde_json::from_str(trimmed)?;
        Ok(arr)
    } else {
        Ok(trimmed.split(',').map(|d| d.trim().to_string()).filter(|d| !d.is_empty()).collect())
    }
}

fn finish_bind(
    bind: &BindInfo,
    store: ZarrStore,
    group: &DimGroup,
) -> Result<ReadZarrBind, Box<dyn std::error::Error>> {
    // Load coord arrays.
    let mut coord_arrays: HashMap<String, CoordArray> = HashMap::new();
    for coord_name in &group.coord_var_names {
        let ca = load_coord_array(&store, coord_name)?;
        coord_arrays.insert(coord_name.clone(), ca);
    }

    let columns = build_column_defs(&store, group, &coord_arrays)?;

    // Register output columns with DuckDB.
    for col in &columns {
        let duckdb_type = col.on_disk_dtype.to_duckdb_type(&col.encoding);
        bind.add_result_column(&col.name, duckdb_type);
    }

    // Pre-open data variable arrays once at bind time.
    let mut arrays: HashMap<String, ZarrArray> = HashMap::new();
    for col in &columns {
        if !col.is_coord {
            let arr = open_array(&store, &col.name)?;
            arrays.insert(col.name.clone(), arr);
        }
    }

    let work_units = build_work_units(group);

    Ok(ReadZarrBind {
        group_shape: group.shape.clone(),
        group_chunk_shape: group.chunk_shape.clone(),
        columns,
        coord_arrays,
        arrays,
        work_units,
        next_unit: AtomicUsize::new(0),
    })
}

fn decode_work_unit(
    bind: &ReadZarrBind,
    wu: &WorkUnit,
    projected: &HashMap<usize, usize>,
) -> Result<HashMap<String, Vec<u8>>, Box<dyn std::error::Error>> {
    let mut chunk_bytes = HashMap::new();

    for (col_idx, col) in bind.columns.iter().enumerate() {
        if col.is_coord {
            continue; // coord data is pre-loaded at bind time
        }
        if !projected.contains_key(&col_idx) {
            continue; // skip decompression for non-projected data vars
        }
        let arr = bind.arrays.get(&col.name)
            .ok_or_else(|| format!("array '{}' not found in bind cache", col.name))?;
        // ArrayBytes<'static>: zarrs convention for requesting owned decoded bytes.
        // retrieve_chunk fills missing (implicit) chunks with fill_value automatically.
        let raw = arr.retrieve_chunk::<zarrs::array::ArrayBytes<'static>>(&wu.chunk_indices)?;
        let bytes: Vec<u8> = raw.into_fixed()
            .map_err(|_| format!("variable-length dtype not supported for '{}'", col.name))?
            .into_owned();
        chunk_bytes.insert(col.name.clone(), bytes);
    }

    Ok(chunk_bytes)
}

fn compute_chunk_rows(wu: &WorkUnit, shape: &[u64], chunk_shape: &[u64]) -> usize {
    let ndim = wu.chunk_indices.len();
    (0..ndim)
        .map(|k| {
            let origin = wu.chunk_indices[k] * chunk_shape[k];
            let remaining = shape[k] - origin;
            remaining.min(chunk_shape[k]) as usize
        })
        .product()
}

/// Fill a slice of rows from a chunk into the DuckDB output vector.
///
/// `vector_base` = starting row in the DuckDB output vector.
/// `chunk_row_start` = starting row within the chunk.
/// `n_rows` = how many rows to write.
#[allow(clippy::too_many_arguments)]
fn fill_chunk_slice(
    col_defs: &[ColumnDef],
    coord_arrays: &HashMap<String, CoordArray>,
    wu: &WorkUnit,
    group_shape: &[u64],
    group_chunk_shape: &[u64],
    chunk_bytes: &HashMap<String, Vec<u8>>,
    output: &mut DataChunkHandle,
    vector_base: usize,
    chunk_row_start: usize,
    n_rows: usize,
    projected: &HashMap<usize, usize>,
) -> usize {
    let ndim = wu.chunk_indices.len();

    // Logical chunk shape: clipped to array bounds for boundary chunks.
    // Used to determine the number of valid rows and to map flat_row → dim_indices.
    let chunk_shape: Vec<usize> = (0..ndim)
        .map(|k| {
            let origin = wu.chunk_indices[k] * group_chunk_shape[k];
            let remaining = group_shape[k] - origin;
            remaining.min(group_chunk_shape[k]) as usize
        })
        .collect();

    let chunk_origin: Vec<usize> = (0..ndim)
        .map(|k| (wu.chunk_indices[k] * group_chunk_shape[k]) as usize)
        .collect();

    // Logical strides: map flat_row → per-dim indices within the logical chunk.
    let mut strides = vec![1usize; ndim];
    for k in (0..ndim.saturating_sub(1)).rev() {
        strides[k] = strides[k + 1] * chunk_shape[k + 1];
    }

    // Physical (zarrs) strides: zarrs always returns full-chunk-size bytes, including
    // fill-value padding for boundary chunks. Use group_chunk_shape for byte offset math.
    let zarrs_shape: Vec<usize> = (0..ndim).map(|k| group_chunk_shape[k] as usize).collect();
    let mut zarrs_strides = vec![1usize; ndim];
    for k in (0..ndim.saturating_sub(1)).rev() {
        zarrs_strides[k] = zarrs_strides[k + 1] * zarrs_shape[k + 1];
    }

    for (col_idx, col_def) in col_defs.iter().enumerate() {
        let out_vec_idx = match projected.get(&col_idx) {
            Some(&i) => i,
            None => continue,
        };

        let mut vector = output.flat_vector(out_vec_idx);

        for out_i in 0..n_rows {
            let flat_row = chunk_row_start + out_i;
            let dst = vector_base + out_i;

            // Map flat logical row → per-dim indices within the logical chunk.
            let dim_indices: Vec<usize> = (0..ndim)
                .map(|k| (flat_row / strides[k]) % chunk_shape[k])
                .collect();
            let global_indices: Vec<usize> = (0..ndim)
                .map(|k| chunk_origin[k] + dim_indices[k])
                .collect();

            // Physical element index in the zarrs byte buffer (accounting for padding).
            let zarrs_flat: usize = (0..ndim).map(|k| dim_indices[k] * zarrs_strides[k]).sum();

            if let Some(dim_k) = col_def.dim_idx {
                let coord_idx = global_indices[dim_k];
                if let Some(ca) = coord_arrays.get(&col_def.name) {
                    let elem_size = ca.dtype.byte_size();
                    crate::zarr_reader::scan::fill_scalar_element_pub(
                        &mut vector,
                        &ca.bytes,
                        &ca.dtype,
                        &ca.sentinel,
                        coord_idx,
                        elem_size,
                        dst,
                    );
                } else {
                    // Unindexed dim → synthesize range.
                    let slot = vector.as_mut_ptr::<i64>();
                    unsafe { *slot.add(dst) = global_indices[dim_k] as i64; }
                }
            } else {
                // Data variable: use zarrs_flat to index into the physical byte buffer.
                if let Some(bytes) = chunk_bytes.get(&col_def.name) {
                    let elem_size = col_def.on_disk_dtype.byte_size();
                    fill_data_element(
                        &mut vector,
                        bytes,
                        &col_def.on_disk_dtype,
                        &col_def.encoding,
                        &col_def.sentinel,
                        zarrs_flat,
                        elem_size,
                        dst,
                    );
                } else {
                    unreachable!("projected data variable '{}' missing from chunk_bytes", col_def.name);
                }
            }
        }
    }

    n_rows
}

#[allow(clippy::too_many_arguments)]
fn fill_data_element(
    vector: &mut duckdb::core::FlatVector<'_>,
    bytes: &[u8],
    dtype: &ZarrDtype,
    encoding: &crate::zarr_reader::types::ColumnEncoding,
    sentinel: &Option<crate::zarr_reader::types::FillSentinel>,
    flat_row: usize,
    elem_size: usize,
    dst: usize,
) {
    use crate::zarr_reader::types::ColumnEncoding;
    match encoding {
        ColumnEncoding::Plain => {
            crate::zarr_reader::scan::fill_scalar_element_pub(
                vector, bytes, dtype, sentinel, flat_row, elem_size, dst,
            );
        }
        ColumnEncoding::PackedInt { scale_factor, add_offset } => {
            let src = flat_row * elem_size;
            let raw = crate::zarr_reader::scan::read_int_as_i64_pub(bytes, dtype, src);
            let is_null = match sentinel {
                Some(crate::zarr_reader::types::FillSentinel::Int(v)) => raw == *v,
                Some(crate::zarr_reader::types::FillSentinel::UInt(v)) => (raw as u64) == *v,
                _ => false,
            };
            if is_null {
                vector.set_null(dst);
            } else {
                let slot = vector.as_mut_ptr::<f64>();
                unsafe { *slot.add(dst) = raw as f64 * scale_factor + add_offset; }
            }
        }
    }
}
