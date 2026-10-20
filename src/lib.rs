use duckdb::Connection;
use std::error::Error;

mod list_columns;
mod read_zarr;
mod read_zarr_metadata;
mod read_zarr_tables;
mod read_zarr_views;
mod replacement_scan;
mod selection;
mod zarr_reader;

unsafe fn zarr_init_c_api_internal(
    info: duckdb::ffi::duckdb_extension_info,
    access: *const duckdb::ffi::duckdb_extension_access,
) -> Result<bool, Box<dyn Error>> {
    unsafe {
        if !duckdb::ffi::duckdb_rs_extension_api_init(info, access, "v1.2.0")? {
            return Ok(false);
        }

        let get_database = (*access).get_database.ok_or("get_database is null")?;
        let db_ptr = get_database(info);
        if db_ptr.is_null() {
            return Ok(false);
        }
        let db: duckdb::ffi::duckdb_database = *db_ptr;

        // wasm has no thread spawning; build rayon's global pool with the current
        // thread as its only worker so zarrs' parallel iterators run inline.
        #[cfg(target_family = "wasm")]
        {
            let _ = rayon::ThreadPoolBuilder::new()
                .num_threads(1)
                .use_current_thread()
                .build_global();
        }

        replacement_scan::register(db);

        let con = Connection::open_from_raw(db.cast())?;
        con.register_table_function::<read_zarr::ReadZarrVTab>("read_zarr")?;
        con.register_table_function::<read_zarr_metadata::ReadZarrMetaVTab>("read_zarr_metadata")?;
        con.register_table_function::<read_zarr_tables::ReadZarrTablesVTab>("read_zarr_tables")?;
        // The 0.1.x name, from before a row was called a table; same function.
        con.register_table_function::<read_zarr_tables::ReadZarrTablesVTab>("read_zarr_groups")?;
        con.register_table_function::<read_zarr_views::ReadZarrViewsVTab>("read_zarr_views")?;

        Ok(true)
    }
}

/// # Safety
/// Entrypoint called by DuckDB.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zarr_init_c_api(
    info: duckdb::ffi::duckdb_extension_info,
    access: *const duckdb::ffi::duckdb_extension_access,
) -> bool {
    unsafe {
        match zarr_init_c_api_internal(info, access) {
            Ok(v) => v,
            Err(e) => {
                if let Some(set_error) = (*access).set_error {
                    if let Ok(msg) = std::ffi::CString::new(e.to_string()) {
                        set_error(info, msg.as_ptr());
                    }
                }
                false
            }
        }
    }
}
