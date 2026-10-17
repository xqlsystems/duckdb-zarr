//! The DuckDB side of Zarr reading. Engine-independent code lives in
//! `crates/zarr-core` and is re-exported under the adapter's module paths.

pub use zarr_core::{anndata, bounds, sparse, tree};
pub mod duckdb_store;
pub mod meta;
pub mod scan;
pub mod types;
