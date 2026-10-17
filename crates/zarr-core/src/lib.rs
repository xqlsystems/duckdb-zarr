//! Engine-independent Zarr reading for duckdb-zarr: store metadata, dimension
//! groups, CF decoding (packing, time), coordinate-range pruning and chunk
//! work units. No DuckDB, no Arrow: the DuckDB extension (repo root) and the
//! VGI worker (`crates/vgi-zarr`) are thin adapters that turn these into
//! DataChunks and RecordBatches respectively.

pub mod anndata;
pub mod bounds;
pub mod cftime;
pub mod consolidated_store;
pub mod decode;
pub mod meta;
pub mod sparse;
pub mod tree;
pub mod types;
