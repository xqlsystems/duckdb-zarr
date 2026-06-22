# duckdb-zarr

A DuckDB extension for querying Zarr-format scientific arrays with SQL.

## What It Does

**duckdb-zarr** extends DuckDB with the ability to query Zarr arrays directly using SQL. [Zarr](https://zarr.dev/) is a format for storing chunked, compressed N-dimensional arrays, widely used in scientific computing and big data applications—particularly in climate science, astronomy, genomics, and remote sensing.

With this extension, you can:

- Query Zarr array metadata (dimensions, chunk structure, data type, attributes)
- Read array data directly into DuckDB tables
- Combine Zarr data with other data sources using standard SQL joins

### The Pivot Concept

Zarr arrays are n-dimensional gridded data (like satellite imagery, climate model outputs, or sensor readings). This extension "pivots" these multi-dimensional arrays into relational tables—treating each cell in the array as a row, with coordinates as columns. This lets you use familiar SQL to query data that would otherwise require specialized array-processing tools.

### Why This Is Useful

Scientific datasets are often stored in Zarr format because it handles multi-terabyte to petabyte-scale arrays efficiently through chunking and compression. However, traditional SQL databases don't understand Zarr. This extension bridges that gap—allowing data scientists and engineers to:

- Query Zarr metadata without loading entire arrays into memory
- Perform SQL analytics on scientific data alongside structured data
- Use DuckDB's fast SQL engine to filter and aggregate chunked array data
- Integrate Zarr data pipelines with existing SQL-based workflows

This project is related to [xarray-sql](https://github.com/alxmrs/xarray-sql) and [zarr-datafusion](https://github.com/alxmrs/zarr-datafusion), which provide similar functionality for other query engines.

## Quick Start

### Installation

**Pre-built binaries** (coming soon):
```sql
INSTALL zarr FROM community;
LOAD zarr;
```

**Build from source** (see Development Setup below)

### Basic Usage

```sql
-- Load the extension
LOAD 'duckdb_zarr';

-- Read metadata from a Zarr array
SELECT * FROM read_zarr_metadata('/path/to/my/array.zarr');

-- Read array data as a table
SELECT * FROM read_zarr('/path/to/my/array.zarr');

-- Query specific dimensions or filter data
SELECT * FROM read_zarr('/path/to/my/array.zarr') 
WHERE dimension_0 > 100 AND dimension_1 < 50;
```

For a small bioimage walkthrough, see [Querying OME-Zarr](ome-zarr.md).
For the domains covered by the current test suite, see
[Tested scientific domains](domains.md).

## Development Setup

```shell
make configure
make debug
duckdb -unsigned # run the extension
make test_debug # testing
make test_release # testing the release build
```

### Related Projects

- [xarray-sql](https://github.com/alxmrs/xarray-sql) — xarray integration for DuckDB
- [zarr-datafusion](https://github.com/alxmrs/zarr-datafusion) — Zarr support for DataFusion
- [DuckDB](https://github.com/duckdb/duckdb) — In-process SQL OLAP database
