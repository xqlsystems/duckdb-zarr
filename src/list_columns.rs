//! List-typed output columns for the catalog table functions
//! (`read_zarr_tables`, `read_zarr_metadata`): `VARCHAR[]` for names and
//! `UBIGINT[]` for shapes.

use duckdb::core::{DataChunkHandle, Inserter, LogicalTypeHandle, LogicalTypeId};

/// The `VARCHAR[]` type.
pub fn varchar_list() -> LogicalTypeHandle {
    LogicalTypeHandle::list(&LogicalTypeId::Varchar.into())
}

/// The `UBIGINT[]` type.
pub fn ubigint_list() -> LogicalTypeHandle {
    LogicalTypeHandle::list(&LogicalTypeId::UBigint.into())
}

/// Write one `VARCHAR[]` value per row into column `col`.
pub fn write_string_lists<S: AsRef<str>>(output: &DataChunkHandle, col: usize, rows: &[&[S]]) {
    let mut list = output.list_vector(col);
    let total = rows.iter().map(|r| r.len()).sum();
    let child = list.child(total);
    let mut offset = 0;
    for (row, items) in rows.iter().enumerate() {
        for (i, item) in items.iter().enumerate() {
            child.insert(offset + i, item.as_ref());
        }
        list.set_entry(row, offset, items.len());
        offset += items.len();
    }
    list.set_len(total);
}

/// Write one `UBIGINT[]` value per row into column `col`.
pub fn write_u64_lists(output: &DataChunkHandle, col: usize, rows: &[&[u64]]) {
    let mut list = output.list_vector(col);
    let total = rows.iter().map(|r| r.len()).sum();
    let mut child = list.child(total);
    // SAFETY: the child of a UBIGINT[] holds u64 values, and `child(total)`
    // reserved room for `total` of them.
    let values = unsafe { child.as_mut_slice_with_len::<u64>(total) };
    let mut offset = 0;
    for (row, items) in rows.iter().enumerate() {
        values[offset..offset + items.len()].copy_from_slice(items);
        list.set_entry(row, offset, items.len());
        offset += items.len();
    }
    list.set_len(total);
}
