//! Store metadata from `zarr_core::meta`, plus opening a store through
//! DuckDB: the only part that needs a DuckDB FileSystem (s3/gs/az, and every
//! path on duckdb-wasm).

use std::sync::Arc;

#[cfg(not(target_family = "wasm"))]
use duckdb::ffi::duckdb_destroy_file_system;
use duckdb::ffi::{
    duckdb_client_context, duckdb_client_context_get_file_system, duckdb_destroy_client_context,
    duckdb_file_system, duckdb_table_function_get_client_context,
};
#[cfg(not(target_family = "wasm"))]
use zarrs::filesystem::FilesystemStore;

pub use zarr_core::meta::*;

use super::duckdb_store::DuckDbStore;

/// Extract a DuckDB FileSystem handle from a table-function BindInfo.
///
/// The returned handle must be passed to [`open_store`], which either adopts it (remote stores)
/// or destroys it immediately (local / HTTP stores).
///
/// # Safety
/// Must be called from within a DuckDB table-function bind callback.
pub unsafe fn extract_file_system(bind: &duckdb::vtab::BindInfo) -> duckdb_file_system {
    let raw = raw_bind_info(bind);
    let mut ctx: duckdb_client_context = std::ptr::null_mut();
    duckdb_table_function_get_client_context(raw, &mut ctx);
    let fs = duckdb_client_context_get_file_system(ctx);
    duckdb_destroy_client_context(&mut ctx);
    fs
}

/// The C API handle inside a duckdb-rs `BindInfo`, for the bind calls
/// duckdb-rs does not wrap.
///
/// # Safety
/// Must be called from within a DuckDB table-function bind callback.
pub unsafe fn raw_bind_info(bind: &duckdb::vtab::BindInfo) -> duckdb::ffi::duckdb_bind_info {
    // SAFETY: BindInfo is `struct BindInfo { ptr: duckdb_bind_info }` — one pointer-sized field,
    // no padding, offset 0. transmute_copy reads those pointer bytes as a duckdb_bind_info.
    // The assert catches any future duckdb-rs version that adds fields to BindInfo.
    const _: () = assert!(
        std::mem::size_of::<duckdb::vtab::BindInfo>()
            == std::mem::size_of::<duckdb::ffi::duckdb_bind_info>()
    );
    std::mem::transmute_copy(bind)
}

/// Open a Zarr store.
///
/// - HTTP/HTTPS → `zarrs_http::HTTPStore` (no DuckDB filesystem needed)
/// - S3/GCS/Azure → `DuckDbStore` backed by the provided `file_system` handle
///   (the store takes ownership and destroys it on drop)
/// - Local path → `zarrs::FilesystemStore` (destroys the handle if provided)
/// - wasm32: every path → `DuckDbStore` (no `zarrs_http`, no host filesystem)
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
    // On wasm there is no reqwest::blocking and no host filesystem: every path
    // (HTTP included, and files registered in duckdb-wasm's virtual FS) is read
    // through DuckDB's own FileSystem via DuckDbStore.
    #[cfg(target_family = "wasm")]
    let store: ZarrStore = {
        let _ = &lower;
        let fs = file_system.ok_or(
            "wasm build requires a DuckDB FileSystem handle (call from a table function bind)",
        )?;
        Arc::new(DuckDbStore::new(fs, path))
    };
    #[cfg(not(target_family = "wasm"))]
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
