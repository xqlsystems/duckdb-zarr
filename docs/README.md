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

## Guides by domain

- **Biology:** [AnnData annotated matrices](anndata.md), with a cell-by-gene
  example and a section on AnnData tables in SpatialData stores;
  [OME-Zarr bioimages](ome-zarr.md).
- **Climate and geoscience:** [gridded-data quick start](#basic-usage) below;
  [tested atmospheric and ocean datasets](domains.md#climate-and-geoscience).
- **General reference:** [design and SQL interfaces](design.md) and
  [tested scientific domains](domains.md).

## Quick Start

### Installation

**Pre-built binaries**: not published yet.

**Build from source** (see Development Setup below)

### Basic Usage

```sql
-- Load the extension
LOAD 'zarr';

-- Read metadata from a Zarr store (a local path or a URL)
SELECT * FROM read_zarr_metadata('test/fixtures/xarray_tutorial/float_baseline.zarr');

-- Read a store as a table
SELECT * FROM read_zarr('test/fixtures/xarray_tutorial/float_baseline.zarr');

-- Filter with plain SQL on the coordinate columns, dates included
SELECT time, lat, lon, air
FROM read_zarr('test/fixtures/xarray_tutorial/air_temperature.zarr')
WHERE lat > 0 AND time >= TIMESTAMP '2014-06-01';
```

Time coordinates stored the CF way — a number plus a `units` attr like
`"hours since 1800-01-01"` — are decoded to `TIMESTAMP`, which is why the query
above can compare `time` against a date literal. Pass `decode_times := false` if
you want the raw on-disk offsets instead.

For a small bioimage walkthrough, see [Querying OME-Zarr](ome-zarr.md).
For annotated matrices written by AnnData, see [Querying AnnData stores](anndata.md).
For the domains covered by the current test suite, see
[Tested scientific domains](domains.md).

### Nested groups and DataTree stores

A nested store has one set of tables per Zarr group. `read_zarr(store)` reads
only the root group; it does not combine sibling groups, even when their
arrays have identical names, dimensions and shapes. This is a change from
older releases, which pooled arrays across the store.

Discover the tables first, then select a group and its table:

```sql
SELECT group_path, schema_name, table_name, dims, data_vars
FROM read_zarr_tables('test/fixtures/xarray_tutorial/datatree.zarr')
ORDER BY group_path;

SELECT x, y, foo
FROM read_zarr('test/fixtures/xarray_tutorial/datatree.zarr',
  group_path := 'simulation/fine', table_name := 'x_y')
ORDER BY x, y;
```

Coordinates are inherited from ancestor groups, so the second query uses the
root's `x` values. Misaligned groups have a NULL `table_name` in the catalog
and cannot be read as a table. `dims := ['x', 'y']` is an alternative to
`table_name := 'x_y'`; do not pass both. Catalog columns such as `dims` and
`data_vars` are SQL lists, not JSON strings. `read_zarr_groups` remains an
alias of `read_zarr_tables`.

For arrays without dimension names, select one explicitly with `array_path :=`.
Its unnamed axes become integer-position columns `dim_0`, `dim_1`, and so on.
Group tables omit these arrays rather than joining unrelated data by position.
AnnData has its own axis naming and decoded tables; see the
[AnnData guide](anndata.md).

To mount all readable tables as views, call `read_zarr_views(store, database)`
and execute the returned SQL statements on the same connection. It returns
SQL; it does not execute the statements itself. See the
[Python and CLI mounting recipes](design.md#3-mounting-multi-table-stores-attach-and-read_zarr_views).

### OME-Zarr Features

OME-Zarr stores often contain nested arrays for resolution levels and labels.
Use metadata discovery first to see the available store-relative paths:

```sql
SELECT name, dims, shape, dtype
FROM read_zarr_metadata('test/fixtures/bioimage/ome_zarr/synthetic_multichannel.ome.zarr');
```

Then pass `array_path=` when you want a specific image or label array:

```sql
SELECT c, AVG(value) AS mean_intensity
FROM read_zarr('test/fixtures/bioimage/ome_zarr/synthetic_multichannel.ome.zarr', array_path='0')
GROUP BY c;

SELECT value AS label, COUNT(*) AS pixels
FROM read_zarr('test/fixtures/bioimage/ome_zarr/synthetic_multichannel.ome.zarr', array_path='labels/nuclei/0')
WHERE value > 0
GROUP BY label;
```

`array_path` is optional for simple stores where the reader can infer a single
compatible array group. It becomes useful for multiscale images, nested labels,
or stores with multiple incompatible array shapes.

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
