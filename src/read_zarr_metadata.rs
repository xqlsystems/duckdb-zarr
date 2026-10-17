use std::sync::atomic::{AtomicUsize, Ordering};

use duckdb::core::LogicalTypeId;
use duckdb::vtab::{BindInfo, InitInfo, TableFunctionInfo, VTab};

use crate::list_columns::{ubigint_list, varchar_list, write_string_lists, write_u64_lists};
use crate::selection::Selection;
use crate::zarr_reader::meta::{
    array_path_dims, cf_auxiliary_vars, dimension_names as get_dim_names, extract_file_system,
    first_chunk_shape, is_unsupported_array_error, list_array_names, open_array, open_store,
    select_array_name, ArrayFacts,
};
use crate::zarr_reader::{anndata, tree};

/// One metadata row per array.
#[derive(Debug, Clone)]
struct MetaRow {
    name: String,
    dims: Vec<String>,
    dtype: String,
    shape: Vec<u64>,
    chunk_shape: Vec<u64>,
    attrs: String, // full attrs as JSON string; for "unsupported", {"error": ...}
    role: String,  // "coord" | "data" | "aux_coord" | "bounds" | "scalar" | "unsupported"
    // Dimension names that read_zarr(store, array_path := name) binds. Differs from
    // `dims` only when the array declares no names (then dim_0, dim_1, ...).
    array_path_dims: Vec<String>,
}

pub struct ReadZarrMetaBind {
    rows: Vec<MetaRow>,
    next: AtomicUsize,
}

unsafe impl Send for ReadZarrMetaBind {}
unsafe impl Sync for ReadZarrMetaBind {}

/// InitData carries no state; pagination is driven entirely by `ReadZarrMetaBind::next`.
pub struct ReadZarrMetaInit;

unsafe impl Send for ReadZarrMetaInit {}
unsafe impl Sync for ReadZarrMetaInit {}

pub struct ReadZarrMetaVTab;

impl VTab for ReadZarrMetaVTab {
    type BindData = ReadZarrMetaBind;
    type InitData = ReadZarrMetaInit;

    fn bind(bind: &BindInfo) -> Result<Self::BindData, Box<dyn std::error::Error>> {
        bind.add_result_column("name", LogicalTypeId::Varchar.into());
        bind.add_result_column("dims", varchar_list());
        bind.add_result_column("dtype", LogicalTypeId::Varchar.into());
        bind.add_result_column("shape", ubigint_list());
        bind.add_result_column("chunk_shape", ubigint_list());
        bind.add_result_column("attrs", LogicalTypeId::Varchar.into());
        bind.add_result_column("role", LogicalTypeId::Varchar.into());
        bind.add_result_column("array_path_dims", varchar_list());

        let store_path = bind.get_parameter(0).to_string();
        let fs = unsafe { extract_file_system(bind) };
        let store = open_store(&store_path, Some(fs))?;
        let mut array_names = list_array_names(&store_path, &store)?;
        // The AnnData layout (None for other stores), read once for all arrays.
        let layout = anndata::layout(&store, &array_names);
        match Selection::from_bind(bind)? {
            Selection::Array(requested) => {
                // An AnnData encoded variable lists the arrays it is made of.
                let is_variable = layout.as_ref().is_some_and(|layout| {
                    layout
                        .encoding_of(&requested)
                        .is_some_and(|e| anndata::is_variable_encoding(&e))
                });
                if is_variable {
                    let prefix = format!("{requested}/");
                    array_names.retain(|name| name.starts_with(&prefix));
                } else {
                    array_names = vec![select_array_name(&array_names, &requested)?];
                }
            }
            // group_path= lists the arrays of that node's tables, not of its
            // subgroups. In an AnnData store that is the node read_zarr reads
            // each array's variable from: the obs and var columns are in the
            // root, and the arrays of an encoded group (X/data,
            // obs/cell_type/codes) go with it.
            Selection::Node(node) => {
                tree::ensure_group_exists(&store_path, &array_names, &node)?;
                anndata::check_group_path(&store_path, layout.as_ref(), &node)?;
                match &layout {
                    Some(layout) => array_names.retain(|name| layout.node_of_array(name) == node),
                    None => array_names.retain(|name| tree::node_of(name) == node),
                }
            }
            Selection::Store => {}
        }

        // Open every array once. One with a data type or codec that zarrs cannot
        // open must not hide the rest of the store, so it is listed as
        // "unsupported"; any other error (I/O, auth, missing metadata) fails.
        let mut opened = Vec::with_capacity(array_names.len());
        for name in &array_names {
            match open_array(&store, name) {
                Ok(arr) => opened.push((name, Ok(arr))),
                Err(err) if is_unsupported_array_error(err.as_ref()) => {
                    opened.push((name, Err(err.to_string())))
                }
                Err(err) => return Err(err),
            }
        }
        // As in StoreTree::tables, CF rules do not apply to AnnData objects.
        let in_anndata = |name: &str| {
            layout
                .as_ref()
                .is_some_and(|l| l.object_of(tree::node_of(name)).is_some())
        };
        let facts: Vec<ArrayFacts> = opened
            .iter()
            .filter(|(name, _)| !in_anndata(name))
            .filter_map(|(name, arr)| {
                let arr = arr.as_ref().ok()?;
                Some((name.as_str(), arr.attributes(), arr.shape()))
            })
            .collect();
        let (aux_coords, bounds_vars) = cf_auxiliary_vars(&facts);

        let mut rows = Vec::new();
        for &(name, ref arr) in &opened {
            let arr = match arr {
                Ok(arr) => arr,
                Err(err) => {
                    rows.push(MetaRow {
                        name: name.to_string(),
                        dims: Vec::new(),
                        dtype: "unsupported".to_string(),
                        shape: Vec::new(),
                        chunk_shape: Vec::new(),
                        attrs: serde_json::json!({ "error": err }).to_string(),
                        role: "unsupported".to_string(),
                        array_path_dims: Vec::new(),
                    });
                    continue;
                }
            };
            let shape = arr.shape().to_vec();
            let chunk_shape = first_chunk_shape(arr).unwrap_or_default();

            let dims = get_dim_names(arr, name).unwrap_or_default();
            let bound_dims = array_path_dims(&store, layout.as_ref(), arr, name);
            let dtype_str = arr.data_type().to_string();
            let attrs = arr.attributes().clone();

            let role = if bounds_vars.contains(name) {
                "bounds"
            } else if shape.is_empty() {
                "scalar"
            } else if aux_coords.contains(name) {
                "aux_coord"
            } else if shape.len() == 1
                && dims.len() == 1
                && dims[0] == *name.rsplit('/').next().unwrap_or(name)
            {
                "coord"
            } else {
                "data"
            };

            rows.push(MetaRow {
                name: name.clone(),
                dims,
                dtype: dtype_str,
                shape,
                chunk_shape,
                attrs: serde_json::to_string(&attrs).unwrap_or_default(),
                role: role.to_string(),
                array_path_dims: bound_dims,
            });
        }

        Ok(ReadZarrMetaBind {
            rows,
            next: AtomicUsize::new(0),
        })
    }

    fn init(_: &InitInfo) -> Result<Self::InitData, Box<dyn std::error::Error>> {
        Ok(ReadZarrMetaInit)
    }

    fn func(
        func: &TableFunctionInfo<Self>,
        output: &mut duckdb::core::DataChunkHandle,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let bind = func.get_bind_data();
        let vector_size = unsafe { duckdb::ffi::duckdb_vector_size() as usize };

        // Atomically claim the next batch of rows. fetch_add is safe for concurrent
        // calls; each call gets a unique [start, end) window into bind.rows.
        let start = bind.next.fetch_add(vector_size, Ordering::Relaxed);
        if start >= bind.rows.len() {
            output.set_len(0);
            return Ok(());
        }
        let end = (start + vector_size).min(bind.rows.len());
        let n = end - start;

        let rows = &bind.rows[start..end];
        let v_name = output.flat_vector(0);
        let v_dtype = output.flat_vector(2);
        let v_attrs = output.flat_vector(5);
        let v_role = output.flat_vector(6);
        for (i, row) in rows.iter().enumerate() {
            use duckdb::core::Inserter;
            v_name.insert(i, row.name.as_str());
            v_dtype.insert(i, row.dtype.as_str());
            v_attrs.insert(i, row.attrs.as_str());
            v_role.insert(i, row.role.as_str());
        }
        let strings = |f: fn(&MetaRow) -> &[String]| rows.iter().map(f).collect::<Vec<_>>();
        let numbers = |f: fn(&MetaRow) -> &[u64]| rows.iter().map(f).collect::<Vec<_>>();
        write_string_lists(output, 1, &strings(|r| &r.dims));
        write_u64_lists(output, 3, &numbers(|r| &r.shape));
        write_u64_lists(output, 4, &numbers(|r| &r.chunk_shape));
        write_string_lists(output, 7, &strings(|r| &r.array_path_dims));

        output.set_len(n);
        Ok(())
    }

    fn parameters() -> Option<Vec<duckdb::core::LogicalTypeHandle>> {
        Some(vec![LogicalTypeId::Varchar.into()])
    }

    fn named_parameters() -> Option<Vec<(String, duckdb::core::LogicalTypeHandle)>> {
        Some(Selection::named_parameters())
    }
}
