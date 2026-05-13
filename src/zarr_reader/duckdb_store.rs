use std::ffi::CString;

use zarrs::storage::{
    byte_range::ByteRangeIterator, Bytes, MaybeBytesIterator, ReadableStorageTraits,
    StorageError, StoreKey,
};

use duckdb::ffi::{
    duckdb_create_file_open_options, duckdb_destroy_file_handle, duckdb_destroy_file_open_options,
    duckdb_destroy_file_system, duckdb_file_flag_DUCKDB_FILE_FLAG_READ, duckdb_file_handle,
    duckdb_file_handle_close, duckdb_file_handle_read, duckdb_file_handle_seek,
    duckdb_file_handle_size, duckdb_file_open_options_set_flag, duckdb_file_system,
    duckdb_file_system_open, DuckDBSuccess,
};

/// A read-only zarrs store backed by DuckDB's FileSystem API.
///
/// Forwards all key reads to DuckDB's internal filesystem, which transparently handles
/// S3, GCS, Azure Blob, and local paths — including credentials via the secrets manager.
pub struct DuckDbStore {
    file_system: duckdb_file_system,
    base_path: String,
}

// SAFETY: DuckDB's FileSystem uses internal locking; the raw pointer is not mutated
// after construction. All access goes through the stable C API.
unsafe impl Send for DuckDbStore {}
unsafe impl Sync for DuckDbStore {}

impl Drop for DuckDbStore {
    fn drop(&mut self) {
        unsafe { duckdb_destroy_file_system(&mut self.file_system) }
    }
}

impl DuckDbStore {
    pub fn new(file_system: duckdb_file_system, base_path: &str) -> Self {
        Self {
            file_system,
            base_path: base_path.trim_end_matches('/').to_string(),
        }
    }

    fn key_path(&self, key: &StoreKey) -> Result<CString, StorageError> {
        let path = format!("{}/{}", self.base_path, key.as_str());
        CString::new(path).map_err(|e| StorageError::Other(e.to_string()))
    }

    /// Opens a file for reading. Returns `None` if the key does not exist.
    unsafe fn open_read(
        &self,
        key: &StoreKey,
    ) -> Result<Option<duckdb_file_handle>, StorageError> {
        let path_cstr = self.key_path(key)?;
        let opts = duckdb_create_file_open_options();
        duckdb_file_open_options_set_flag(opts, duckdb_file_flag_DUCKDB_FILE_FLAG_READ, true);
        let mut handle: duckdb_file_handle = std::ptr::null_mut();
        let state =
            duckdb_file_system_open(self.file_system, path_cstr.as_ptr(), opts, &mut handle);
        let mut opts_owned = opts;
        duckdb_destroy_file_open_options(&mut opts_owned);
        if state != DuckDBSuccess || handle.is_null() {
            Ok(None)
        } else {
            Ok(Some(handle))
        }
    }
}

impl ReadableStorageTraits for DuckDbStore {
    fn get_partial_many<'a>(
        &'a self,
        key: &StoreKey,
        byte_ranges: ByteRangeIterator<'a>,
    ) -> Result<MaybeBytesIterator<'a>, StorageError> {
        let handle = match unsafe { self.open_read(key)? } {
            None => return Ok(None),
            Some(h) => h,
        };
        let size = unsafe { duckdb_file_handle_size(handle) as u64 };

        let ranges: Vec<(u64, u64)> = byte_ranges
            .map(|br: zarrs::storage::byte_range::ByteRange| (br.start(size), br.length(size)))
            .collect();

        let mut results: Vec<Result<Bytes, StorageError>> = Vec::with_capacity(ranges.len());
        for (offset, length) in ranges {
            let state = unsafe { duckdb_file_handle_seek(handle, offset as i64) };
            if state != DuckDBSuccess {
                results.push(Err(StorageError::Other(format!(
                    "seek to offset {offset} failed"
                ))));
                continue;
            }
            let mut buf = vec![0u8; length as usize];
            unsafe {
                duckdb_file_handle_read(handle, buf.as_mut_ptr().cast(), length as i64);
            }
            results.push(Ok(Bytes::from(buf)));
        }

        unsafe {
            duckdb_file_handle_close(handle);
            let mut h = handle;
            duckdb_destroy_file_handle(&mut h);
        }

        Ok(Some(Box::new(results.into_iter())))
    }

    fn size_key(&self, key: &StoreKey) -> Result<Option<u64>, StorageError> {
        let handle = match unsafe { self.open_read(key)? } {
            None => return Ok(None),
            Some(h) => h,
        };
        let size = unsafe { duckdb_file_handle_size(handle) as u64 };
        unsafe {
            duckdb_file_handle_close(handle);
            let mut h = handle;
            duckdb_destroy_file_handle(&mut h);
        }
        Ok(Some(size))
    }

    fn supports_get_partial(&self) -> bool {
        true
    }
}
