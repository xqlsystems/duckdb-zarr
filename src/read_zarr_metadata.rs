use std::sync::atomic::{AtomicUsize, Ordering};

use duckdb::core::LogicalTypeId;
use duckdb::vtab::{BindInfo, InitInfo, TableFunctionInfo, VTab};

use crate::zarr_reader::meta::{
    array_path_dims, cf_auxiliary_vars, dimension_names as get_dim_names, extract_file_system,
    is_unsupported_array_error, list_array_names, open_array, open_store, select_array_name,
    ArrayFacts,
};

/// One metadata row per array.
#[derive(Debug, Clone)]
struct MetaRow {
    name: String,
    dims: String, // JSON array string e.g. '["lat","lon"]'
    dtype: String,
    shape: String, // JSON array string e.g. '[4,6]'
    chunk_shape: String,
    attrs: String, // full attrs as JSON string; for "unsupported", {"error": ...}
    role: String,  // "coord" | "data" | "aux_coord" | "bounds" | "scalar" | "unsupported"
    // Dimension names that read_zarr(store, array_path := name) binds. Differs from
    // `dims` only when the array declares no names (then dim_0, dim_1, ...).
    array_path_dims: String,
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
        bind.add_result_column("dims", LogicalTypeId::Varchar.into());
        bind.add_result_column("dtype", LogicalTypeId::Varchar.into());
        bind.add_result_column("shape", LogicalTypeId::Varchar.into());
        bind.add_result_column("chunk_shape", LogicalTypeId::Varchar.into());
        bind.add_result_column("attrs", LogicalTypeId::Varchar.into());
        bind.add_result_column("role", LogicalTypeId::Varchar.into());
        bind.add_result_column("array_path_dims", LogicalTypeId::Varchar.into());

        let store_path = bind.get_parameter(0).to_string();
        let fs = unsafe { extract_file_system(bind) };
        let store = open_store(&store_path, Some(fs))?;
        let mut array_names = list_array_names(&store_path, &store)?;
        let array_path = bind
            .get_named_parameter("array_path")
            .map(|value| value.to_string());
        let array_alias = bind
            .get_named_parameter("array")
            .map(|value| value.to_string());
        if array_path.is_some() && array_alias.is_some() {
            return Err("use either array_path= or \"array\"=, not both".into());
        }
        let requested_group = crate::zarr_reader::tree::group_param(bind)?;
        let requested_array = array_path.or(array_alias);
        if requested_group.is_some() && requested_array.is_some() {
            return Err("use either group_path= or array_path=, not both".into());
        }
        if let Some(requested) = requested_array {
            array_names = vec![select_array_name(&array_names, &requested)?];
        }
        // group_path= lists the arrays directly in that group, not in its subgroups.
        if let Some(node) = requested_group {
            crate::zarr_reader::tree::ensure_group_exists(&store_path, &array_names, &node)?;
            array_names.retain(|name| crate::zarr_reader::tree::node_of(name) == node);
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
        let facts: Vec<ArrayFacts> = opened
            .iter()
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
                        dims: "[]".to_string(),
                        dtype: "unsupported".to_string(),
                        shape: "[]".to_string(),
                        chunk_shape: "[]".to_string(),
                        attrs: serde_json::json!({ "error": err }).to_string(),
                        role: "unsupported".to_string(),
                        array_path_dims: "[]".to_string(),
                    });
                    continue;
                }
            };
            let shape = arr.shape().to_vec();
            // chunk_grid_shape() returns number-of-chunks per dim, NOT element shape.
            // Use chunk_shape([0,0,...]) to get the actual per-chunk element dimensions.
            let chunk_shape: Vec<u64> = if !shape.is_empty() {
                let first = vec![0u64; shape.len()];
                arr.chunk_shape(&first)
                    .map(|cs| cs.iter().map(|x| x.get()).collect())
                    .unwrap_or_default()
            } else {
                Vec::new()
            };

            let dims = get_dim_names(arr, name).unwrap_or_default();
            let bound_dims = array_path_dims(&store, arr, name);
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
                dims: serde_json::to_string(&dims).unwrap_or_default(),
                dtype: dtype_str,
                shape: serde_json::to_string(&shape).unwrap_or_default(),
                chunk_shape: serde_json::to_string(&chunk_shape).unwrap_or_default(),
                attrs: serde_json::to_string(&attrs).unwrap_or_default(),
                role: role.to_string(),
                array_path_dims: serde_json::to_string(&bound_dims).unwrap_or_default(),
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

        let v_name = output.flat_vector(0);
        let v_dims = output.flat_vector(1);
        let v_dtype = output.flat_vector(2);
        let v_shape = output.flat_vector(3);
        let v_cshape = output.flat_vector(4);
        let v_attrs = output.flat_vector(5);
        let v_role = output.flat_vector(6);
        let v_bound = output.flat_vector(7);

        for (i, row) in bind.rows[start..end].iter().enumerate() {
            use duckdb::core::Inserter;
            v_name.insert(i, row.name.as_str());
            v_dims.insert(i, row.dims.as_str());
            v_dtype.insert(i, row.dtype.as_str());
            v_shape.insert(i, row.shape.as_str());
            v_cshape.insert(i, row.chunk_shape.as_str());
            v_attrs.insert(i, row.attrs.as_str());
            v_role.insert(i, row.role.as_str());
            v_bound.insert(i, row.array_path_dims.as_str());
        }

        output.set_len(n);
        Ok(())
    }

    fn parameters() -> Option<Vec<duckdb::core::LogicalTypeHandle>> {
        Some(vec![LogicalTypeId::Varchar.into()])
    }

    fn named_parameters() -> Option<Vec<(String, duckdb::core::LogicalTypeHandle)>> {
        let mut params = vec![
            ("array".to_string(), LogicalTypeId::Varchar.into()),
            ("array_path".to_string(), LogicalTypeId::Varchar.into()),
        ];
        params.extend(crate::zarr_reader::tree::group_named_parameters());
        Some(params)
    }
}
