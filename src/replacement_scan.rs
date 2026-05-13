use std::ffi::{CStr, CString};
use std::path::Path;

use duckdb::ffi::{
    duckdb_add_replacement_scan, duckdb_create_varchar, duckdb_database,
    duckdb_replacement_scan_add_parameter, duckdb_replacement_scan_info,
    duckdb_replacement_scan_set_function_name,
};

/// Register the replacement scan on `db` so that bare paths ending in `.zarr`
/// (or containing a `zarr.json` / `.zgroup` at the root) are rewritten to
/// `read_zarr('<path>')`.
pub unsafe fn register(db: duckdb_database) {
    unsafe {
        duckdb_add_replacement_scan(db, Some(zarr_replacement_scan), std::ptr::null_mut(), None);
    }
}

unsafe extern "C" fn zarr_replacement_scan(
    info: duckdb_replacement_scan_info,
    table_name: *const std::os::raw::c_char,
    _data: *mut std::os::raw::c_void,
) {
    unsafe {
        let name = match CStr::from_ptr(table_name).to_str() {
            Ok(s) => s,
            Err(_) => return,
        };

        if !looks_like_zarr(name) {
            return;
        }

        let fn_name = c"read_zarr";
        duckdb_replacement_scan_set_function_name(info, fn_name.as_ptr());

        let path_cstr = match CString::new(name) {
            Ok(s) => s,
            Err(_) => return,
        };
        let val = duckdb_create_varchar(path_cstr.as_ptr());
        duckdb_replacement_scan_add_parameter(info, val);
    }
}

fn looks_like_zarr(name: &str) -> bool {
    let trimmed = name.trim_end_matches('/');
    let lower = trimmed.to_ascii_lowercase();

    // Remote URIs: trust the .zarr suffix; probing requires a live DuckDB connection.
    if lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("s3://")
        || lower.starts_with("gs://")
        || lower.starts_with("az://")
    {
        return lower.ends_with(".zarr");
    }

    // Design spec §replacement-scan: two-step probe for local paths.
    // Step 1: suffix check (case-insensitive). Step 2: zarr.json/.zgroup stat.
    // Suffix-less paths are NOT probed — that would stat every SQL identifier.
    if lower.ends_with(".zarr") {
        let p = Path::new(trimmed);
        return p.join("zarr.json").exists() || p.join(".zgroup").exists();
    }

    false
}

#[cfg(test)]
mod tests {
    use super::looks_like_zarr;

    #[test]
    fn uppercase_zarr_suffix_not_claimed_if_missing() {
        // .ZARR suffix without existing directory → false (stat fails)
        assert!(!looks_like_zarr("/tmp/NONEXISTENT_PATH.ZARR"));
        assert!(!looks_like_zarr("/tmp/NONEXISTENT_PATH.zarr"));
    }

    #[test]
    fn suffix_less_paths_not_claimed() {
        // Bare SQL identifiers must never trigger a stat.
        assert!(!looks_like_zarr("my_table"));
        assert!(!looks_like_zarr("some_schema.my_table"));
        assert!(!looks_like_zarr("SELECT"));
    }

    #[test]
    fn trailing_slash_stripped_before_suffix_check() {
        // Should not claim a non-existent path even with trailing slash.
        assert!(!looks_like_zarr("/tmp/NONEXISTENT.zarr/"));
    }
}
