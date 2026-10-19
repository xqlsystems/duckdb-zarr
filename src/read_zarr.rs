use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use duckdb::core::{DataChunkHandle, LogicalTypeHandle, LogicalTypeId};
use duckdb::vtab::{BindInfo, InitInfo, TableFunctionInfo, VTab, Value};
use zarrs::array::ArraySubset;

use crate::selection::Selection;
use crate::zarr_reader::anndata;
use crate::zarr_reader::meta::{
    array_path_table, build_column_defs, build_work_units, extract_file_system, first_chunk_shape,
    load_coord_array, open_array, open_store, ZarrArray, ZarrStore,
};
use crate::zarr_reader::sparse::{decode_block, plan_blocks, MajorRange, SparseBlock, SparseInput};
use crate::zarr_reader::tree::{self, StoreTree};
use crate::zarr_reader::types::{
    ColumnDef, CoordArray, FixedValues, MaskedValues, SharedColumnValues, StringValues, Table,
    WorkUnit, ZarrDtype,
};

// ---------------------------------------------------------------------------
// BindData — shared, immutable, produced once per query.
// ---------------------------------------------------------------------------

/// Decoded major ranges of sparse matrices, by (variable, range start), with
/// the number of work units that have yet to use each.
type RangeCache = HashMap<(String, u64), (Arc<MajorRange>, u64)>;

pub struct ReadZarrBind {
    pub table_shape: Vec<u64>,
    pub table_chunk_shape: Vec<u64>,
    pub columns: Vec<ColumnDef>,
    pub coord_arrays: HashMap<String, CoordArray>,
    /// Pre-opened data-variable arrays; avoids O(n_chunks × n_vars) metadata reads.
    pub arrays: HashMap<String, ZarrArray>,
    /// Pre-opened `mask` arrays of nullable variables, keyed by variable name.
    pub masks: HashMap<String, ZarrArray>,
    /// Sparse matrices, keyed by variable name.
    pub sparse: HashMap<String, SparseInput>,
    pub plan: ScanPlan,
    pub next_unit: AtomicUsize,
}

/// How a table's rows are produced. A table's rows are the cells its
/// variables store (decision 9): every cell of its shape, unless all its
/// variables are sparse matrices, which store only their entries.
pub enum ScanPlan {
    /// One row per cell, in work units of one plan-grid cell each.
    Cells(CellScan),
    /// A table of sparse matrices only: one row per stored entry, in work
    /// units of one range of the matrices' major axis each.
    StoredEntries {
        blocks: Vec<(u64, u64)>,
        /// The axis that `indptr` indexes (0 for CSR, 1 for CSC), the same for
        /// every matrix of the table.
        major_axis: usize,
    },
}

/// The work of a [`ScanPlan::Cells`] scan.
pub struct CellScan {
    pub work_units: Vec<WorkUnit>,
    /// Data variables whose chunk shape differs from the table's plan grid.
    /// They are read as an array subset per work unit instead of one chunk.
    pub subset_reads: HashSet<String>,
    /// For sparse matrices in the table: decoded ranges of a matrix's major
    /// axis, shared by the work units across each range so that its entries
    /// are read once, not once per chunk of the minor axis. Keyed by variable
    /// and range start; the count is how many work units have yet to use the
    /// range, and the entry is dropped after the last one.
    pub sparse_ranges: Mutex<RangeCache>,
    /// The outer axis of the work-unit order. Only matrices with this major
    /// axis use `sparse_ranges`.
    pub cached_major_axis: usize,
}

impl ReadZarrBind {
    fn n_units(&self) -> usize {
        match &self.plan {
            ScanPlan::Cells(cells) => cells.work_units.len(),
            ScanPlan::StoredEntries { blocks, .. } => blocks.len(),
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
        // Optional table_name= named parameter (quoted alias "table"=; TABLE
        // is a DuckDB keyword): a table's name as read_zarr_tables gives it,
        // e.g. read_zarr(store, table_name := 'time_lat_lon'). Selects the same
        // table as its dims= list.
        let requested_table = match (
            bind.get_named_parameter("table_name"),
            bind.get_named_parameter("table"),
        ) {
            (Some(_), Some(_)) => {
                return Err("use either table_name= or \"table\"=, not both".into())
            }
            (name, alias) => name.or(alias).map(|value| value.to_string()),
        };
        if requested_table.is_some() && requested_dims.is_some() {
            return Err("use either table_name= or dims=, not both".into());
        }
        let selection = Selection::from_bind(bind)?;
        if requested_table.is_some() && matches!(selection, Selection::Array(_)) {
            return Err("use either table_name= or array_path=, not both".into());
        }

        // Mirrors xarray.open_zarr's decode_times=: on by default, opt out to see
        // the raw CF offsets instead of TIMESTAMPs.
        let decode_times = bind
            .get_named_parameter("decode_times")
            .map(|value| value.is_null() || value.to_bool())
            .unwrap_or(true);

        let fs = unsafe { extract_file_system(bind) };
        let store = open_store(&store_path, Some(fs))?;
        let node = match selection {
            Selection::Array(requested) => {
                // Only this one array is needed. Listing requires consolidated
                // metadata on remote stores, so here it is best-effort: when
                // available it lets coordinate arrays be resolved; when not (e.g.
                // an OME-Zarr store served over HTTP without consolidation) the
                // array still reads by array_path, with dimensions synthesized as
                // integer indices.
                let array_names = crate::zarr_reader::meta::list_array_names(&store_path, &store)
                    .unwrap_or_default();
                let (array_name, table) = array_path_table(&store, &array_names, &requested)?;
                if let Some(dims) = requested_dims {
                    if table.dims != dims {
                        return Err(format!(
                            "array '{array_name}' has dimensions {:?}, not {dims:?}",
                            table.dims
                        )
                        .into());
                    }
                }
                return finish_bind(bind, store, &table, decode_times, ColumnNames::ArrayPath);
            }
            // One node (Zarr group) of the store, the root unless group_path=
            // says otherwise, as in xarray.open_zarr (design decision 8).
            Selection::Node(node) => node,
            Selection::Store => String::new(),
        };

        let array_names = crate::zarr_reader::meta::list_array_names(&store_path, &store)?;
        if array_names.is_empty() {
            return Err(format!("no Zarr arrays found in '{store_path}'").into());
        }

        let shown = tree::display_group(&node);
        tree::ensure_group_exists(&store_path, &array_names, &node)?;
        let store_tree = StoreTree::load_for_node(&store, &array_names, &node)?;
        anndata::check_group_path(&store_path, store_tree.layout.as_ref(), &node)?;
        let node_tables = store_tree.tables(&node);
        if let Some(msg) = node_tables.misaligned {
            return Err(msg.into());
        }
        let tables = &node_tables.tables;

        if tables.is_empty() {
            let mut msg = format!("'{store_path}': no data variables in group '{shown}'");
            if !node_tables.unnamed.is_empty() {
                msg.push_str(&format!(
                    "; these arrays declare no dimension names: {:?}. Read one with \
                     array_path=, which names its dimensions dim_0, dim_1 and so on",
                    node_tables.unnamed
                ));
            }
            msg.push_str(&format!(
                ". Use group_path= to read another group; read_zarr_tables('{store_path}') lists \
                 the tables in every group"
            ));
            return Err(msg.into());
        }

        let table = match &requested_table {
            Some(name) => table_named(tables, name).ok_or_else(|| {
                format!(
                    "'{store_path}': group '{shown}' has no table named '{name}'; its tables \
                     are {:?}. A table without a name (NULL in read_zarr_tables) is read with \
                     dims=",
                    tree::table_names(tables)
                        .into_iter()
                        .flatten()
                        .collect::<Vec<_>>()
                )
            })?,
            None => table_with_dims(tables, requested_dims.as_deref(), &store_path, &shown)?,
        };
        let wanted = &table.dims;

        // Leaving an array out would return the table without it and no sign
        // that it is missing.
        let unreadable = table.unreadable();
        if let Some(&(first, err)) = unreadable.first() {
            let names: Vec<&str> = unreadable.iter().map(|&(p, _)| p).collect();
            return Err(format!(
                "'{store_path}': zarrs cannot open {names:?}, which belong to the table \
                 {wanted:?} in group '{shown}' ('{first}': {err}); read the other arrays one \
                 at a time with array_path="
            )
            .into());
        }
        finish_bind(bind, store, table, decode_times, ColumnNames::Basename)
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
                let cells = match &bind.plan {
                    ScanPlan::Cells(cells) => cells,
                    ScanPlan::StoredEntries { blocks, .. } => {
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
                };
                let wu = &cells.work_units[unit_idx];
                // Decode chunk for each data variable.
                let chunk_values = decode_work_unit(bind, cells, wu, projected)?;
                let chunk_rows = compute_chunk_rows(wu, &bind.table_shape, &bind.table_chunk_shape);
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

            let cells = match &bind.plan {
                ScanPlan::Cells(cells) => cells,
                &ScanPlan::StoredEntries { major_axis, .. } => {
                    fill_sparse_rows(
                        bind,
                        major_axis,
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
            };

            let wu = &cells.work_units[state.current_unit_idx];

            // fill_output_chunk writes into output starting at rows_written.
            // It reads from the chunk starting at row_cursor.
            let written = fill_chunk_slice(
                &bind.columns,
                &bind.coord_arrays,
                wu,
                &bind.table_shape,
                &bind.table_chunk_shape,
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
            ("table_name".to_string(), LogicalTypeId::Varchar.into()),
            ("table".to_string(), LogicalTypeId::Varchar.into()),
            ("decode_times".to_string(), LogicalTypeId::Boolean.into()),
        ];
        params.extend(Selection::named_parameters());
        Some(params)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// The table of a node that `name` names, as `read_zarr_tables` names it
/// (DuckDB identifiers ignore case).
fn table_named<'a>(tables: &'a [Table], name: &str) -> Option<&'a Table> {
    let names = tree::table_names(tables);
    tables
        .iter()
        .zip(names)
        .find(|(_, n)| {
            n.as_deref()
                .is_some_and(|n| tree::fold_case(n) == tree::fold_case(name))
        })
        .map(|(table, _)| table)
}

/// The table of a node with dimensions `requested`, or the node's only table
/// when none are requested.
fn table_with_dims<'a>(
    tables: &'a [Table],
    requested: Option<&[String]>,
    store_path: &str,
    shown: &str,
) -> Result<&'a Table, Box<dyn std::error::Error>> {
    let available = || {
        let mut dims: Vec<&Vec<String>> = tables.iter().map(|t| &t.dims).collect();
        dims.dedup();
        dims
    };
    let wanted: &[String] = match requested {
        Some(dims) => dims,
        None => {
            let distinct = available();
            if distinct.len() > 1 {
                return Err(format!(
                    "'{store_path}': group '{shown}' has {} tables {distinct:?}; use table_name= \
             or dims= to select one, or array_path= to read one array",
                    distinct.len()
                )
                .into());
            }
            &tables[0].dims
        }
    };
    let matching: Vec<&Table> = tables.iter().filter(|t| t.dims == wanted).collect();
    let table = match matching.as_slice() {
        [] => {
            return Err(format!(
                "'{store_path}': no table in group '{shown}' has dims={wanted:?}; \
             available: {:?}",
                available()
            )
            .into())
        }
        [table] => *table,
        [a, b, ..] => {
            return Err(format!(
                "arrays of different shapes share dims {wanted:?}: {:?} {:?} vs {:?} {:?}; \
             use array_path= to read one array",
                a.variable_paths(),
                a.shape,
                b.variable_paths(),
                b.shape
            )
            .into());
        }
    };
    Ok(table)
}

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
    /// The variable's name within its node, as in xarray (`foo`, not `a/b/foo`).
    Basename,
    /// One array selected by `array_path=`: its store-relative path.
    ArrayPath,
}

fn finish_bind(
    bind: &BindInfo,
    store: ZarrStore,
    table: &Table,
    decode_times: bool,
    names: ColumnNames,
) -> Result<ReadZarrBind, Box<dyn std::error::Error>> {
    if table.mixes_sparse_storage() {
        let paths = table.variable_paths();
        return Err(format!(
            "sparse matrices {paths:?} are stored partly as CSR and partly as CSC, so their \
             stored entries cannot be read as one table; read each alone with array_path := \
             '{}'",
            paths[0]
        )
        .into());
    }
    // Load each coordinate array once, even when two dimensions share it
    // (obsp's obs_i and obs_j are both obs/_index); clones share the values.
    let mut coord_arrays: HashMap<String, CoordArray> = HashMap::new();
    let mut loaded: HashMap<&str, CoordArray> = HashMap::new();
    for (dim, coord_name) in &table.coords {
        let ca = match loaded.get(coord_name.as_str()) {
            Some(ca) => ca.clone(),
            None => {
                let ca = load_coord_array(&store, coord_name, decode_times)?;
                loaded.insert(coord_name, ca.clone());
                ca
            }
        };
        coord_arrays.insert(dim.clone(), ca);
    }

    let columns = build_column_defs(&store, table, &coord_arrays, decode_times)?;

    // Register output columns with DuckDB. A table read names each data column
    // after the variable's name within its node. When a single array's name is a
    // numeric level (`0`) or, for array_path=, a nested store-relative path
    // (`labels/nuclei/0`), surface that value column as `value` so callers don't
    // have to double-quote it. Decoding still keys off `col.name`.
    let single_data_var = columns.iter().filter(|c| !c.is_coord).count() == 1;
    // DuckDB column names ignore case and must be unique. A data column whose
    // name equals, ignoring case, a dimension of the table (an AnnData obs
    // column called `obs` or `Obs`) or an earlier column (`A` and `a`) keeps its
    // store path instead (`obs/obs`), as array_path= names it.
    let mut used: HashSet<String> = table.dims.iter().map(|d| tree::fold_case(d)).collect();
    for col in &columns {
        let duckdb_type = col.on_disk_dtype.to_duckdb_type(&col.encoding);
        let basename = tree::basename(&col.name);
        let collides = matches!(names, ColumnNames::Basename)
            && !col.is_coord
            && used.contains(&tree::fold_case(basename));
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
        if collides && used.contains(&tree::fold_case(&display_name)) {
            display_name = (1..)
                .map(|n| format!("{name}_{n}"))
                .find(|candidate| !used.contains(&tree::fold_case(candidate)))
                .expect("an unused suffix exists");
        }
        if !col.is_coord {
            used.insert(tree::fold_case(&display_name));
        }
        bind.add_result_column(&display_name, duckdb_type);
    }

    // Pre-open data variable arrays once at bind time.
    let mut arrays: HashMap<String, ZarrArray> = HashMap::new();
    let mut masks: HashMap<String, ZarrArray> = HashMap::new();
    let mut sparse: HashMap<String, SparseInput> = HashMap::new();
    let mut subset_reads = HashSet::new();
    for col in columns.iter().filter(|c| !c.is_coord) {
        if let Some(matrix) = col.storage.sparse() {
            sparse.insert(
                col.name.clone(),
                SparseInput::open(&store, &col.name, matrix)?,
            );
            continue;
        }
        let source = col
            .storage
            .values_array(&col.name)
            .ok_or_else(|| format!("'{}' has no array to read", col.name))?;
        let arr = open_array(&store, source)?;
        let mut chunked_like_plan = first_chunk_shape(&arr)? == table.chunk_shape;
        arrays.insert(col.name.clone(), arr);
        if let Some(mask) = col.storage.mask() {
            let mask = open_array(&store, mask)?;
            // A mask is read the same way as its values, so both share a layout.
            chunked_like_plan &= first_chunk_shape(&mask)? == table.chunk_shape;
            masks.insert(col.name.clone(), mask);
        }
        if !chunked_like_plan {
            subset_reads.insert(col.name.clone());
        }
    }

    let plan = if table.is_sparse() {
        // One row per stored entry, read in blocks of the major axis. Every
        // matrix is stored the same way (mixes_sparse_storage, above).
        let inputs = sparse_inputs(&columns, &sparse);
        ScanPlan::StoredEntries {
            blocks: plan_blocks(&inputs, SPARSE_BLOCK_ENTRIES),
            major_axis: inputs[0].matrix.major_axis,
        }
    } else {
        ScanPlan::Cells(cell_scan(table, &sparse, subset_reads))
    };

    Ok(ReadZarrBind {
        table_shape: table.shape.clone(),
        table_chunk_shape: table.chunk_shape.clone(),
        columns,
        coord_arrays,
        arrays,
        masks,
        sparse,
        plan,
        next_unit: AtomicUsize::new(0),
    })
}

/// The work units of a one-row-per-cell scan of `table`, ordered so that the
/// decoded ranges of its sparse matrices can be shared (see below).
fn cell_scan(
    table: &Table,
    sparse: &HashMap<String, SparseInput>,
    subset_reads: HashSet<String>,
) -> CellScan {
    let mut work_units = build_work_units(table);
    // Work units run roughly in order, and the decoded range of a sparse matrix
    // is kept until every unit across it has run (`sparse_ranges`). That bounds
    // memory to a few ranges only for a matrix whose major axis is the outer
    // axis of the unit order. build_work_units puts axis 0 outermost, which
    // suits CSR; when every sparse matrix of the table is CSC, put axis 1
    // outermost instead. A matrix stored the other way from the order is read
    // per work unit without the cache.
    let mut cached_major_axis = 0;
    if !sparse.is_empty() && sparse.values().all(|i| i.matrix.major_axis == 1) {
        cached_major_axis = 1;
        work_units.sort_by_key(|wu| (wu.chunk_indices[1], wu.chunk_indices[0]));
    }
    CellScan {
        work_units,
        subset_reads,
        sparse_ranges: Mutex::new(HashMap::new()),
        cached_major_axis,
    }
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
    cells: &CellScan,
    wu: &WorkUnit,
    projected: &HashMap<usize, usize>,
) -> Result<HashMap<String, UnitValues>, Box<dyn std::error::Error>> {
    let mut chunk_values = HashMap::new();
    let region = unit_region(wu, &bind.table_shape, &bind.table_chunk_shape);

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
                .zip(&bind.table_chunk_shape)
                .map(|(i, c)| i * c)
                .collect();
            let range = sparse_range(bind, cells, &col.name, input, &origin)?;
            let bytes = input.dense_chunk(&range, &origin, &bind.table_chunk_shape);
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
        let padded = !cells.subset_reads.contains(&col.name);
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
/// `origin` lies in, from `cells.sparse_ranges` or else read now and kept for
/// the other work units across the same range.
fn sparse_range(
    bind: &ReadZarrBind,
    cells: &CellScan,
    name: &str,
    input: &SparseInput,
    origin: &[u64],
) -> Result<Arc<MajorRange>, Box<dyn std::error::Error>> {
    let major = input.matrix.major_axis;
    let minor = 1 - major;
    let m0 = origin[major];
    let m1 = (m0 + bind.table_chunk_shape[major]).min(bind.table_shape[major]);
    // Stored the other way from the unit order, the units across one range
    // are spread over the whole scan, so caching it would keep every range of
    // the matrix in memory at once.
    if major != cells.cached_major_axis {
        return Ok(Arc::new(input.major_range(m0, m1)?));
    }
    let uses = bind.table_shape[minor].div_ceil(bind.table_chunk_shape[minor]);
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
    if let Some(range) = take(&mut cells.sparse_ranges.lock().unwrap()) {
        return Ok(range);
    }
    let range = Arc::new(input.major_range(m0, m1)?);
    if uses > 1 {
        let mut cache = cells.sparse_ranges.lock().unwrap();
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
    table_shape: &[u64],
    table_chunk_shape: &[u64],
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
            let origin = wu.chunk_indices[k] * table_chunk_shape[k];
            let remaining = table_shape[k] - origin;
            remaining.min(table_chunk_shape[k]) as usize
        })
        .collect();

    let chunk_origin: Vec<usize> = (0..ndim)
        .map(|k| (wu.chunk_indices[k] * table_chunk_shape[k]) as usize)
        .collect();

    // Logical strides: map flat_row → per-dim indices within the logical chunk.
    let mut strides = vec![1usize; ndim];
    for k in (0..ndim.saturating_sub(1)).rev() {
        strides[k] = strides[k + 1] * chunk_shape[k + 1];
    }

    // Physical (zarrs) strides: zarrs always returns full-chunk-size bytes, including
    // fill-value padding for boundary chunks. Use table_chunk_shape for byte offset math.
    let zarrs_shape: Vec<usize> = (0..ndim).map(|k| table_chunk_shape[k] as usize).collect();
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
#[allow(clippy::too_many_arguments)]
fn fill_sparse_rows(
    bind: &ReadZarrBind,
    major_axis: usize,
    block: &SparseBlock,
    output: &mut DataChunkHandle,
    vector_base: usize,
    row_start: usize,
    n_rows: usize,
    projected: &HashMap<usize, usize>,
) {
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
