use duckdb::Connection;
use std::error::Error;

mod read_zarr;
mod read_zarr_groups;
mod read_zarr_metadata;
mod replacement_scan;
mod zarr_reader;

unsafe fn duckdb_zarr_init_c_api_internal(
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

        replacement_scan::register(db);

        let con = Connection::open_from_raw(db.cast())?;
        con.register_table_function::<read_zarr::ReadZarrVTab>("read_zarr")?;
        con.register_table_function::<read_zarr_metadata::ReadZarrMetaVTab>("read_zarr_metadata")?;
        con.register_table_function::<read_zarr_groups::ReadZarrGroupsVTab>("read_zarr_groups")?;

        Ok(true)
    }
}

/// # Safety
/// Entrypoint called by DuckDB.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_zarr_init_c_api(
    info: duckdb::ffi::duckdb_extension_info,
    access: *const duckdb::ffi::duckdb_extension_access,
) -> bool {
    unsafe {
        match duckdb_zarr_init_c_api_internal(info, access) {
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
