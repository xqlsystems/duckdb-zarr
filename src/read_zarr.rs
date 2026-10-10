use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use duckdb::core::{DataChunkHandle, LogicalTypeHandle, LogicalTypeId};
use duckdb::vtab::{BindInfo, InitInfo, TableFunctionInfo, VTab, Value};
use zarrs::array::ArraySubset;

use crate::zarr_reader::anndata;
use crate::zarr_reader::meta::{
    array_path_group, build_column_defs, build_work_units, extract_file_system, first_chunk_shape,
    load_coord_array, open_array, open_store, ZarrArray, ZarrStore,
};
use crate::zarr_reader::sparse::{decode_block, plan_blocks, MajorRange, SparseBlock, SparseInput};
use crate::zarr_reader::tree::{self, StoreTree};
use crate::zarr_reader::types::{
    ColumnDef, CoordArray, DimGroup, FixedValues, MaskedValues, SharedColumnValues, StringValues,
    WorkUnit, ZarrDtype,
};

// ---------------------------------------------------------------------------
// BindData — shared, immutable, produced once per query.
// ---------------------------------------------------------------------------

/// Decoded major ranges of sparse matrices, by (variable, range start), with
/// the number of work units that have yet to use each.
type RangeCache = HashMap<(String, u64), (Arc<MajorRange>, u64)>;

pub struct ReadZarrBind {
    pub group_shape: Vec<u64>,
    pub group_chunk_shape: Vec<u64>,
    pub columns: Vec<ColumnDef>,
    pub coord_arrays: HashMap<String, CoordArray>,
    /// Pre-opened data-variable arrays; avoids O(n_chunks × n_vars) metadata reads.
    pub arrays: HashMap<String, ZarrArray>,
    /// Data variables whose chunk shape differs from `group_chunk_shape`. They
    /// are read as an array subset per work unit instead of one chunk.
    pub subset_reads: HashSet<String>,
    /// Pre-opened `mask` arrays of nullable variables, keyed by variable name.
    pub masks: HashMap<String, ZarrArray>,
    /// Sparse matrices, keyed by variable name.
    pub sparse: HashMap<String, SparseInput>,
    /// For a table of sparse matrices only: ranges of the matrices' major axis,
    /// one per work unit, in place of `work_units`.
    pub sparse_blocks: Option<Vec<(u64, u64)>>,
    /// For a table of sparse matrices only: the axis that `indptr` indexes
    /// (0 for CSR, 1 for CSC), the same for every matrix of the table.
    pub sparse_major_axis: usize,
    /// For sparse matrices in a dense table: decoded ranges of a matrix's major
    /// axis, shared by the work units across each range so that its entries
    /// are read once, not once per chunk of the minor axis. Keyed by variable
    /// and range start; the count is how many work units have yet to use the
    /// range, and the entry is dropped after the last one.
    pub sparse_ranges: Mutex<RangeCache>,
    /// The outer axis of the work-unit order. Only matrices with this major
    /// axis use `sparse_ranges`.
    pub cached_major_axis: usize,
    pub work_units: Vec<WorkUnit>,
    pub next_unit: AtomicUsize,
}

impl ReadZarrBind {
    fn n_units(&self) -> usize {
        match &self.sparse_blocks {
            Some(blocks) => blocks.len(),
            None => self.work_units.len(),
        }
    }

    /// The sparse matrices of a sparse table, in data-column order.
    fn sparse_inputs(&self) -> Vec<&SparseInput> {
        sparse_inputs(&self.columns, &self.sparse)
    }
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
    /// For each data column, in order, whether it is projected. A sparse table
    /// skips reading the values of the others.
    pub projected_vars: Vec<bool>,
    pub inner: Mutex<LocalState>,
}

pub struct LocalState {
    /// Index of the current work unit being streamed out row-by-row.
    pub current_unit_idx: usize,
    /// Decoded values for the current work unit, one entry per data variable.
    pub current_chunk_values: HashMap<String, UnitValues>,
    /// The rows of the current block of a sparse table.
    pub current_sparse: SparseBlock,
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

        // Optional dims= named parameter: a list of dimension names,
        // e.g. read_zarr(store, dims=['time','lat','lon']).
        let requested_dims: Option<Vec<String>> = bind
            .get_named_parameter("dims")
            .map(parse_dims_param)
            .transpose()?;
        let array_path = bind
            .get_named_parameter("array_path")
            .map(|value| value.to_string());
        let array_alias = bind
            .get_named_parameter("array")
            .map(|value| value.to_string());
        if array_path.is_some() && array_alias.is_some() {
            return Err("use either array_path= or \"array\"=, not both".into());
        }
        let requested_array = array_path.or(array_alias);
        let requested_group = tree::group_param(bind)?;
        if requested_group.is_some() && requested_array.is_some() {
            return Err(
                "use either group_path= or array_path=, not both; array_path= is relative to \
                 the store root"
                    .into(),
            );
        }

        // Mirrors xarray.open_zarr's decode_times=: on by default, opt out to see
        // the raw CF offsets instead of TIMESTAMPs.
        let decode_times = bind
            .get_named_parameter("decode_times")
            .map(|value| value.is_null() || value.to_bool())
            .unwrap_or(true);

        let fs = unsafe { extract_file_system(bind) };
        let store = open_store(&store_path, Some(fs))?;
        if let Some(requested) = requested_array {
            // Only this one array is needed. Listing requires consolidated metadata
            // on remote stores, so here it is best-effort: when available it lets
            // coordinate arrays be resolved; when not (e.g. an OME-Zarr store served
            // over HTTP without consolidation) the array still reads by array_path,
            // with dimensions synthesized as integer indices.
            let array_names =
                crate::zarr_reader::meta::list_array_names(&store_path, &store).unwrap_or_default();
            let (array_name, group) = array_path_group(&store, &array_names, &requested)?;
            if let Some(dims) = requested_dims {
                if group.dims != dims {
                    return Err(format!(
                        "array '{array_name}' has dimensions {:?}, not {dims:?}",
                        group.dims
                    )
                    .into());
                }
            }
            return finish_bind(bind, store, &group, decode_times, ColumnNames::ArrayPath);
        }

        let array_names = crate::zarr_reader::meta::list_array_names(&store_path, &store)?;
        if array_names.is_empty() {
            return Err(format!("no Zarr arrays found in '{store_path}'").into());
        }

        // One node (Zarr group) of the store, the root unless group_path= says
        // otherwise, as in xarray.open_zarr (design decision 8).
        let node = requested_group.unwrap_or_default();
        let shown = tree::display_group(&node);
        tree::ensure_group_exists(&store_path, &array_names, &node)?;
        let store_tree = StoreTree::load_for_node(&store, &array_names, &node)?;
        anndata::check_group_path(&store_path, store_tree.layout.as_ref(), &node)?;
        let node_groups = store_tree.dim_groups(&node);
        if let Some(msg) = node_groups.misaligned {
            return Err(msg.into());
        }
        let dim_groups = &node_groups.groups;

        if dim_groups.is_empty() {
            // AnnData's obs and var columns are read from the root group, and
            // raw/var's from raw (decision 9).
            let mut msg = format!("'{store_path}': no data variables in group '{shown}'");
            if !node_groups.unnamed.is_empty() {
                msg.push_str(&format!(
                    "; these arrays declare no dimension names: {:?}. Read one with \
                     array_path=, which names its dimensions dim_0, dim_1 and so on",
                    node_groups.unnamed
                ));
            }
            msg.push_str(&format!(
                ". Use group_path= to read another group; read_zarr_groups('{store_path}') lists \
                 the tables in every group"
            ));
            return Err(msg.into());
        }

        let available = || {
            let mut dims: Vec<&Vec<String>> = dim_groups.iter().map(|g| &g.dims).collect();
            dims.dedup();
            dims
        };
        let wanted: &[String] = match &requested_dims {
            Some(dims) => dims,
            None => {
                let distinct = available();
                if distinct.len() > 1 {
                    return Err(format!(
                        "'{store_path}': group '{shown}' contains multiple dimension groups ({}) \
                         {distinct:?}; use dims= to select a compatible group or array_path= to \
                         select one array",
                        distinct.len()
                    )
                    .into());
                }
                &dim_groups[0].dims
            }
        };
        let matching: Vec<&DimGroup> = dim_groups.iter().filter(|g| g.dims == wanted).collect();
        let group = match matching.as_slice() {
            [] => {
                return Err(format!(
                    "'{store_path}': no dimension group matches dims={wanted:?} in group \
                     '{shown}'; available: {:?}",
                    available()
                )
                .into())
            }
            [group] => *group,
            [a, b, ..] => {
                return Err(format!(
                    "array shape mismatch in dim group {wanted:?}: {:?} {:?} vs {:?} {:?}; \
                     use array_path= to select one array",
                    a.data_var_names, a.shape, b.data_var_names, b.shape
                )
                .into());
            }
        };

        // Leaving an array out would return the table without it and no sign
        // that it is missing.
        if !group.unreadable.is_empty() {
            let names: Vec<&str> = group.unreadable.iter().map(|(p, _)| p.as_str()).collect();
            let (first, err) = &group.unreadable[0];
            return Err(format!(
                "'{store_path}': zarrs cannot open {names:?}, which belong to the table \
                 {wanted:?} in group '{shown}' ('{first}': {err}); read the other arrays one \
                 at a time with array_path="
            )
            .into());
        }
        finish_bind(bind, store, group, decode_times, ColumnNames::Basename)
    }

    fn supports_pushdown() -> bool {
        true
    }

    fn init(init: &InitInfo) -> Result<Self::InitData, Box<dyn std::error::Error>> {
        let bind = unsafe { &*init.get_bind_data::<ReadZarrBind>() };
        init.set_max_threads(bind.n_units().max(1) as u64);

        // DuckDB guarantees output.flat_vector(i) in scan() corresponds to
        // get_column_indices()[i] from init(). Do NOT sort — sorting destroys
        // the positional relationship and scrambles output in JOIN context.
        let projected_cols: HashMap<usize, usize> = init
            .get_column_indices()
            .into_iter()
            .enumerate()
            .map(|(out_idx, col_idx)| (col_idx as usize, out_idx))
            .collect();
        let projected_vars = bind
            .columns
            .iter()
            .enumerate()
            .filter(|(_, c)| !c.is_coord)
            .map(|(i, _)| projected_cols.contains_key(&i))
            .collect();
        Ok(ReadZarrInit {
            projected_cols,
            projected_vars,
            inner: Mutex::new(LocalState {
                current_unit_idx: usize::MAX,
                current_chunk_values: HashMap::new(),
                current_sparse: SparseBlock::default(),
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
                if unit_idx >= bind.n_units() {
                    state.done = true;
                    break;
                }
                if let Some(blocks) = &bind.sparse_blocks {
                    let block = decode_block(
                        &bind.sparse_inputs(),
                        blocks[unit_idx],
                        &init.projected_vars,
                    )?;
                    state.chunk_rows = block.len();
                    state.current_sparse = block;
                    state.current_unit_idx = unit_idx;
                    state.row_cursor = 0;
                    continue;
                }
                let wu = &bind.work_units[unit_idx];
                // Decode chunk for each data variable.
                let chunk_values = decode_work_unit(bind, wu, projected)?;
                let chunk_rows = compute_chunk_rows(wu, &bind.group_shape, &bind.group_chunk_shape);
                state.current_unit_idx = unit_idx;
                state.current_chunk_values = chunk_values;
                state.row_cursor = 0;
                state.chunk_rows = chunk_rows;
            }

            let remaining_in_chunk = state.chunk_rows - state.row_cursor;
            let can_write = (vector_size - rows_written).min(remaining_in_chunk);

            if can_write == 0 {
                break;
            }

            if bind.sparse_blocks.is_some() {
                fill_sparse_rows(
                    bind,
                    &state.current_sparse,
                    output,
                    rows_written,
                    state.row_cursor,
                    can_write,
                    projected,
                );
                state.row_cursor += can_write;
                rows_written += can_write;
                continue;
            }

            let wu = &bind.work_units[state.current_unit_idx];

            // fill_output_chunk writes into output starting at rows_written.
            // It reads from the chunk starting at row_cursor.
            let written = fill_chunk_slice(
                &bind.columns,
                &bind.coord_arrays,
                wu,
                &bind.group_shape,
                &bind.group_chunk_shape,
                &state.current_chunk_values,
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
        let mut params = vec![
            (
                "dims".to_string(),
                LogicalTypeHandle::list(&LogicalTypeId::Varchar.into()),
            ),
            ("array".to_string(), LogicalTypeId::Varchar.into()),
            ("array_path".to_string(), LogicalTypeId::Varchar.into()),
            ("decode_times".to_string(), LogicalTypeId::Boolean.into()),
        ];
        params.extend(crate::zarr_reader::tree::group_named_parameters());
        Some(params)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Extract the ordered dimension names from a `dims` named-parameter value.
///
/// `dims` is a `LIST(VARCHAR)`, e.g. `read_zarr(store, dims=['time','lat','lon'])`.
/// A single data-variable column selected by `array_path` has a numeric level
/// name (`0`) or a nested store-relative path (`labels/nuclei/0`), both of which
/// otherwise need SQL double-quoting. Surface it as `value` instead. Ordinary
/// variable names (`temperature`, `precip`) are left unchanged.
fn needs_value_alias(name: &str) -> bool {
    name.contains('/') || name.parse::<u64>().is_ok()
}

fn parse_dims_param(value: Value) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let items = value
        .to_list()
        .ok_or("dims must be a list of dimension names, e.g. dims=['time','lat','lon']")?;
    Ok(items.iter().map(|item| item.to_string()).collect())
}

/// How data-variable columns are named.
#[derive(Clone, Copy, PartialEq)]
enum ColumnNames {
    /// The variable's name within its group, as in xarray (`foo`, not `a/b/foo`).
    Basename,
    /// One array selected by `array_path=`: its store-relative path.
    ArrayPath,
}

fn finish_bind(
    bind: &BindInfo,
    store: ZarrStore,
    group: &DimGroup,
    decode_times: bool,
    names: ColumnNames,
) -> Result<ReadZarrBind, Box<dyn std::error::Error>> {
    if group.mixes_sparse_storage() {
        return Err(format!(
            "sparse matrices {:?} are stored partly as CSR and partly as CSC, so their stored \
             entries cannot be read as one table; read each alone with array_path := '{}'",
            group.data_var_names, group.data_var_names[0]
        )
        .into());
    }
    // Load coord arrays.
    let mut coord_arrays: HashMap<String, CoordArray> = HashMap::new();
    for (dim, coord_name) in &group.coords {
        let ca = load_coord_array(&store, coord_name, decode_times)?;
        coord_arrays.insert(dim.clone(), ca);
    }

    let columns = build_column_defs(&store, group, &coord_arrays, decode_times)?;

    // Register output columns with DuckDB. A group read names each data column
    // after the variable's name within the group. When a single array's name is a
    // numeric level (`0`) or, for array_path=, a nested store-relative path
    // (`labels/nuclei/0`), surface that value column as `value` so callers don't
    // have to double-quote it. Decoding still keys off `col.name`.
    let single_data_var = columns.iter().filter(|c| !c.is_coord).count() == 1;
    // DuckDB column names ignore case and must be unique. A data column whose
    // name equals, ignoring case, a dimension of the table (an AnnData obs
    // column called `obs` or `Obs`) or an earlier column (`A` and `a`) keeps its
    // store path instead (`obs/obs`), as array_path= names it.
    let mut used: HashSet<String> = group.dims.iter().map(|d| d.to_lowercase()).collect();
    for col in &columns {
        let duckdb_type = col.on_disk_dtype.to_duckdb_type(&col.encoding);
        let basename = tree::basename(&col.name);
        let collides = matches!(names, ColumnNames::Basename)
            && !col.is_coord
            && used.contains(&basename.to_lowercase());
        let name = match names {
            ColumnNames::Basename if !collides => basename,
            _ => col.name.as_str(),
        };
        let mut display_name =
            if !col.is_coord && !collides && single_data_var && needs_value_alias(name) {
                "value".to_string()
            } else {
                name.to_string()
            };
        // Paths can collide too (`obs/obs` and `obs/Obs`); number the later one.
        if collides && used.contains(&display_name.to_lowercase()) {
            display_name = (1..)
                .map(|n| format!("{name}_{n}"))
                .find(|candidate| !used.contains(&candidate.to_lowercase()))
                .expect("an unused suffix exists");
        }
        if !col.is_coord {
            used.insert(display_name.to_lowercase());
        }
        bind.add_result_column(&display_name, duckdb_type);
    }

    // Pre-open data variable arrays once at bind time.
    let mut arrays: HashMap<String, ZarrArray> = HashMap::new();
    let mut masks: HashMap<String, ZarrArray> = HashMap::new();
    let mut sparse: HashMap<String, SparseInput> = HashMap::new();
    let mut subset_reads = HashSet::new();
    for col in columns.iter().filter(|c| !c.is_coord) {
        if let Some(matrix) = &col.sparse {
            sparse.insert(
                col.name.clone(),
                SparseInput::open(&store, &col.name, matrix)?,
            );
            continue;
        }
        let source = col.source.as_deref().unwrap_or(&col.name);
        let arr = open_array(&store, source)?;
        let mut chunked_like_plan = first_chunk_shape(&arr)? == group.chunk_shape;
        arrays.insert(col.name.clone(), arr);
        if let Some(mask) = &col.mask {
            let mask = open_array(&store, mask)?;
            // A mask is read the same way as its values, so both share a layout.
            chunked_like_plan &= first_chunk_shape(&mask)? == group.chunk_shape;
            masks.insert(col.name.clone(), mask);
        }
        if !chunked_like_plan {
            subset_reads.insert(col.name.clone());
        }
    }

    // A table of sparse matrices only has one row per stored entry, read in
    // blocks of the major axis. Every matrix must be stored the same way.
    let mut sparse_major_axis = 0;
    let sparse_blocks = if group.is_sparse() {
        let inputs = sparse_inputs(&columns, &sparse);
        sparse_major_axis = inputs[0].matrix.major_axis;
        Some(plan_blocks(&inputs, SPARSE_BLOCK_ENTRIES))
    } else {
        None
    };

    let mut work_units = build_work_units(group);
    // Work units run roughly in order, and the decoded range of a sparse matrix
    // is kept until every unit across it has run (`sparse_ranges`). That bounds
    // memory to a few ranges only for a matrix whose major axis is the outer
    // axis of the unit order. build_work_units puts axis 0 outermost, which
    // suits CSR; when every sparse matrix of the table is CSC, put axis 1
    // outermost instead. A matrix stored the other way from the order is read
    // per work unit without the cache.
    let mut cached_major_axis = 0;
    if sparse_blocks.is_none()
        && !sparse.is_empty()
        && sparse.values().all(|i| i.matrix.major_axis == 1)
    {
        cached_major_axis = 1;
        work_units.sort_by_key(|wu| (wu.chunk_indices[1], wu.chunk_indices[0]));
    }

    Ok(ReadZarrBind {
        group_shape: group.shape.clone(),
        group_chunk_shape: group.chunk_shape.clone(),
        columns,
        coord_arrays,
        arrays,
        subset_reads,
        masks,
        sparse,
        sparse_blocks,
        sparse_major_axis,
        sparse_ranges: Mutex::new(HashMap::new()),
        cached_major_axis,
        work_units,
        next_unit: AtomicUsize::new(0),
    })
}

/// One data variable's decoded values for a work unit.
pub struct UnitValues {
    pub values: SharedColumnValues,
    /// `true` when the values are one whole chunk as `retrieve_chunk` returns
    /// it: laid out over the full chunk shape, padded past the array's edge.
    /// `false` for an array subset, laid out over the work unit's clipped
    /// region only.
    pub padded: bool,
}

/// The region of the array that work unit `wu` covers, clipped to `shape`.
fn unit_region(wu: &WorkUnit, shape: &[u64], chunk_shape: &[u64]) -> ArraySubset {
    let ranges: Vec<std::ops::Range<u64>> = (0..shape.len())
        .map(|k| {
            let start = wu.chunk_indices[k] * chunk_shape[k];
            start..(start + chunk_shape[k]).min(shape[k])
        })
        .collect();
    ArraySubset::new_with_ranges(&ranges)
}

/// The sparse matrices of a sparse table, in data-column order.
fn sparse_inputs<'a>(
    columns: &[ColumnDef],
    sparse: &'a HashMap<String, SparseInput>,
) -> Vec<&'a SparseInput> {
    columns
        .iter()
        .filter(|c| !c.is_coord)
        .map(|c| &sparse[&c.name])
        .collect()
}

/// About how many stored entries one work unit of a sparse table decodes.
const SPARSE_BLOCK_ENTRIES: u64 = 1 << 18;

fn decode_work_unit(
    bind: &ReadZarrBind,
    wu: &WorkUnit,
    projected: &HashMap<usize, usize>,
) -> Result<HashMap<String, UnitValues>, Box<dyn std::error::Error>> {
    let mut chunk_values = HashMap::new();
    let region = unit_region(wu, &bind.group_shape, &bind.group_chunk_shape);

    for (col_idx, col) in bind.columns.iter().enumerate() {
        if col.is_coord {
            continue; // coord data is pre-loaded at bind time
        }
        if !projected.contains_key(&col_idx) {
            continue; // skip decompression for non-projected data vars
        }
        if let Some(input) = bind.sparse.get(&col.name) {
            let origin: Vec<u64> = wu
                .chunk_indices
                .iter()
                .zip(&bind.group_chunk_shape)
                .map(|(i, c)| i * c)
                .collect();
            let range = sparse_range(bind, &col.name, input, &origin)?;
            let bytes = input.dense_chunk(&range, &origin, &bind.group_chunk_shape);
            let values =
                FixedValues::new(bytes, input.dtype.clone(), col.encoding.clone(), None)
                    .ok_or_else(|| format!("sparse matrix '{}' is not fixed-width", col.name))?;
            // dense_chunk lays the values out like retrieve_chunk: over the
            // full plan chunk, padded past the array's edge.
            chunk_values.insert(
                col.name.clone(),
                UnitValues {
                    values: Arc::new(values),
                    padded: true,
                },
            );
            continue;
        }
        let arr = bind
            .arrays
            .get(&col.name)
            .ok_or_else(|| format!("array '{}' not found in bind cache", col.name))?;
        // An array chunked like the plan reads one chunk. An array chunked
        // differently reads the work unit's region, which may span several of
        // its chunks or part of one (decision 6).
        let padded = !bind.subset_reads.contains(&col.name);
        let data: SharedColumnValues = if col.on_disk_dtype == ZarrDtype::String {
            // Both calls fill missing (implicit) chunks with the dtype's
            // fill_value automatically, same as the fixed-width path below.
            let strings = if padded {
                arr.retrieve_chunk::<Vec<String>>(&wu.chunk_indices)?
            } else {
                arr.retrieve_array_subset::<Vec<String>>(&region)?
            };
            Arc::new(StringValues { strings })
        } else {
            // ArrayBytes<'static>: zarrs convention for requesting owned decoded bytes.
            let raw = if padded {
                arr.retrieve_chunk::<zarrs::array::ArrayBytes<'static>>(&wu.chunk_indices)?
            } else {
                arr.retrieve_array_subset::<zarrs::array::ArrayBytes<'static>>(&region)?
            };
            let bytes: Vec<u8> = raw
                .into_fixed()
                .map_err(|_| format!("variable-length dtype not supported for '{}'", col.name))?
                .into_owned();
            Arc::new(
                FixedValues::new(
                    bytes,
                    col.on_disk_dtype.clone(),
                    col.encoding.clone(),
                    col.sentinel.clone(),
                )
                .ok_or_else(|| format!("no fixed-width dtype for '{}'", col.name))?,
            )
        };
        // A nullable variable's mask marks missing values.
        let data = match bind.masks.get(&col.name) {
            Some(mask) => {
                let raw = if padded {
                    mask.retrieve_chunk::<zarrs::array::ArrayBytes<'static>>(&wu.chunk_indices)?
                } else {
                    mask.retrieve_array_subset::<zarrs::array::ArrayBytes<'static>>(&region)?
                };
                let mask = raw
                    .into_fixed()
                    .map_err(|_| format!("mask of '{}' is not a bool array", col.name))?
                    .into_owned();
                Arc::new(MaskedValues { values: data, mask }) as SharedColumnValues
            }
            None => data,
        };
        chunk_values.insert(
            col.name.clone(),
            UnitValues {
                values: data,
                padded,
            },
        );
    }

    Ok(chunk_values)
}

/// The decoded major range of sparse matrix `name` that the dense chunk at
/// `origin` lies in, from `bind.sparse_ranges` or else read now and kept for
/// the other work units across the same range.
fn sparse_range(
    bind: &ReadZarrBind,
    name: &str,
    input: &SparseInput,
    origin: &[u64],
) -> Result<Arc<MajorRange>, Box<dyn std::error::Error>> {
    let major = input.matrix.major_axis;
    let minor = 1 - major;
    let m0 = origin[major];
    let m1 = (m0 + bind.group_chunk_shape[major]).min(bind.group_shape[major]);
    // Stored the other way from the unit order, the units across one range
    // are spread over the whole scan, so caching it would keep every range of
    // the matrix in memory at once.
    if major != bind.cached_major_axis {
        return Ok(Arc::new(input.major_range(m0, m1)?));
    }
    let uses = bind.group_shape[minor].div_ceil(bind.group_chunk_shape[minor]);
    let key = (name.to_string(), m0);
    // Count one use of the cached range; drop it after the last.
    let take = |cache: &mut RangeCache| {
        let (range, left) = cache.get_mut(&key)?;
        let range = range.clone();
        *left -= 1;
        if *left == 0 {
            cache.remove(&key);
        }
        Some(range)
    };
    if let Some(range) = take(&mut bind.sparse_ranges.lock().unwrap()) {
        return Ok(range);
    }
    let range = Arc::new(input.major_range(m0, m1)?);
    if uses > 1 {
        let mut cache = bind.sparse_ranges.lock().unwrap();
        // Another thread may have read the same range meanwhile.
        if take(&mut cache).is_none() {
            cache.insert(key, (range.clone(), uses - 1));
        }
    }
    Ok(range)
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
    chunk_values: &HashMap<String, UnitValues>,
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
                    ca.data.write_element(&mut vector, coord_idx, dst);
                } else {
                    // Unindexed dim → synthesize range.
                    unsafe {
                        let slot = vector.as_mut_ptr::<i64>();
                        *slot.add(dst) = global_indices[dim_k] as i64;
                    }
                }
            } else {
                // Data variable: a whole chunk is indexed through the padded
                // (zarrs) strides; an array subset holds only the clipped region,
                // in row order.
                let unit = chunk_values.get(&col_def.name).unwrap_or_else(|| {
                    unreachable!(
                        "projected data variable '{}' missing from chunk_values",
                        col_def.name
                    )
                });
                let idx = if unit.padded { zarrs_flat } else { flat_row };
                unit.values.write_element(&mut vector, idx, dst);
            }
        }
    }

    n_rows
}

/// Write rows `row_start..row_start + n_rows` of a sparse table's block into
/// the output, starting at output row `vector_base`.
fn fill_sparse_rows(
    bind: &ReadZarrBind,
    block: &SparseBlock,
    output: &mut DataChunkHandle,
    vector_base: usize,
    row_start: usize,
    n_rows: usize,
    projected: &HashMap<usize, usize>,
) {
    let major_axis = bind.sparse_major_axis;
    let mut var_idx = 0usize;
    for (col_idx, col_def) in bind.columns.iter().enumerate() {
        let this_var = (!col_def.is_coord).then(|| {
            var_idx += 1;
            var_idx - 1
        });
        let Some(&out_vec_idx) = projected.get(&col_idx) else {
            continue;
        };
        let mut vector = output.flat_vector(out_vec_idx);
        let rows = row_start..row_start + n_rows;
        if let Some(dim_k) = col_def.dim_idx {
            let indices = if dim_k == major_axis {
                &block.major[rows]
            } else {
                &block.minor[rows]
            };
            match bind.coord_arrays.get(&col_def.name) {
                Some(ca) => {
                    for (i, &index) in indices.iter().enumerate() {
                        ca.data
                            .write_element(&mut vector, index as usize, vector_base + i);
                    }
                }
                None => {
                    let slot = unsafe { vector.as_mut_ptr::<i64>() };
                    for (i, &index) in indices.iter().enumerate() {
                        unsafe { *slot.add(vector_base + i) = index as i64 };
                    }
                }
            }
        } else if let Some(Some(values)) = this_var.map(|v| &block.values[v]) {
            let size = col_def
                .on_disk_dtype
                .byte_size()
                .expect("sparse data has a fixed-width dtype");
            for (i, row) in rows.enumerate() {
                crate::zarr_reader::scan::fill_scalar_element_pub(
                    &mut vector,
                    values,
                    &col_def.on_disk_dtype,
                    &None,
                    row,
                    size,
                    vector_base + i,
                );
            }
        }
    }
}
