use std::sync::atomic::{AtomicUsize, Ordering};

use duckdb::core::LogicalTypeId;
use duckdb::vtab::{BindInfo, InitInfo, TableFunctionInfo, VTab};

use crate::list_columns::{ubigint_list, varchar_list, write_string_lists, write_u64_lists};
use crate::selection::Selection;
use crate::zarr_reader::anndata;
use crate::zarr_reader::meta::{
    array_path_table, extract_file_system, list_array_names, open_store,
};
use crate::zarr_reader::tree::{self, StoreTree};
use crate::zarr_reader::types::Table;

#[derive(Debug, Clone)]
pub(crate) struct TableRow {
    pub group_path: String,
    pub schema_name: String,
    /// `None` when `read_zarr(store, group_path := ..., dims := ...)` cannot read
    /// this row as one table, or its name collides with another table's after
    /// case folding (see [`tree::table_names`] and `check_alignment`).
    pub table_name: Option<String>,
    dims: Vec<String>,
    shape: Vec<u64>,
    chunk_shape: Vec<u64>,
    data_vars: Vec<String>,
    coord_vars: Vec<String>,
}

pub struct ReadZarrTablesBind {
    rows: Vec<TableRow>,
}

unsafe impl Send for ReadZarrTablesBind {}
unsafe impl Sync for ReadZarrTablesBind {}

pub struct ReadZarrTablesInit {
    /// The first row the next call emits. A DataTree store can have more
    /// tables than one output chunk holds, so rows go out in pages.
    next: AtomicUsize,
}

unsafe impl Send for ReadZarrTablesInit {}
unsafe impl Sync for ReadZarrTablesInit {}

pub struct ReadZarrTablesVTab;

impl VTab for ReadZarrTablesVTab {
    type BindData = ReadZarrTablesBind;
    type InitData = ReadZarrTablesInit;

    fn bind(bind: &BindInfo) -> Result<Self::BindData, Box<dyn std::error::Error>> {
        bind.add_result_column("group_path", LogicalTypeId::Varchar.into());
        bind.add_result_column("schema_name", LogicalTypeId::Varchar.into());
        bind.add_result_column("table_name", LogicalTypeId::Varchar.into());
        bind.add_result_column("dims", varchar_list());
        bind.add_result_column("shape", ubigint_list());
        bind.add_result_column("chunk_shape", ubigint_list());
        bind.add_result_column("data_vars", varchar_list());
        bind.add_result_column("coord_vars", varchar_list());

        let store_path = bind.get_parameter(0).to_string();
        let rows = catalog_rows(bind, &store_path, Selection::from_bind(bind)?)?;
        Ok(ReadZarrTablesBind { rows })
    }

    fn init(_: &InitInfo) -> Result<Self::InitData, Box<dyn std::error::Error>> {
        Ok(ReadZarrTablesInit {
            next: AtomicUsize::new(0),
        })
    }

    fn func(
        func: &TableFunctionInfo<Self>,
        output: &mut duckdb::core::DataChunkHandle,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let bind = func.get_bind_data();
        let init = func.get_init_data();

        let vector_size = unsafe { duckdb::ffi::duckdb_vector_size() as usize };
        let start = init.next.fetch_add(vector_size, Ordering::Relaxed);
        if start >= bind.rows.len() {
            output.set_len(0);
            return Ok(());
        }
        let end = (start + vector_size).min(bind.rows.len());
        let n = end - start;

        let rows = &bind.rows[start..end];
        let v_group = output.flat_vector(0);
        let v_schema = output.flat_vector(1);
        let mut v_table = output.flat_vector(2);
        for (i, row) in rows.iter().enumerate() {
            use duckdb::core::Inserter;
            v_group.insert(i, row.group_path.as_str());
            v_schema.insert(i, row.schema_name.as_str());
            match &row.table_name {
                Some(name) => v_table.insert(i, name.as_str()),
                None => v_table.set_null(i),
            }
        }
        let strings = |f: fn(&TableRow) -> &[String]| rows.iter().map(f).collect::<Vec<_>>();
        let numbers = |f: fn(&TableRow) -> &[u64]| rows.iter().map(f).collect::<Vec<_>>();
        write_string_lists(output, 3, &strings(|r| &r.dims));
        write_u64_lists(output, 4, &numbers(|r| &r.shape));
        write_u64_lists(output, 5, &numbers(|r| &r.chunk_shape));
        write_string_lists(output, 6, &strings(|r| &r.data_vars));
        write_string_lists(output, 7, &strings(|r| &r.coord_vars));

        output.set_len(n);
        Ok(())
    }

    fn parameters() -> Option<Vec<duckdb::core::LogicalTypeHandle>> {
        Some(vec![LogicalTypeId::Varchar.into()])
    }

    fn named_parameters() -> Option<Vec<(String, duckdb::core::LogicalTypeHandle)>> {
        // Optional selectors: one node or one array. Without either, every
        // node's tables are listed.
        Some(Selection::named_parameters())
    }
}

/// The rows `read_zarr_tables` lists for `selection` in the store at
/// `store_path`, in node order.
pub(crate) fn catalog_rows(
    bind: &BindInfo,
    store_path: &str,
    selection: Selection,
) -> Result<Vec<TableRow>, Box<dyn std::error::Error>> {
    let fs = unsafe { extract_file_system(bind) };
    let store = open_store(store_path, Some(fs))?;
    let array_names = list_array_names(store_path, &store)?;
    let row = |node: &str, table_name: Option<String>, table: &Table| TableRow {
        group_path: tree::display_group(node),
        schema_name: tree::schema_name(node),
        table_name,
        dims: table.dims.clone(),
        shape: table.shape.clone(),
        chunk_shape: table.chunk_shape.clone(),
        data_vars: table
            .variable_paths()
            .into_iter()
            .map(str::to_string)
            .collect(),
        coord_vars: table.coords.iter().map(|(_, path)| path.clone()).collect(),
    };

    let mut rows = Vec::new();
    let (store_tree, nodes) = match selection {
        Selection::Array(requested) => {
            // One array is not a table of its node, so it gets no table name.
            let (array_name, table) = array_path_table(&store, &array_names, &requested)?;
            let node = table.variables.first().map_or_else(
                || tree::node_of(&array_name).to_string(),
                |var| var.node.clone(),
            );
            rows.push(row(&node, None, &table));
            return Ok(rows);
        }
        Selection::Node(node) => {
            tree::ensure_group_exists(store_path, &array_names, &node)?;
            let store_tree = StoreTree::load_for_node(&store, &array_names, &node)?;
            anndata::check_group_path(store_path, store_tree.layout.as_ref(), &node)?;
            (store_tree, vec![node])
        }
        Selection::Store => {
            let store_tree = StoreTree::load(&store, &array_names, None)?;
            let nodes = store_tree.nodes();
            (store_tree, nodes)
        }
    };
    for node in nodes {
        // A row gets a table name only if read_zarr can read it as one table
        // (its node is aligned with its ancestors, no array of another
        // shape in the node shares its dims, zarrs can open all its arrays,
        // and its sparse matrices, if it has only those, are all CSR or all
        // CSC) and the name is unique in the node after case folding.
        let node_tables = store_tree.tables(&node);
        let aligned = node_tables.misaligned.is_none();
        let names = tree::table_names(&node_tables.tables);
        for (table, name) in node_tables.tables.iter().zip(names) {
            let readable =
                aligned && table.unreadable().is_empty() && !table.mixes_sparse_storage();
            rows.push(row(&node, name.filter(|_| readable), table));
        }
    }

    Ok(rows)
}
