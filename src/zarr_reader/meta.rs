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
use zarrs::storage::{Bytes, ReadableStorageTraits, StoreKey};

use super::consolidated_store::ConsolidatedCacheStore;
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
///
/// Remote stores are additionally wrapped with an in-memory consolidated-
/// metadata cache when one is available (see [`with_consolidated_cache`]),
/// so callers get the fast path for free instead of having to remember to
/// apply it themselves.
pub fn open_store(
    path: &str,
    file_system: Option<duckdb_file_system>,
) -> Result<ZarrStore, Box<dyn std::error::Error>> {
    let lower = path.to_ascii_lowercase();
    let store: ZarrStore = if lower.starts_with("http://") || lower.starts_with("https://") {
        if let Some(mut fs) = file_system {
            unsafe { duckdb_destroy_file_system(&mut fs) };
        }
        Arc::new(zarrs_http::HTTPStore::new(path)?)
    } else if lower.starts_with("s3://") || lower.starts_with("gs://") || lower.starts_with("az://")
    {
        let fs = file_system.ok_or(
            "remote store requires a DuckDB FileSystem handle (call from a table function bind)",
        )?;
        Arc::new(DuckDbStore::new(fs, path))
    } else {
        if let Some(mut fs) = file_system {
            unsafe { duckdb_destroy_file_system(&mut fs) };
        }
        Arc::new(FilesystemStore::new(path)?)
    };
    Ok(with_consolidated_cache(path, store))
}

/// Wrap a remote store with an in-memory cache of its consolidated metadata,
/// if any, so that the many per-array metadata opens in [`infer_dim_groups`]
/// and [`finish_bind`](crate::read_zarr) are served from memory instead of one
/// HTTP round trip each. A no-op for local stores (`is_remote_scheme` guards
/// that). For remote stores, finding out costs:
/// - **one** extra GET when `zarr.json` itself carries v3 consolidated
///   metadata (the common case for v3 stores);
/// - **two** when it doesn't — a `zarr.json` probe that 404s on a v2-only
///   store, followed by `.zmetadata` — including when the store has no
///   consolidated metadata at all, in which case both probes 404 and this
///   is a no-op too.
fn with_consolidated_cache(store_path: &str, store: ZarrStore) -> ZarrStore {
    if !is_remote_scheme(store_path) {
        return store;
    }
    match build_consolidated_cache(&store) {
        Ok(Some(cache)) => Arc::new(ConsolidatedCacheStore::new(store, cache)),
        // No consolidated metadata found — normal for a store that predates
        // it, or was never written with `consolidated=True`. Not an error.
        Ok(None) => store,
        // Something went wrong reading it — malformed JSON, a missing
        // `metadata` object, but also a transient network/auth failure on
        // the zarr.json/.zmetadata probe itself (both surface through the
        // same `?`, and there's no cheap way to tell them apart here). Don't
        // claim to know which; just say what happened and what we're doing
        // about it. Printed to stderr, so it's visible when running the
        // DuckDB CLI directly — clients that don't inherit the process's
        // stderr (Python, JDBC, GUI shells) won't see it, only the slower
        // per-array fallback.
        Err(err) => {
            eprintln!(
                "duckdb_zarr: could not read consolidated metadata for '{store_path}' ({err}); \
                 falling back to per-array metadata reads"
            );
            store
        }
    }
}

fn build_consolidated_cache(
    store: &ZarrStore,
) -> Result<Option<HashMap<StoreKey, Bytes>>, Box<dyn std::error::Error>> {
    // Zarr v3: consolidated metadata is embedded in the root `zarr.json`, as a
    // map of node path -> that node's full metadata document.
    if let Some(bytes) = store.get(&StoreKey::new("zarr.json")?)? {
        let doc: serde_json::Value = serde_json::from_slice(&bytes)?;
        if let Some(metadata) = doc
            .get("consolidated_metadata")
            .and_then(|c| c.get("metadata"))
            .and_then(serde_json::Value::as_object)
        {
            let mut cache = HashMap::new();
            // Insert the full root document (the only entry carrying the
            // `consolidated_metadata` block itself) first. Some writers may
            // additionally list the root group under its own path ("" or
            // "/") in `metadata` — if so, skip it below rather than
            // overwrite this entry with that sub-document, which lacks
            // `consolidated_metadata` and would break any later
            // `Group::open(store, "/")` that reads it back from the cache.
            cache.insert(StoreKey::new("zarr.json")?, bytes.clone());
            for (name, node_meta) in metadata {
                let name = name.trim_start_matches('/');
                if name.is_empty() {
                    continue;
                }
                cache.insert(
                    StoreKey::new(format!("{name}/zarr.json"))?,
                    Bytes::from(serde_json::to_vec(node_meta)?),
                );
            }
            return Ok(Some(cache));
        }
    }

    // Zarr v2: consolidated metadata lives in a separate `.zmetadata` object —
    // a flat map from `<path>/.zarray` / `<path>/.zattrs` / `.zgroup` keys to
    // their file contents.
    if let Some(bytes) = store.get(&StoreKey::new(".zmetadata")?)? {
        let doc: serde_json::Value = serde_json::from_slice(&bytes)?;
        if let Some(metadata) = doc.get("metadata").and_then(serde_json::Value::as_object) {
            let mut cache = HashMap::new();
            cache.insert(StoreKey::new(".zmetadata")?, bytes.clone());
            for (key, value) in metadata {
                cache.insert(
                    StoreKey::new(key.as_str())?,
                    Bytes::from(serde_json::to_vec(value)?),
                );
            }
            return Ok(Some(cache));
        }
    }

    Ok(None)
}

/// List the store-relative paths of all arrays in the Zarr hierarchy.
///
/// - Local paths: recursively scans directories containing `zarr.json` (v3) or `.zarray` (v2).
/// - Remote paths (HTTP/HTTPS/S3/GCS/Azure): enumerates arrays from consolidated metadata,
///   since object stores cannot list directories — a v3 `consolidated_metadata` block in
///   `zarr.json`, or a v2 `.zmetadata` object.
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
    collect_array_names_local(root, root, &mut names)?;
    names.sort();
    Ok(names)
}

fn collect_array_names_local(
    root: &Path,
    directory: &Path,
    names: &mut Vec<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let child_path = entry.path();

        // Zarr v3: arrays are terminal; groups are traversed recursively.
        if child_path.join("zarr.json").exists() {
            let content = std::fs::read_to_string(child_path.join("zarr.json"))?;
            let meta: serde_json::Value = serde_json::from_str(&content)?;
            if meta.get("node_type").and_then(|v| v.as_str()) == Some("group") {
                collect_array_names_local(root, &child_path, names)?;
                continue;
            }
            names.push(relative_array_path(root, &child_path)?);
            continue;
        }

        // Zarr v2: .zarray present.
        if child_path.join(".zarray").exists() {
            names.push(relative_array_path(root, &child_path)?);
            continue;
        }

        // A v2 group has .zgroup; intermediate containers may have no metadata.
        collect_array_names_local(root, &child_path, names)?;
    }
    Ok(())
}

fn relative_array_path(
    root: &Path,
    array_path: &Path,
) -> Result<String, Box<dyn std::error::Error>> {
    Ok(array_path
        .strip_prefix(root)?
        .components()
        .map(|part| part.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/"))
}

fn list_array_names_remote(store: &ZarrStore) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    use zarrs::group::Group;
    use zarrs::metadata::NodeMetadata;

    // Zarr v3: consolidated metadata is embedded in the root `zarr.json`.
    let group = Group::open(store.clone(), "/")?;
    if let Some(consolidated) = group.consolidated_metadata() {
        let mut names: Vec<String> = consolidated
            .metadata
            .iter()
            .filter_map(|(path, meta)| {
                let name = path.trim_start_matches('/');
                if matches!(meta, NodeMetadata::Array(_)) {
                    Some(name.to_string())
                } else {
                    None
                }
            })
            .collect();
        names.sort();
        return Ok(names);
    }

    // Zarr v2: consolidated metadata lives in a separate `.zmetadata` object.
    // HTTP/object stores cannot list directories, so `.zmetadata` is the only
    // way to enumerate a v2 store's arrays remotely.
    if let Some(names) = list_array_names_zmetadata(store)? {
        return Ok(names);
    }

    Err(
        "remote Zarr store has no consolidated metadata: found neither a v3 \
         `consolidated_metadata` block in `zarr.json` nor a v2 `.zmetadata` object. \
         Re-write the store with consolidated metadata \
         (xarray: `ds.to_zarr(store, consolidated=True)`)."
            .into(),
    )
}

/// Enumerate array names from a Zarr v2 consolidated-metadata (`.zmetadata`) object.
///
/// Returns `Ok(None)` when the store has no `.zmetadata`, so the caller can emit a
/// single clear error. Arrays are the entries whose key ends in `/.zarray`.
fn list_array_names_zmetadata(
    store: &ZarrStore,
) -> Result<Option<Vec<String>>, Box<dyn std::error::Error>> {
    let Some(bytes) = store.get(&StoreKey::new(".zmetadata")?)? else {
        return Ok(None);
    };
    let doc: serde_json::Value = serde_json::from_slice(&bytes)?;
    let metadata = doc
        .get("metadata")
        .and_then(serde_json::Value::as_object)
        .ok_or("`.zmetadata` is missing its `metadata` object")?;
    let mut names: Vec<String> = metadata
        .keys()
        .filter_map(|key| key.strip_suffix("/.zarray").map(str::to_string))
        .collect();
    names.sort();
    Ok(Some(names))
}

/// Open one array by name from the store.
pub fn open_array(store: &ZarrStore, name: &str) -> Result<ZarrArray, Box<dyn std::error::Error>> {
    let path = format!("/{name}");
    Ok(Array::open(store.clone(), &path)?)
}

/// Resolve and validate a user-provided store-relative array path.
pub fn select_array_name(
    array_names: &[String],
    requested: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let normalized = requested.trim().trim_matches('/');
    array_names
        .iter()
        .find(|name| name.as_str() == normalized)
        .cloned()
        .ok_or_else(|| {
            format!(
                "array '{requested}' not found; available arrays: {:?}",
                array_names
            )
            .into()
        })
}

/// Build a one-array dimension group for `read_zarr(..., array_path='path')`.
///
/// Coordinate arrays are resolved from the selected array's sibling group first,
/// then from the store root. OME-Zarr arrays normally have no coordinate arrays,
/// so their named dimensions are synthesized as integer indices.
pub fn dim_group_for_array(
    store: &ZarrStore,
    array_names: &[String],
    array_name: &str,
) -> Result<DimGroup, Box<dyn std::error::Error>> {
    let arr = open_array(store, array_name)?;
    let shape = arr.shape().to_vec();
    // Named dimensions come from xarray metadata; fall back to OME-Zarr
    // `multiscales.axes` (matched by rank) for stores that carry neither
    // `dimension_names` nor `_ARRAY_DIMENSIONS` on the array itself.
    let dims = match dimension_names(&arr, array_name) {
        Ok(dims) => dims,
        Err(err) => ome_axis_names(store, array_name)
            .filter(|axes| axes.len() == shape.len())
            .ok_or(err)?,
    };
    let first_chunk = vec![0u64; shape.len()];
    let chunk_shape = arr
        .chunk_shape(&first_chunk)?
        .iter()
        .map(|x| x.get())
        .collect();
    let coord_var_names = dims
        .iter()
        .filter_map(|dim| find_coord_array_path(store, array_names, array_name, dim))
        .collect();

    Ok(DimGroup {
        dims,
        shape,
        chunk_shape,
        data_var_names: vec![array_name.to_string()],
        coord_var_names,
    })
}

/// OME-Zarr fallback for dimension names.
///
/// Real OME-Zarr images record axis names in the parent group's
/// `multiscales.axes` rather than in per-array `dimension_names` /
/// `_ARRAY_DIMENSIONS`. This resolves them by matching the array to its
/// `multiscales.datasets[].path` entry, recovering `c`/`z`/`y`/`x` for
/// `array_path` reads of stores like those in the IDR.
fn ome_axis_names(store: &ZarrStore, array_name: &str) -> Option<Vec<String>> {
    use zarrs::group::Group;

    let (group_path, dataset) = match array_name.rsplit_once('/') {
        Some((group, ds)) => (format!("/{group}"), ds),
        None => ("/".to_string(), array_name),
    };
    let group = Group::open(store.clone(), &group_path).ok()?;
    let multiscales = group.attributes().get("multiscales")?.as_array()?;
    for ms in multiscales {
        let matches_dataset = ms
            .get("datasets")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|datasets| {
                datasets
                    .iter()
                    .any(|d| d.get("path").and_then(serde_json::Value::as_str) == Some(dataset))
            });
        if !matches_dataset {
            continue;
        }
        let Some(axes) = ms.get("axes").and_then(serde_json::Value::as_array) else {
            continue;
        };
        let names: Vec<String> = axes
            .iter()
            .filter_map(|axis| {
                axis.get("name")
                    .and_then(serde_json::Value::as_str)
                    .map(String::from)
            })
            .collect();
        if !names.is_empty() {
            return Some(names);
        }
    }
    None
}

fn parent_path(name: &str) -> &str {
    name.rsplit_once('/')
        .map(|(parent, _)| parent)
        .unwrap_or("")
}

fn basename(name: &str) -> &str {
    name.rsplit('/').next().unwrap_or(name)
}

fn find_coord_array_path(
    store: &ZarrStore,
    array_names: &[String],
    data_var_name: &str,
    dim: &str,
) -> Option<String> {
    let parent = parent_path(data_var_name);
    let sibling = if parent.is_empty() {
        dim.to_string()
    } else {
        format!("{parent}/{dim}")
    };

    let match_path = [sibling.as_str(), dim].into_iter().find_map(|candidate| {
        if !array_names.iter().any(|name| name == candidate) {
            return None;
        }
        let arr = open_array(store, candidate).ok()?;
        let dims = dimension_names(&arr, candidate).ok()?;
        (arr.shape().len() == 1 && dims.as_slice() == [dim]).then(|| candidate.to_string())
    });
    match_path
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
pub fn parse_dtype(array: &ZarrArray, name: &str) -> Result<ZarrDtype, Box<dyn std::error::Error>> {
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
    let fill = attrs
        .get("_FillValue")
        .and_then(|v| parse_fill_value(dtype, v));
    let missing = attrs
        .get("missing_value")
        .and_then(|v| parse_fill_value(dtype, v));
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
pub fn collect_auxiliary_coords(store: &ZarrStore, array_names: &[String]) -> HashSet<String> {
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

        // Dim-coord heuristic: 1-D array whose sole dim shares its basename.
        if shape.len() == 1 {
            if let Ok(dims) = dimension_names(&arr, name) {
                if dims.len() == 1 && dims[0] == basename(name) {
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
            .filter_map(|dim| find_coord_array_path(store, array_names, var_name, dim))
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
                "array shape mismatch in dim group {:?}: existing {:?} vs '{var_name}' {:?}; use array_path= to select one array",
                entry.dims, entry.shape, shape
            )
            .into());
        }
        if entry.chunk_shape != chunk_shape {
            return Err(format!(
                "chunk shape mismatch in dim group {:?}: existing {:?} vs '{var_name}' {:?}; use array_path= to select one array",
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

/// Discover dimension groups for metadata inspection without requiring every
/// array that shares dimension names to also share shape and chunk layout.
///
/// Multiscale OME-Zarr levels deliberately reuse axis names at different
/// resolutions, so `read_zarr_groups` groups by `(dims, shape, chunk_shape)`.
/// The stricter [`infer_dim_groups`] remains in the scan path because one scan
/// can only align variables with identical shapes and chunk grids.
pub fn discover_dim_groups(
    store: &ZarrStore,
    array_names: &[String],
) -> Result<Vec<DimGroup>, Box<dyn std::error::Error>> {
    let aux_coords = collect_auxiliary_coords(store, array_names);
    let bounds_vars = collect_bounds_vars(store, array_names, &aux_coords);
    let mut coord_names = HashSet::new();

    for name in array_names {
        if bounds_vars.contains(name) || aux_coords.contains(name) {
            continue;
        }
        let arr = open_array(store, name)?;
        if arr.shape().len() == 1 {
            if let Ok(dims) = dimension_names(&arr, name) {
                if dims.len() == 1 && dims[0] == basename(name) {
                    coord_names.insert(name.clone());
                }
            }
        }
    }

    type GroupKey = (Vec<String>, Vec<u64>, Vec<u64>);
    let mut groups: HashMap<GroupKey, DimGroup> = HashMap::new();
    for name in array_names {
        if bounds_vars.contains(name) || aux_coords.contains(name) || coord_names.contains(name) {
            continue;
        }
        let arr = open_array(store, name)?;
        let shape = arr.shape().to_vec();
        if shape.is_empty() {
            continue;
        }
        let dims = dimension_names(&arr, name)?;
        let chunk_shape = arr
            .chunk_shape(&vec![0u64; shape.len()])?
            .iter()
            .map(|x| x.get())
            .collect::<Vec<_>>();
        let coord_var_names = dims
            .iter()
            .filter_map(|dim| find_coord_array_path(store, array_names, name, dim))
            .collect::<Vec<_>>();
        let key = (dims.clone(), shape.clone(), chunk_shape.clone());
        let group = groups.entry(key).or_insert_with(|| DimGroup {
            dims,
            shape,
            chunk_shape,
            data_var_names: Vec::new(),
            coord_var_names,
        });
        group.data_var_names.push(name.clone());
    }

    let mut dim_groups = groups.into_values().collect::<Vec<_>>();
    dim_groups.sort_by(|a, b| {
        a.dims
            .cmp(&b.dims)
            .then_with(|| a.shape.cmp(&b.shape))
            .then_with(|| a.data_var_names.cmp(&b.data_var_names))
    });
    Ok(dim_groups)
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
    let raw = array_bytes
        .into_fixed()
        .map_err(|_| "coord array has variable-length dtype")?;
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
        .map(|(i, _)| group.shape[i].div_ceil(group.chunk_shape[i]))
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

/// Inclusive `[lo, hi]` bound on one dimension's raw coordinate values,
/// as given by `read_zarr(..., ranges=['dim:lo:hi'])`. Both bounds are
/// inclusive (`lo <= coord <= hi`), matching SQL `BETWEEN`. An exclusive
/// bound has to be expressed in `WHERE`; the range still prunes the chunks.
#[derive(Debug, Clone, PartialEq)]
pub struct DimRange {
    pub dim: String,
    pub lo: f64,
    pub hi: f64,
}

/// Parse one `dim:lo:hi` item. Either bound may be empty (`time:1000:` is
/// "time >= 1000"). Values are compared against the raw on-disk coordinate
/// values, exactly as `read_zarr` emits them in the coordinate columns.
pub fn parse_dim_range(item: &str) -> Result<DimRange, Box<dyn std::error::Error>> {
    let parts: Vec<&str> = item.rsplitn(3, ':').collect();
    if parts.len() != 3 {
        return Err(
            format!("range '{item}' must look like 'dim:lo:hi' (bounds may be empty)").into(),
        );
    }
    let (hi, lo, dim) = (parts[0].trim(), parts[1].trim(), parts[2].trim());
    let bound = |s: &str, default: f64| -> Result<f64, Box<dyn std::error::Error>> {
        if s.is_empty() {
            Ok(default)
        } else {
            s.parse::<f64>()
                .map_err(|_| format!("range '{item}': '{s}' is not a number").into())
        }
    };
    Ok(DimRange {
        dim: dim.to_string(),
        lo: bound(lo, f64::NEG_INFINITY)?,
        hi: bound(hi, f64::INFINITY)?,
    })
}

/// Raw coordinate value at `idx` as f64 (`None` for bool or out of range).
pub(crate) fn coord_value_f64(ca: &CoordArray, idx: usize) -> Option<f64> {
    let w = ca.dtype.byte_size();
    let b = ca.bytes.get(idx * w..(idx + 1) * w)?;
    Some(match ca.dtype {
        ZarrDtype::Bool => return None,
        ZarrDtype::Int8 => b[0] as i8 as f64,
        ZarrDtype::Int16 => i16::from_ne_bytes(b.try_into().ok()?) as f64,
        ZarrDtype::Int32 => i32::from_ne_bytes(b.try_into().ok()?) as f64,
        ZarrDtype::Int64 => i64::from_ne_bytes(b.try_into().ok()?) as f64,
        ZarrDtype::UInt8 => b[0] as f64,
        ZarrDtype::UInt16 => u16::from_ne_bytes(b.try_into().ok()?) as f64,
        ZarrDtype::UInt32 => u32::from_ne_bytes(b.try_into().ok()?) as f64,
        ZarrDtype::UInt64 => u64::from_ne_bytes(b.try_into().ok()?) as f64,
        ZarrDtype::Float32 => f32::from_ne_bytes(b.try_into().ok()?) as f64,
        ZarrDtype::Float64 => f64::from_ne_bytes(b.try_into().ok()?),
    })
}

/// Chunk pruning: keep only the chunks whose coordinate `[min, max]` along each
/// ranged dimension intersects the requested `[lo, hi]`. Min/max are computed
/// over the actual coordinate slice (not first/last), so descending coordinates
/// (ERA5 latitude 90 -> -90) prune correctly. A dimension without a coordinate
/// array is pruned on its integer index. Rows inside a kept chunk that fall
/// outside the range are dropped by the scan via `build_dim_keep_masks`.
pub fn build_work_units_pruned(
    group: &DimGroup,
    coord_arrays: &HashMap<String, CoordArray>,
    ranges: &[DimRange],
) -> Result<Vec<WorkUnit>, Box<dyn std::error::Error>> {
    let ndim = group.dims.len();
    let mut kept: Vec<Vec<u64>> = (0..ndim)
        .map(|k| (0..group.shape[k].div_ceil(group.chunk_shape[k])).collect())
        .collect();

    for r in ranges {
        let k = group.dims.iter().position(|d| d == &r.dim).ok_or_else(|| {
            format!(
                "ranges: '{}' is not a dimension of this table; dims are {:?}",
                r.dim, group.dims
            )
        })?;
        let n = group.shape[k] as usize;
        let cs = group.chunk_shape[k] as usize;
        let ca = coord_arrays.get(&r.dim);
        if let Some(ca) = ca {
            if matches!(ca.dtype, ZarrDtype::Bool) {
                return Err(format!("ranges: dimension '{}' has a bool coordinate", r.dim).into());
            }
        }
        kept[k].retain(|&c| {
            let start = c as usize * cs;
            let end = (start + cs).min(n);
            let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
            for i in start..end {
                let v = match ca {
                    Some(ca) => match coord_value_f64(ca, i) {
                        Some(v) if !v.is_nan() => v,
                        _ => continue,
                    },
                    None => i as f64,
                };
                lo = lo.min(v);
                hi = hi.max(v);
            }
            lo <= r.hi && hi >= r.lo
        });
    }

    let mut units = Vec::new();
    let mut idx = vec![0usize; ndim];
    if kept.iter().any(|v| v.is_empty()) {
        return Ok(units);
    }
    loop {
        units.push(WorkUnit {
            chunk_indices: (0..ndim).map(|k| kept[k][idx[k]]).collect(),
        });
        // Odometer increment, last dim fastest (C order).
        let mut k = ndim;
        loop {
            if k == 0 {
                return Ok(units);
            }
            k -= 1;
            idx[k] += 1;
            if idx[k] < kept[k].len() {
                break;
            }
            idx[k] = 0;
        }
    }
}

/// Per-dimension row masks for `ranges=`: `masks[k][i]` is whether global
/// index `i` along dimension `k` satisfies every range on that dimension.
/// `None` for a dimension with no range (every index kept). A coordinate that
/// is NaN or unreadable never satisfies a range.
pub fn build_dim_keep_masks(
    group: &DimGroup,
    coord_arrays: &HashMap<String, CoordArray>,
    ranges: &[DimRange],
) -> Result<Vec<Option<Vec<bool>>>, Box<dyn std::error::Error>> {
    let mut masks: Vec<Option<Vec<bool>>> = vec![None; group.dims.len()];
    for r in ranges {
        let k = group.dims.iter().position(|d| d == &r.dim).ok_or_else(|| {
            format!(
                "ranges: '{}' is not a dimension of this table; dims are {:?}",
                r.dim, group.dims
            )
        })?;
        let n = group.shape[k] as usize;
        let ca = coord_arrays.get(&r.dim);
        let mask = masks[k].get_or_insert_with(|| vec![true; n]);
        for (i, keep) in mask.iter_mut().enumerate() {
            let v = match ca {
                Some(ca) => coord_value_f64(ca, i).unwrap_or(f64::NAN),
                None => i as f64,
            };
            *keep = *keep && v >= r.lo && v <= r.hi;
        }
    }
    Ok(masks)
}

/// Flat logical row indices (C order) inside one chunk that survive the
/// per-dimension masks. `None` when every row survives, so the common
/// unpruned path costs nothing. `logical_shape` is the chunk shape clipped
/// to the array bounds.
pub fn kept_rows_in_chunk(
    wu: &WorkUnit,
    chunk_shape: &[u64],
    logical_shape: &[usize],
    masks: &[Option<Vec<bool>>],
) -> Option<Vec<usize>> {
    if masks.iter().all(|m| m.is_none()) {
        return None;
    }
    let ndim = logical_shape.len();
    // Per-dim list of kept local indices.
    let per_dim: Vec<Vec<usize>> = (0..ndim)
        .map(|k| {
            let origin = (wu.chunk_indices[k] * chunk_shape[k]) as usize;
            (0..logical_shape[k])
                .filter(|&j| masks[k].as_ref().is_none_or(|m| m[origin + j]))
                .collect()
        })
        .collect();
    let total: usize = logical_shape.iter().product();
    if per_dim
        .iter()
        .enumerate()
        .all(|(k, v)| v.len() == logical_shape[k])
    {
        return None;
    }
    let mut strides = vec![1usize; ndim];
    for k in (0..ndim.saturating_sub(1)).rev() {
        strides[k] = strides[k + 1] * logical_shape[k + 1];
    }
    let mut rows = Vec::new();
    if per_dim.iter().any(|v| v.is_empty()) {
        return Some(rows);
    }
    let mut idx = vec![0usize; ndim];
    loop {
        let flat: usize = (0..ndim).map(|k| per_dim[k][idx[k]] * strides[k]).sum();
        debug_assert!(flat < total);
        rows.push(flat);
        let mut k = ndim;
        loop {
            if k == 0 {
                return Some(rows);
            }
            k -= 1;
            idx[k] += 1;
            if idx[k] < per_dim[k].len() {
                break;
            }
            idx[k] = 0;
        }
    }
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use zarrs::storage::store::MemoryStore;
    use zarrs::storage::WritableStorageTraits;

    use super::*;

    #[test]
    fn v3_consolidated_cache_survives_a_self_referencing_root_entry() {
        // Some v3 writers list the root group under its own path ("" or "/")
        // inside `consolidated_metadata.metadata`. That sub-document doesn't
        // itself carry `consolidated_metadata`, so it must never overwrite
        // the full root document already cached under the "zarr.json" key.
        let store_inner = MemoryStore::new();
        let root_doc = serde_json::json!({
            "zarr_format": 3,
            "node_type": "group",
            "consolidated_metadata": {
                "kind": "inline",
                "must_understand": false,
                "metadata": {
                    "/": {"zarr_format": 3, "node_type": "group"},
                    "foo": {"zarr_format": 3, "node_type": "array"},
                }
            }
        });
        store_inner
            .set(
                &StoreKey::new("zarr.json").unwrap(),
                Bytes::from(serde_json::to_vec(&root_doc).unwrap()),
            )
            .unwrap();
        let store: ZarrStore = Arc::new(store_inner);

        let cache = build_consolidated_cache(&store).unwrap().unwrap();

        let cached_root = cache.get(&StoreKey::new("zarr.json").unwrap()).unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(cached_root).unwrap();
        assert!(
            parsed.get("consolidated_metadata").is_some(),
            "root zarr.json cache entry lost its consolidated_metadata block: {parsed}"
        );
        assert!(cache.contains_key(&StoreKey::new("foo/zarr.json").unwrap()));
    }

    fn f64_coord(vals: &[f64]) -> CoordArray {
        CoordArray {
            dtype: ZarrDtype::Float64,
            encoding: ColumnEncoding::Plain,
            sentinel: None,
            bytes: vals.iter().flat_map(|v| v.to_ne_bytes()).collect(),
        }
    }

    #[test]
    fn parse_dim_range_forms() {
        assert_eq!(
            parse_dim_range("lat:40:50").unwrap(),
            DimRange {
                dim: "lat".into(),
                lo: 40.0,
                hi: 50.0
            }
        );
        let r = parse_dim_range("time:1000:").unwrap();
        assert_eq!((r.lo, r.hi), (1000.0, f64::INFINITY));
        let r = parse_dim_range("time::-5").unwrap();
        assert_eq!((r.lo, r.hi), (f64::NEG_INFINITY, -5.0));
        assert!(parse_dim_range("lat:40").is_err());
        assert!(parse_dim_range("lat:a:b").is_err());
    }

    #[test]
    fn prune_descending_coord_and_unindexed_dim() {
        // time: 10 steps, chunks of 4 (3 chunks); lat: 90 -> -90 by 30, chunks of 4 (2 chunks).
        let group = DimGroup {
            dims: vec!["time".into(), "lat".into()],
            shape: vec![10, 7],
            chunk_shape: vec![4, 4],
            data_var_names: vec!["t".into()],
            coord_var_names: vec!["lat".into()],
        };
        let mut coords = HashMap::new();
        coords.insert(
            "lat".to_string(),
            f64_coord(&[90.0, 60.0, 30.0, 0.0, -30.0, -60.0, -90.0]),
        );
        let all = build_work_units_pruned(&group, &coords, &[]).unwrap();
        assert_eq!(all.len(), 6);
        assert_eq!(all, build_work_units(&group));

        // lat in [-40, -20]: only the second lat chunk (-30..-90) intersects.
        let r = vec![parse_dim_range("lat:-40:-20").unwrap()];
        let u = build_work_units_pruned(&group, &coords, &r).unwrap();
        assert_eq!(u.len(), 3);
        assert!(u.iter().all(|w| w.chunk_indices[1] == 1));

        // time is unindexed: pruned on integer index. time in [4, 5] -> chunk 1 only.
        let r = vec![
            parse_dim_range("time:4:5").unwrap(),
            parse_dim_range("lat:0:100").unwrap(),
        ];
        let u = build_work_units_pruned(&group, &coords, &r).unwrap();
        assert_eq!(u.len(), 1);
        assert_eq!(u[0].chunk_indices, vec![1, 0]);

        // Empty intersection -> no work units.
        let r = vec![parse_dim_range("lat:200:300").unwrap()];
        assert!(build_work_units_pruned(&group, &coords, &r)
            .unwrap()
            .is_empty());

        // Unknown dim -> error.
        let r = vec![parse_dim_range("lon:0:1").unwrap()];
        assert!(build_work_units_pruned(&group, &coords, &r).is_err());
    }

    // ---- Test cases required by docs/design.md "Predicate & projection pushdown" ----

    fn era5_like_group() -> (DimGroup, HashMap<String, CoordArray>) {
        // time: 12 steps (0..12), chunks of 4 -> 3 chunks.
        // lat: 90 -> -90 by 30 (7 values), chunks of 3 -> 3 chunks: [90,60,30] [0,-30,-60] [-90].
        // level: pressure levels, non-uniform, chunks of 2 -> 3 chunks:
        //   [1000, 850] [700, 500] [250, 50].
        let group = DimGroup {
            dims: vec!["time".into(), "lat".into(), "level".into()],
            shape: vec![12, 7, 6],
            chunk_shape: vec![4, 3, 2],
            data_var_names: vec!["t".into()],
            coord_var_names: vec!["time".into(), "lat".into(), "level".into()],
        };
        let mut coords = HashMap::new();
        coords.insert(
            "time".to_string(),
            f64_coord(&(0..12).map(|i| i as f64).collect::<Vec<_>>()),
        );
        coords.insert(
            "lat".to_string(),
            f64_coord(&[90.0, 60.0, 30.0, 0.0, -30.0, -60.0, -90.0]),
        );
        coords.insert(
            "level".to_string(),
            f64_coord(&[1000.0, 850.0, 700.0, 500.0, 250.0, 50.0]),
        );
        (group, coords)
    }

    fn pruned(
        group: &DimGroup,
        coords: &HashMap<String, CoordArray>,
        rs: &[&str],
    ) -> Vec<WorkUnit> {
        let rs: Vec<DimRange> = rs.iter().map(|r| parse_dim_range(r).unwrap()).collect();
        build_work_units_pruned(group, coords, &rs).unwrap()
    }

    fn chunk_set(units: &[WorkUnit], k: usize) -> Vec<u64> {
        let mut v: Vec<u64> = units.iter().map(|w| w.chunk_indices[k]).collect();
        v.sort();
        v.dedup();
        v
    }

    #[test]
    fn design_decreasing_coordinate() {
        // ERA5 latitude runs 90 -> -90; a range on the low end must select the
        // chunk at the END of the index space, and the index translation must
        // not assume monotonic-increasing.
        let (g, c) = era5_like_group();
        assert_eq!(chunk_set(&pruned(&g, &c, &["lat:-90:-70"]), 1), vec![2]);
        assert_eq!(chunk_set(&pruned(&g, &c, &["lat:-70:-10"]), 1), vec![1]);
        assert_eq!(chunk_set(&pruned(&g, &c, &["lat:50:90"]), 1), vec![0]);
        // Straddles two chunks: 30 (chunk 0) and 0 (chunk 1).
        assert_eq!(chunk_set(&pruned(&g, &c, &["lat:0:30"]), 1), vec![0, 1]);
    }

    #[test]
    fn design_non_uniform_spacing() {
        // Pressure levels are not evenly spaced; chunk index cannot be
        // coord / chunk_size. [1000,850] [700,500] [250,50].
        let (g, c) = era5_like_group();
        assert_eq!(chunk_set(&pruned(&g, &c, &["level:600:800"]), 2), vec![1]);
        assert_eq!(chunk_set(&pruned(&g, &c, &["level:100:300"]), 2), vec![2]);
        assert_eq!(chunk_set(&pruned(&g, &c, &["level:900:1000"]), 2), vec![0]);
        // Gap between 500 and 250: nothing lives there.
        assert!(pruned(&g, &c, &["level:300:450"]).is_empty());
    }

    #[test]
    fn design_exact_chunk_boundary_predicate() {
        // A point predicate on a value sitting exactly on a chunk seam must
        // select the owning chunk once, not zero or two chunks.
        let (g, c) = era5_like_group();
        // time=4 is the first element of chunk 1; time=3 the last of chunk 0.
        assert_eq!(chunk_set(&pruned(&g, &c, &["time:4:4"]), 0), vec![1]);
        assert_eq!(chunk_set(&pruned(&g, &c, &["time:3:3"]), 0), vec![0]);
        // lat=0 is the first element of lat chunk 1.
        assert_eq!(chunk_set(&pruned(&g, &c, &["lat:0:0"]), 1), vec![1]);
        // Rows: exactly one time step survives inside the kept chunk.
        let rs = [parse_dim_range("time:4:4").unwrap()];
        let masks = build_dim_keep_masks(&g, &c, &rs).unwrap();
        let wu = WorkUnit {
            chunk_indices: vec![1, 0, 0],
        };
        let rows = kept_rows_in_chunk(&wu, &g.chunk_shape, &[4, 3, 2], &masks).unwrap();
        assert_eq!(rows.len(), 6);
        assert!(rows.iter().all(|r| r / 6 == 0));
    }

    #[test]
    fn design_empty_result_predicate() {
        // lat > 100: zero work units, no panic, nothing scheduled.
        let (g, c) = era5_like_group();
        assert!(pruned(&g, &c, &["lat:100:"]).is_empty());
        assert!(pruned(&g, &c, &["time::-1"]).is_empty());
        // And the row masks agree: nothing kept along that dim.
        let rs = [parse_dim_range("lat:100:").unwrap()];
        let masks = build_dim_keep_masks(&g, &c, &rs).unwrap();
        assert!(masks[1].as_ref().unwrap().iter().all(|k| !k));
    }

    #[test]
    fn design_inclusive_bounds_and_row_clipping() {
        // ranges= is inclusive on both ends (BETWEEN). Rows inside a kept chunk
        // outside the bounds are clipped; rows on the bound are kept.
        let (g, c) = era5_like_group();
        let rs = [
            parse_dim_range("time:1:2").unwrap(),
            parse_dim_range("lat:0:60").unwrap(),
        ];
        let units = build_work_units_pruned(&g, &c, &rs).unwrap();
        // time chunk 0 only; lat chunks 0 (60, 30) and 1 (0); all 3 level chunks.
        assert_eq!(chunk_set(&units, 0), vec![0]);
        assert_eq!(chunk_set(&units, 1), vec![0, 1]);
        assert_eq!(units.len(), 6);

        let masks = build_dim_keep_masks(&g, &c, &rs).unwrap();
        assert_eq!(
            masks[0].as_ref().unwrap(),
            &[false, true, true, false, false, false, false, false, false, false, false, false]
        );
        assert_eq!(
            masks[1].as_ref().unwrap(),
            &[false, true, true, true, false, false, false]
        );
        assert!(masks[2].is_none());

        // Chunk (0,0,0): logical shape [4,3,2]. Kept: time {1,2} x lat {1,2} x level {0,1}.
        let wu = WorkUnit {
            chunk_indices: vec![0, 0, 0],
        };
        let rows = kept_rows_in_chunk(&wu, &g.chunk_shape, &[4, 3, 2], &masks).unwrap();
        let expect: Vec<usize> = [1usize, 2]
            .iter()
            .flat_map(|&t| {
                [1usize, 2]
                    .iter()
                    .flat_map(move |&l| [0usize, 1].iter().map(move |&p| t * 6 + l * 2 + p))
            })
            .collect();
        assert_eq!(rows, expect);

        // Chunk (0,1,0): lat chunk 1 = [0,-30,-60]; only lat local 0 kept.
        let wu = WorkUnit {
            chunk_indices: vec![0, 1, 0],
        };
        let rows = kept_rows_in_chunk(&wu, &g.chunk_shape, &[4, 3, 2], &masks).unwrap();
        assert_eq!(rows, vec![6, 7, 12, 13]);

        // No ranges at all -> None (fast path).
        let masks = build_dim_keep_masks(&g, &c, &[]).unwrap();
        assert!(kept_rows_in_chunk(&wu, &g.chunk_shape, &[4, 3, 2], &masks).is_none());
        // A range that keeps every row of the chunk -> None too.
        let rs = [parse_dim_range("time:0:11").unwrap()];
        let masks = build_dim_keep_masks(&g, &c, &rs).unwrap();
        assert!(kept_rows_in_chunk(&wu, &g.chunk_shape, &[4, 3, 2], &masks).is_none());
    }

    #[test]
    fn design_partial_boundary_chunk_rows() {
        // Last lat chunk holds one value (-90) of a chunk_shape 3: logical shape
        // is [4,1,2]; row indices must use the logical shape, not the chunk shape.
        let (g, c) = era5_like_group();
        let rs = [parse_dim_range("level:0:100").unwrap()];
        let masks = build_dim_keep_masks(&g, &c, &rs).unwrap();
        let wu = WorkUnit {
            chunk_indices: vec![0, 2, 2],
        };
        // level chunk 2 = [250, 50]; only local 1 kept.
        let rows = kept_rows_in_chunk(&wu, &g.chunk_shape, &[4, 1, 2], &masks).unwrap();
        assert_eq!(rows, vec![1, 3, 5, 7]);
    }
}
