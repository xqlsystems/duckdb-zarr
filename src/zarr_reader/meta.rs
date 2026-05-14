use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use base64::Engine as _;
use duckdb::ffi::{
    duckdb_client_context, duckdb_client_context_get_file_system, duckdb_destroy_client_context,
    duckdb_destroy_file_system, duckdb_file_system, duckdb_table_function_get_client_context,
};
use zarrs::array::Array;
use zarrs::filesystem::FilesystemStore;
use zarrs::storage::ReadableStorageTraits;

use super::duckdb_store::DuckDbStore;
use super::types::{
    ColumnDef, ColumnEncoding, CoordArray, DimGroup, FillSentinel, WorkUnit, ZarrDtype,
};

pub type ZarrStore = Arc<dyn ReadableStorageTraits>;
pub type ZarrArray = Array<dyn ReadableStorageTraits>;

/// Extract a DuckDB FileSystem handle from a table-function BindInfo.
///
/// The returned handle must be passed to [`open_store`], which either adopts it (remote stores)
/// or destroys it immediately (local / HTTP stores).
///
/// # Safety
/// Must be called from within a DuckDB table-function bind callback.
pub unsafe fn extract_file_system(bind: &duckdb::vtab::BindInfo) -> duckdb_file_system {
    // SAFETY: BindInfo is `struct BindInfo { ptr: duckdb_bind_info }` — one pointer-sized field,
    // no padding, offset 0. transmute_copy reads those pointer bytes as a duckdb_bind_info.
    // The assert catches any future duckdb-rs version that adds fields to BindInfo.
    const _: () = assert!(
        std::mem::size_of::<duckdb::vtab::BindInfo>()
            == std::mem::size_of::<duckdb::ffi::duckdb_bind_info>()
    );
    let raw: duckdb::ffi::duckdb_bind_info = std::mem::transmute_copy(bind);
    let mut ctx: duckdb_client_context = std::ptr::null_mut();
    duckdb_table_function_get_client_context(raw, &mut ctx);
    let fs = duckdb_client_context_get_file_system(ctx);
    duckdb_destroy_client_context(&mut ctx);
    fs
}

pub fn is_remote_scheme(path: &str) -> bool {
    let l = path.to_ascii_lowercase();
    l.starts_with("http://")
        || l.starts_with("https://")
        || l.starts_with("s3://")
        || l.starts_with("gs://")
        || l.starts_with("az://")
}

/// Open a Zarr store.
///
/// - HTTP/HTTPS → `zarrs_http::HTTPStore` (no DuckDB filesystem needed)
/// - S3/GCS/Azure → `DuckDbStore` backed by the provided `file_system` handle
///   (the store takes ownership and destroys it on drop)
/// - Local path → `zarrs::FilesystemStore` (destroys the handle if provided)
pub fn open_store(
    path: &str,
    file_system: Option<duckdb_file_system>,
) -> Result<ZarrStore, Box<dyn std::error::Error>> {
    let lower = path.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        if let Some(mut fs) = file_system {
            unsafe { duckdb_destroy_file_system(&mut fs) };
        }
        Ok(Arc::new(zarrs_http::HTTPStore::new(path)?))
    } else if lower.starts_with("s3://") || lower.starts_with("gs://") || lower.starts_with("az://") {
        let fs = file_system.ok_or(
            "remote store requires a DuckDB FileSystem handle (call from a table function bind)",
        )?;
        Ok(Arc::new(DuckDbStore::new(fs, path)))
    } else {
        if let Some(mut fs) = file_system {
            unsafe { duckdb_destroy_file_system(&mut fs) };
        }
        Ok(Arc::new(FilesystemStore::new(path)?))
    }
}

/// List the names of all top-level arrays in the Zarr store root.
///
/// - Local paths: scans for child directories containing `zarr.json` (v3) or `.zarray` (v2).
/// - Remote paths (HTTP/HTTPS/S3/GCS/Azure): reads consolidated metadata from the root zarr.json.
pub fn list_array_names(
    store_path: &str,
    store: &ZarrStore,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    if is_remote_scheme(store_path) {
        list_array_names_remote(store)
    } else {
        list_array_names_local(store_path)
    }
}

fn list_array_names_local(store_path: &str) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let root = Path::new(store_path);
    let mut names = Vec::new();
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let child_path = entry.path();

        // Zarr v3: zarr.json present — check node_type to skip nested groups.
        if child_path.join("zarr.json").exists() {
            let content = std::fs::read_to_string(child_path.join("zarr.json"))?;
            let meta: serde_json::Value = serde_json::from_str(&content)?;
            if meta.get("node_type").and_then(|v| v.as_str()) == Some("group") {
                continue; // nested group — design assumes flat store; skip silently
            }
            if let Some(name) = child_path.file_name().and_then(|n| n.to_str()) {
                names.push(name.to_string());
            }
            continue;
        }

        // Zarr v2: .zarray present.
        if child_path.join(".zarray").exists() {
            if let Some(name) = child_path.file_name().and_then(|n| n.to_str()) {
                names.push(name.to_string());
            }
        }
    }
    names.sort();
    Ok(names)
}

fn list_array_names_remote(store: &ZarrStore) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    use zarrs::group::Group;
    use zarrs::metadata::NodeMetadata;

    let group = Group::open(store.clone(), "/")?;
    let consolidated = group.consolidated_metadata().ok_or(
        "remote Zarr store has no consolidated metadata in zarr.json; \
         re-write the store with consolidated=True (xarray: zarr.consolidate_metadata(store))"
    )?;
    let mut names: Vec<String> = consolidated
        .metadata
        .iter()
        .filter_map(|(path, meta)| {
            // Only top-level arrays (no '/' in path after stripping leading '/').
            let name = path.trim_start_matches('/');
            if name.contains('/') {
                return None;
            }
            if matches!(meta, NodeMetadata::Array(_)) {
                Some(name.to_string())
            } else {
                None
            }
        })
        .collect();
    names.sort();
    Ok(names)
}

/// Open one array by name from the store.
pub fn open_array(
    store: &ZarrStore,
    name: &str,
) -> Result<ZarrArray, Box<dyn std::error::Error>> {
    let path = format!("/{name}");
    Ok(Array::open(store.clone(), &path)?)
}

/// Resolve dimension names for an array.
/// Priority: zarr v3 `dimension_names` field → `_ARRAY_DIMENSIONS` attr → error.
pub fn dimension_names(
    array: &ZarrArray,
    name: &str,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    // Zarr v3: dimension_names is a first-class field in zarr.json.
    if let Some(dim_names) = array.dimension_names() {
        return Ok(dim_names
            .iter()
            .enumerate()
            .map(|(i, d)| d.as_deref().unwrap_or(&format!("dim_{i}")).to_string())
            .collect());
    }
    // Zarr v2 / OME-Zarr fallback: _ARRAY_DIMENSIONS in attrs.
    let attrs = array.attributes();
    if let Some(serde_json::Value::Array(arr)) = attrs.get("_ARRAY_DIMENSIONS") {
        return Ok(arr
            .iter()
            .enumerate()
            .map(|(i, v)| {
                v.as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("dim_{i}"))
            })
            .collect());
    }
    Err(format!("array '{name}' has no dimension_names or _ARRAY_DIMENSIONS").into())
}

/// Parse `ZarrDtype` from the zarrs DataType.
/// `DataType::to_string()` may emit "v3_name / v2_name"; we use the v3 name (first token).
pub fn parse_dtype(
    array: &ZarrArray,
    name: &str,
) -> Result<ZarrDtype, Box<dyn std::error::Error>> {
    let full = array.data_type().to_string();
    let type_str = full.split(" / ").next().unwrap_or(&full);
    ZarrDtype::from_str(type_str)
        .ok_or_else(|| format!("unsupported dtype '{full}' for array '{name}'").into())
}

/// Parse `ColumnEncoding` and `FillSentinel` from CF attrs.
///
/// Packed-int rule: integer on-disk dtype AND (scale_factor OR add_offset in attrs).
pub fn parse_encoding_and_sentinel(
    dtype: &ZarrDtype,
    attrs: &serde_json::Map<String, serde_json::Value>,
) -> (ColumnEncoding, Option<FillSentinel>) {
    let scale = attrs
        .get("scale_factor")
        .and_then(|v| v.as_f64())
        .unwrap_or(1.0);
    let offset = attrs
        .get("add_offset")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);

    let has_packing = attrs.contains_key("scale_factor") || attrs.contains_key("add_offset");
    let encoding = if dtype.is_integer() && has_packing {
        ColumnEncoding::PackedInt {
            scale_factor: scale,
            add_offset: offset,
        }
    } else {
        ColumnEncoding::Plain
    };

    let sentinel = parse_sentinel(dtype, attrs);
    (encoding, sentinel)
}

fn parse_sentinel(
    dtype: &ZarrDtype,
    attrs: &serde_json::Map<String, serde_json::Value>,
) -> Option<FillSentinel> {
    let fill = attrs.get("_FillValue").and_then(|v| parse_fill_value(dtype, v));
    let missing = attrs.get("missing_value").and_then(|v| parse_fill_value(dtype, v));
    // xarray encodes _FillValue=NaN when it has already replaced fill values with NaN
    // in memory. For stores that use missing_value as the actual on-disk sentinel,
    // NaN in _FillValue is not the sentinel we need to mask; prefer missing_value.
    match fill {
        Some(FillSentinel::Float(v)) if v.is_nan() => missing.or(fill),
        Some(s) => Some(s),
        None => missing,
    }
}

fn parse_fill_value(dtype: &ZarrDtype, v: &serde_json::Value) -> Option<FillSentinel> {
    match v {
        // xarray FillValueCoder encodes float _FillValue as base64 LE float64.
        serde_json::Value::String(s) => {
            let bytes = base64::engine::general_purpose::STANDARD.decode(s).ok()?;
            if bytes.len() == 8 {
                let arr: [u8; 8] = bytes.try_into().ok()?;
                Some(FillSentinel::Float(f64::from_le_bytes(arr)))
            } else {
                None
            }
        }
        serde_json::Value::Number(n) => {
            if dtype.is_unsigned() {
                n.as_u64().map(FillSentinel::UInt)
            } else if dtype.is_integer() {
                n.as_i64().map(FillSentinel::Int)
            } else {
                n.as_f64().map(FillSentinel::Float)
            }
        }
        _ => None,
    }
}

/// Fall back to the zarr.json `fill_value` field when no CF sentinel attr is present.
/// Returns None for the all-zero default fill_value so we don't mask legitimate zeros.
fn parse_zarr_fill_sentinel(array: &ZarrArray, dtype: &ZarrDtype) -> Option<FillSentinel> {
    let bytes = array.fill_value().as_ne_bytes();
    if bytes.iter().all(|&b| b == 0) {
        return None;
    }
    match dtype {
        ZarrDtype::Bool => None,
        ZarrDtype::Int8 => Some(FillSentinel::Int(bytes[0] as i8 as i64)),
        ZarrDtype::Int16 => {
            let arr: [u8; 2] = bytes.try_into().ok()?;
            Some(FillSentinel::Int(i16::from_ne_bytes(arr) as i64))
        }
        ZarrDtype::Int32 => {
            let arr: [u8; 4] = bytes.try_into().ok()?;
            Some(FillSentinel::Int(i32::from_ne_bytes(arr) as i64))
        }
        ZarrDtype::Int64 => {
            let arr: [u8; 8] = bytes.try_into().ok()?;
            Some(FillSentinel::Int(i64::from_ne_bytes(arr)))
        }
        ZarrDtype::UInt8 => Some(FillSentinel::UInt(bytes[0] as u64)),
        ZarrDtype::UInt16 => {
            let arr: [u8; 2] = bytes.try_into().ok()?;
            Some(FillSentinel::UInt(u16::from_ne_bytes(arr) as u64))
        }
        ZarrDtype::UInt32 => {
            let arr: [u8; 4] = bytes.try_into().ok()?;
            Some(FillSentinel::UInt(u32::from_ne_bytes(arr) as u64))
        }
        ZarrDtype::UInt64 => {
            let arr: [u8; 8] = bytes.try_into().ok()?;
            Some(FillSentinel::UInt(u64::from_ne_bytes(arr)))
        }
        ZarrDtype::Float32 => {
            let arr: [u8; 4] = bytes.try_into().ok()?;
            Some(FillSentinel::Float(f32::from_ne_bytes(arr) as f64))
        }
        ZarrDtype::Float64 => {
            let arr: [u8; 8] = bytes.try_into().ok()?;
            Some(FillSentinel::Float(f64::from_ne_bytes(arr)))
        }
    }
}

/// Collect the set of non-dimension coord names from all `coordinates` attrs
/// across all arrays. These must be excluded from dim-group classification.
pub fn collect_auxiliary_coords(
    store: &ZarrStore,
    array_names: &[String],
) -> HashSet<String> {
    let mut aux = HashSet::new();
    for name in array_names {
        if let Ok(arr) = open_array(store, name) {
            if let Some(serde_json::Value::String(coords_str)) = arr.attributes().get("coordinates")
            {
                for token in coords_str.split_whitespace() {
                    aux.insert(token.to_string());
                }
            }
        }
    }
    aux
}

/// Determine whether a variable is a CF bounds variable to suppress.
/// Criteria: another array has a `bounds` attr pointing to this name,
/// OR this name matches `<dim>_bnds` / `<dim>_bounds` with shape (N, 2).
pub fn collect_bounds_vars(
    store: &ZarrStore,
    array_names: &[String],
    aux_coords: &HashSet<String>,
) -> HashSet<String> {
    let mut bounds = HashSet::new();

    // Attr-based: bounds = "name" on a coord array.
    for name in array_names {
        if let Ok(arr) = open_array(store, name) {
            if let Some(serde_json::Value::String(b)) = arr.attributes().get("bounds") {
                bounds.insert(b.clone());
            }
        }
    }

    // Name-pattern fallback: *_bnds / *_bounds with shape (N, 2).
    for name in array_names {
        if aux_coords.contains(name) || bounds.contains(name) {
            continue;
        }
        let is_pattern = name.ends_with("_bnds") || name.ends_with("_bounds");
        if !is_pattern {
            continue;
        }
        if let Ok(arr) = open_array(store, name) {
            let shape = arr.shape();
            if shape.len() == 2 && shape[1] == 2 {
                bounds.insert(name.clone());
            }
        }
    }
    bounds
}

/// Infer dim groups from the array set.
///
/// A dim group is a set of arrays sharing an identical ordered dimension list.
/// Coordinates (1-D arrays whose only dim == their name) and bounds vars are
/// excluded from data variables.
///
/// Returns `(dim_groups, coord_names)` where coord_names is the complete set
/// of coordinate array names.
pub fn infer_dim_groups(
    store: &ZarrStore,
    array_names: &[String],
) -> Result<(Vec<DimGroup>, HashSet<String>), Box<dyn std::error::Error>> {
    // Step 1: scan coordinates attr first (must precede dim-group enumeration).
    let aux_coords = collect_auxiliary_coords(store, array_names);
    let bounds_vars = collect_bounds_vars(store, array_names, &aux_coords);

    // Step 2: classify each array as coord or data var.
    //   coord: 1-D, sole dim == array name (dim-coord) OR in aux_coords (non-dim coord)
    //   data var: everything else (excluding bounds and scalar arrays)
    let mut coord_names: HashSet<String> = HashSet::new();
    let mut data_vars: Vec<String> = Vec::new();
    let mut scalar_names: HashSet<String> = HashSet::new();

    for name in array_names {
        if bounds_vars.contains(name) {
            continue;
        }
        let arr = open_array(store, name)?;
        let shape = arr.shape();

        if shape.is_empty() {
            // 0-dim scalar coordinate — suppress from schema.
            scalar_names.insert(name.clone());
            continue;
        }

        if aux_coords.contains(name) {
            coord_names.insert(name.clone());
            continue;
        }

        // Dim-coord heuristic: 1-D array whose sole dim shares its name.
        if shape.len() == 1 {
            if let Ok(dims) = dimension_names(&arr, name) {
                if dims.len() == 1 && dims[0] == *name {
                    coord_names.insert(name.clone());
                    continue;
                }
            }
        }

        data_vars.push(name.clone());
    }

    // Step 3: group data vars by their dim signature.
    let mut groups: HashMap<Vec<String>, DimGroup> = HashMap::new();

    for var_name in &data_vars {
        let arr = open_array(store, var_name)?;
        let dims = dimension_names(&arr, var_name)?;
        let shape = arr.shape().to_vec();

        let ndim = shape.len();
        let first_chunk = vec![0u64; ndim];
        let chunk_shape: Vec<u64> = arr
            .chunk_shape(&first_chunk)?
            .iter()
            .map(|x| x.get())
            .collect();

        // Collect coord names that belong to this dim group (dims that have matching coord arrays).
        let group_coord_names: Vec<String> = dims
            .iter()
            .filter(|d| coord_names.contains(*d))
            .cloned()
            .collect();

        let entry = groups.entry(dims.clone()).or_insert_with(|| DimGroup {
            dims,
            shape: shape.clone(),
            chunk_shape: chunk_shape.clone(),
            data_var_names: Vec::new(),
            coord_var_names: group_coord_names,
        });
        // Validate shape and chunk shape consistency within the dim group.
        if entry.shape != shape {
            return Err(format!(
                "array shape mismatch in dim group {:?}: existing {:?} vs '{var_name}' {:?}",
                entry.dims, entry.shape, shape
            )
            .into());
        }
        if entry.chunk_shape != chunk_shape {
            return Err(format!(
                "chunk shape mismatch in dim group {:?}: existing {:?} vs '{var_name}' {:?}",
                entry.dims, entry.chunk_shape, chunk_shape
            )
            .into());
        }
        entry.data_var_names.push(var_name.clone());
    }

    let mut dim_groups: Vec<DimGroup> = groups.into_values().collect();
    dim_groups.sort_by(|a, b| a.dims.cmp(&b.dims));

    Ok((dim_groups, coord_names))
}

/// Pre-load a coordinate array's raw bytes at bind time.
pub fn load_coord_array(
    store: &ZarrStore,
    coord_name: &str,
) -> Result<CoordArray, Box<dyn std::error::Error>> {
    let arr = open_array(store, coord_name)?;
    let dtype = parse_dtype(&arr, coord_name)?;
    let attrs = arr.attributes().clone();
    let (encoding, sentinel) = parse_encoding_and_sentinel(&dtype, &attrs);
    let sentinel = sentinel.or_else(|| parse_zarr_fill_sentinel(&arr, &dtype));
    let shape = arr.shape().to_vec();
    let n = shape[0] as usize;

    // ArrayBytes<'static> is the zarrs convention for requesting owned (non-borrowed)
    // decoded bytes; zarrs allocates a fresh Vec<u8> satisfying the 'static bound.
    let subset = arr.subset_all();
    let array_bytes = arr.retrieve_array_subset::<zarrs::array::ArrayBytes<'static>>(&subset)?;
    let raw = array_bytes.into_fixed().map_err(|_| "coord array has variable-length dtype")?;
    let bytes: Vec<u8> = raw.into_owned();
    debug_assert_eq!(
        bytes.len(),
        n * dtype.byte_size(),
        "coord byte count mismatch for '{coord_name}'"
    );

    Ok(CoordArray {
        dtype,
        encoding,
        sentinel,
        bytes,
    })
}

/// Build the list of `WorkUnit`s for one dim group.
pub fn build_work_units(group: &DimGroup) -> Vec<WorkUnit> {
    // Number of chunks per dimension.
    let n_chunks_per_dim: Vec<u64> = group
        .dims
        .iter()
        .enumerate()
        .map(|(i, _)| {
            group.shape[i].div_ceil(group.chunk_shape[i])
        })
        .collect();

    // Total number of chunks.
    let total: u64 = n_chunks_per_dim.iter().product();

    // Generate all chunk index tuples in C (row-major) order.
    let ndim = n_chunks_per_dim.len();
    let mut strides = vec![1u64; ndim];
    for k in (0..ndim.saturating_sub(1)).rev() {
        strides[k] = strides[k + 1] * n_chunks_per_dim[k + 1];
    }

    (0..total)
        .map(|i| {
            let chunk_indices = (0..ndim)
                .map(|k| (i / strides[k]) % n_chunks_per_dim[k])
                .collect();
            WorkUnit { chunk_indices }
        })
        .collect()
}

/// Build `ColumnDef`s for one dim group: dims first, then data vars.
pub fn build_column_defs(
    store: &ZarrStore,
    group: &DimGroup,
    coord_arrays: &HashMap<String, CoordArray>,
) -> Result<Vec<ColumnDef>, Box<dyn std::error::Error>> {
    let mut cols = Vec::new();

    // Dimension columns (coords or synthesized integers).
    for (dim_idx, dim) in group.dims.iter().enumerate() {
        if let Some(ca) = coord_arrays.get(dim) {
            cols.push(ColumnDef {
                name: dim.clone(),
                on_disk_dtype: ca.dtype.clone(),
                encoding: ca.encoding.clone(),
                sentinel: ca.sentinel.clone(),
                is_coord: true,
                dim_idx: Some(dim_idx),
            });
        } else {
            // Unindexed dim → synthesize 0..N integer range (Int64).
            // Mark is_coord=true so decode_work_unit skips it (no zarr array to load).
            cols.push(ColumnDef {
                name: dim.clone(),
                on_disk_dtype: ZarrDtype::Int64,
                encoding: ColumnEncoding::Plain,
                sentinel: None,
                is_coord: true,
                dim_idx: Some(dim_idx),
            });
        }
    }

    // Data variable columns.
    for var_name in &group.data_var_names {
        let arr = open_array(store, var_name)?;
        let dtype = parse_dtype(&arr, var_name)?;
        let attrs = arr.attributes().clone();
        let (encoding, sentinel) = parse_encoding_and_sentinel(&dtype, &attrs);
        let sentinel = sentinel.or_else(|| parse_zarr_fill_sentinel(&arr, &dtype));
        cols.push(ColumnDef {
            name: var_name.clone(),
            on_disk_dtype: dtype,
            encoding,
            sentinel,
            is_coord: false,
            dim_idx: None,
        });
    }

    Ok(cols)
}
