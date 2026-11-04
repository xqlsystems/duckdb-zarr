# How duckdb-zarr works

`duckdb-zarr` is a [DuckDB](https://duckdb.org/) extension that reads
[Zarr](https://zarr.dev/) stores as SQL tables. You point a query at a Zarr
store on disk or at a URL, and the arrays inside it come back as rows and
columns that you can filter, aggregate and join with any other data DuckDB can
read: CSV files, Parquet files, other databases.

This document explains the design: how an N-dimensional array becomes a table,
which conventions the reader follows, what happens when a query runs, and why
the main decisions went the way they did. It describes the extension as it is
built today, including its limits, and ends with what is planned.

It is written for two groups of readers. If you work with scientific arrays
(climate model output, microscopy images, single-cell matrices) and know
xarray or NumPy better than SQL, start with [Background for array
users](#duckdb-and-sql-for-array-users). If you know SQL and DuckDB but have
not met Zarr, start with [Background for SQL users](#zarr-for-sql-users).
Contributors will find a map of the source code at the end.

For task-oriented guides, see [Querying AnnData stores](anndata.md) and
[Querying OME-Zarr](ome-zarr.md).

## Background

### Zarr, for SQL users

Zarr is a storage format for N-dimensional arrays. An array is a grid of values
of one type: a 2-D array of 16-bit integers can hold an image, a 3-D array of
floats might hold air temperature over time, latitude and longitude. The number of
dimensions (axes) is the array's *rank*, and the length along each axis is its
*shape*: a temperature array with shape `(2920, 25, 53)` holds 2,920 time steps
on a grid of 25 latitudes by 53 longitudes, 3.9 million values in all.

Zarr splits each array into **chunks**, rectangular blocks of equal shape,
and stores each chunk as a separate compressed object: a file on disk, or an
object in cloud storage such as Amazon S3. The temperature array above might
be chunked `(730, 13, 27)`, so one chunk covers 730 time steps of one quarter
of the grid. A reader that needs only part of an array fetches and
decompresses only the chunks that overlap that part. This is why Zarr works
well for data far larger than memory, read over a network. The chunks at the
far edge of an array can extend past its end; the extra positions hold the
array's *fill value*. A chunk that was never written also reads as the fill
value, so a mostly empty array costs little storage.

The compression and byte layout of each chunk are described by a list of
**codecs** (gzip, zstd, Blosc, and others) in the array's metadata.

A Zarr **store** is a hierarchy, like a directory tree. Arrays live in
**groups**, and groups can contain other groups. Each array and each group has
a small JSON metadata document, and can carry **attributes**: arbitrary JSON
key-value pairs that tools use to record units, descriptions and conventions.

There are two versions of the format. Zarr v2 keeps an array's metadata in a
`.zarray` file and its attributes in `.zattrs`. Zarr v3 keeps both in one
`zarr.json` file. Object stores such as S3 cannot list the contents of a
"directory" cheaply, so many published stores also carry **consolidated
metadata**: one document at the root that contains the metadata of every array
and group. In v2 this is a `.zmetadata` file; in v3 it is a block inside the
root `zarr.json`.

### xarray and the CF conventions

Zarr itself only stores arrays. The meaning of their axes comes from
conventions layered on top, and the most widespread is
[xarray](https://xarray.dev/)'s, a Python library for labelled N-dimensional
data.

In xarray's model, every axis of an array has a **dimension name**, such as
`time`, `lat` or `lon`. Arrays that share a dimension name share that axis.
A **coordinate** is a 1-D array that labels the positions along a dimension
and has the dimension's name: the `lat` array holds the 25 latitudes of the
`lat` dimension. Most other arrays are **data variables**: `air(time, lat,
lon)` is a data variable over three dimensions. A collection of data variables
and coordinates is an xarray `Dataset`. A tree of datasets, one per Zarr group,
is an xarray `DataTree`.

When xarray writes Zarr, it stores dimension names in the array's
`_ARRAY_DIMENSIONS` attribute (v2) or in the `dimension_names` field of
`zarr.json` (v3).

The [CF conventions](https://cfconventions.org/) (Climate and Forecast) are a
standard set of attributes from the climate and ocean sciences that xarray and
NetCDF tools follow. The ones that matter here:

- **Missing values.** A `_FillValue` or `missing_value` attribute names a
  sentinel value (for example `-9999`) that marks a cell as missing.
- **Packing.** An integer array with `scale_factor` and `add_offset` attributes
  stores a real-valued measurement compactly. The true value is
  `stored * scale_factor + add_offset`. Air temperature stored as the 16-bit
  integer `27315` with a scale factor of `0.01` is `273.15` kelvin.
- **Time.** A time coordinate is often stored as numbers with a `units`
  attribute such as `"hours since 1900-01-01"` and a `calendar` attribute such
  as `"standard"` or `"noleap"` (a 365-day calendar some climate models use).
- **Bounds.** A coordinate can point, through a `bounds` attribute, to a
  companion array of shape `(N, 2)` holding the start and end of each cell,
  such as `time_bnds`.
- **Auxiliary coordinates.** A data variable's `coordinates` attribute can
  name extra coordinate arrays that are not indexed by a single dimension,
  such as a 2-D `lat(y, x)` on a curvilinear ocean grid.

Two other communities define their own layouts on Zarr, and the extension
understands both. [OME-Zarr](https://ngff.openmicroscopy.org/) stores
microscopy images as a *multiscale pyramid*: arrays `0`, `1`, `2`, ... hold
the same image at decreasing resolution, and the group's `multiscales`
attribute names the axes (`c`, `z`, `y`, `x` for channel, depth and the two
image axes). [AnnData](https://anndata.readthedocs.io/) stores annotated
matrices, most often single-cell gene expression: a matrix `X` of
observations (cells) by variables (genes), with tables of annotations for each
axis.

### DuckDB and SQL, for array users

SQL is the language of relational databases. Its unit of data is the
**table**: a set of **rows**, each with the same named, typed **columns**. A
query describes the result you want and the database works out how to compute
it:

```sql
SELECT lat, AVG(air) AS mean_air
FROM some_table
WHERE time >= TIMESTAMP '2014-01-01'
GROUP BY lat
ORDER BY lat;
```

`SELECT` picks the output columns, `FROM` names the input, `WHERE` keeps the
rows that match a condition, and `GROUP BY` collapses rows that share a value
into one row per group, with aggregates such as `AVG`, `SUM` and `COUNT`. A
`JOIN` combines rows of two tables that match on some columns. Rows in a table
have no inherent order, so a query that needs one says `ORDER BY`. A missing
value in SQL is `NULL`. It is not the same as a floating-point `NaN`:
aggregates skip `NULL`, but a single `NaN` makes `AVG` return `NaN`.

DuckDB is a SQL database that runs inside your program (a Python session, a
command-line shell, a web page) rather than as a separate server. It is built
for analytical queries over large tables, and it reads files in place: `FROM
'data.parquet'` queries a Parquet file without importing it first.

**Extensions** add features to DuckDB at run time; this project is one.
Extensions can add **table functions**, which are functions that return a
table and can appear in `FROM` like a table. `read_zarr('store.zarr')` is a
table function. Table functions take **named parameters**, written `name :=
value` (DuckDB also accepts `name = value`). An extension can also add a
**replacement scan**, which is a hook that turns a string in `FROM`, such as
`FROM 'store.zarr'`, into a call to one of its functions.

A DuckDB **database** holds **schemas**, and a schema holds tables and
**views**. A view is a stored query that you can select from like a table.
`ATTACH` adds a database to a session, so `era5.main.surface` names the view
`surface` in schema `main` of the attached database `era5`.

DuckDB processes data in **vectors**: batches of values for one column, by
default up to 2,048 of them. A table function fills one batch of rows per call, and DuckDB can call
it from several threads at once.

## From arrays to tables

### One row per cell

The extension turns arrays into a table by giving each cell its own row. Each
dimension becomes a column holding that cell's coordinate value, and each data
variable becomes a column holding that cell's value. Take a store with
coordinates `time` (2 values), `lat` (2 values) and `lon` (3 values), and two
data variables `temperature(time, lat, lon)` and `humidity(time, lat, lon)`.
It reads as one table with 2 × 2 × 3 = 12 rows:

| time | lat | lon | temperature | humidity |
|---|---|---|---|---|
| 2024-01-01 00:00 | 10.0 | 0.0 | 273.15 | 0.81 |
| 2024-01-01 00:00 | 10.0 | 0.5 | 273.42 | 0.79 |
| 2024-01-01 00:00 | 10.0 | 1.0 | 273.60 | 0.78 |
| 2024-01-01 00:00 | 10.5 | 0.0 | 272.90 | 0.83 |
| ... | | | | |

This "long" layout is what lets plain SQL work on gridded data. A spatial
average is `GROUP BY lat, lon`, a time series at a point is `WHERE lat = 10.0
AND lon = 0.5`, and joining to a table of weather stations is an ordinary
`JOIN` on `lat` and `lon`. It is the same layout as xarray's
`Dataset.to_dataframe()` and the [xarray-sql](https://github.com/alxmrs/xarray-sql)
project, which does the same job for the DataFusion query engine.

The coordinate columns repeat a lot: each `lat` value appears once for every
combination of `time` and `lon`. The extension never builds this table in
memory. It generates the rows chunk by chunk as DuckDB asks for them (see
[How a query runs](#how-a-query-runs)), so the cost of a query grows with the
chunks it reads, not with the size of the product.

A dimension with no coordinate array still gets a column. It holds the
position along that axis, `0` to `N - 1`, as a `BIGINT`.

### One table per set of dimensions

Variables over different dimensions cannot share rows. A weather reanalysis
such as ERA5 has surface variables over `(time, latitude, longitude)` and
atmospheric variables over `(time, level, latitude, longitude)`, where `level`
is a pressure level. Putting both in one table would repeat each surface value
once per level, or fill most of the table with `NULL`.

So a group's data variables are split into **tables**, one for each set of
dimension names (and shape) they share. The test fixture `era5_like.zarr`
reads as two tables:

| Table | Dimensions | Columns |
|---|---|---|
| `time_latitude_longitude` | time, latitude, longitude | `10m_u_component_of_wind`, `2m_temperature`, `total_precipitation` |
| `time_level_latitude_longitude` | time, level, latitude, longitude | `geopotential`, `temperature` |

A table's name is its dimension names joined with underscores, in order.
xarray-sql uses the same rule, so both projects give a store the same table
names. Two tables in one group
whose names differ only in ASCII letter case get no name (DuckDB identifiers
ignore case); `dims :=` can still read them.

Arrays that are metadata rather than data are kept out of tables: bounds
arrays, auxiliary coordinates, and 0-dimensional constants, such as the
physical parameters some ocean models store as arrays. (Inside AnnData
objects, only the last rule applies, since the CF attributes have no meaning
there.) They still appear in [`read_zarr_metadata`](#inspecting-a-store).

### Groups become schemas

A nested store has one set of tables per group, the way an xarray `DataTree`
has one dataset per node. `read_zarr(store)` reads the root group, like
`xarray.open_zarr(store)`; `group_path :=` selects another group, like
xarray's `group=` argument. Arrays in different groups never share a table,
even if their names and dimensions match, because sibling groups often reuse
dimension names with different lengths: the levels of a simulation at two
resolutions, or the members of an ensemble.

A group can use the coordinates of its ancestors. If the root has an `x`
coordinate and `simulation/fine` has `foo(x, y)` but no `x` array, the
`simulation/fine` table takes its `x` column from the root. As in xarray, a
group whose dimension lengths disagree with its parent's is *misaligned*, and
its tables cannot be read as tables (single arrays in it still can, through
`array_path :=`).

When a store is mounted as a DuckDB database (see [Mounting a
store](#mounting-a-store-as-views)), each group becomes a schema: the root is
`main`, and a nested group is a schema named by its path, quoted because it
contains a slash: `sim."simulation/fine".x_y`.

### Arrays without dimension names

Some writers record no dimension names. Plain zarr-python does not, and
neither does AnnData. Such arrays are left out of tables. The extension does
not guess, because a guess could be silently wrong: if every unnamed 1-D array
of length 500 got the dimension name `dim_0`, two unrelated arrays would end
up in one table, joined row by row, with no error.

You can still read one such array by its path with `array_path :=`. Its axes
are then named `dim_0`, `dim_1`, and so on, which is safe because only one
array is involved. If a group has no other data, `read_zarr` fails with an
error that names the unnamed arrays and suggests `array_path :=`.

An array that names some of its axes but leaves others empty (a `null` entry
in `dimension_names`) is treated differently: the empty entries become
`dim_<position>` even in table reads, so two such arrays can end up sharing a
`dim_0` axis. See [Known limits](#known-limits).

Two layouts supply names that the arrays themselves lack. In OME-Zarr, an
array takes its axis names from its group's `multiscales` attribute, so a
microscopy image reads with `c`, `z`, `y`, `x` columns. The levels of a
pyramid share those names but not their shapes, so each level is read on its
own with `array_path :=`. For AnnData, the extension names each axis from the element's place
in the AnnData object, described next.

### AnnData stores

An AnnData object is a group with the attribute `encoding-type: anndata`. It
may be the root of a store written by `AnnData.write_zarr()`, a table inside a
[SpatialData](https://spatialdata.scverse.org/) store (`tables/<name>`), or a
modality of a MuData store (`mod/<name>`). Inside one, the extension applies
three extra rules on top of the general model.

First, it names the axes. The observation axis is `obs` and the variable axis
is `var`, so `X`, the main matrix, has dimensions `(obs, var)`. AnnData keeps the
annotations of each axis in a data frame, also named `obs` and `var`. Their
columns are read as tables of the AnnData group itself, beside `X`, so the
root of an AnnData store has the tables `obs`, `var` and `obs_var`. Each data
frame's index (cell or gene names) becomes the coordinate of its axis, so every table can be filtered and joined by name:
`WHERE obs = 'AAACATACAACCAC-1' AND var = 'MKI67'`.

Second, it decodes AnnData's encoded columns. AnnData stores some columns as a
group of arrays. A categorical column is integer codes plus a list of
categories, and reads as a DuckDB `ENUM` (or as the categories' own type, when
they are not distinct strings). A nullable column is values plus a mask, and
reads with `NULL` where the mask is set.

Third, it reads sparse matrices as their stored entries. A sparse matrix
(`csr_matrix` or `csc_matrix`) stores only its nonzero entries. A table that
holds only sparse matrices has one row per stored entry, not one row per
cell: expanding a 2,700 × 32,738 matrix with 2.3 million stored values would
produce 88 million rows. Queries that average over cells must account for the
zeros a sparse table omits. The [AnnData guide](anndata.md) has the full
mapping of elements to tables and examples of these queries.

## The SQL interface

### `read_zarr`

`read_zarr(store)` reads one table. The store is a local path or a URL.

```sql
SELECT * FROM read_zarr('test/fixtures/xarray_tutorial/float_baseline.zarr');
```

If the group being read has tables over more than one set of dimensions,
`read_zarr` needs to know which one. Otherwise it fails with an error such as `group '/' has 2 tables
[...]; use table_name= or dims= to select one, or array_path= to read one
array`.

| Parameter | Meaning |
|---|---|
| `table_name := 'time_lat_lon'` | Read the table with this name, as `read_zarr_tables` lists it. Letter case is ignored. Alias `"table"`. |
| `dims := ['time', 'lat', 'lon']` | Read the table with these dimension names, in this order. An alternative to `table_name`; do not pass both. |
| `group_path := 'simulation/fine'` | Read from this group instead of the root. Slashes at either end are optional. Alias `"group"`. |
| `array_path := 'labels/nuclei/0'` | Read one array by its path in the store. Its dimensions are its own, an OME-Zarr image's axes, AnnData's axis names, or `dim_0`, `dim_1`, .... Alias `"array"`. Cannot be combined with `group_path` or `table_name`. |
| `bounds := [['lat', '-30', '30'], ...]` | Read only coordinate ranges. A workaround until `WHERE` is pushed down; see [Selecting coordinate ranges](#selecting-coordinate-ranges). |
| `decode_times := false` | Leave CF time coordinates as their stored numbers instead of `TIMESTAMP`. The default is `true`, as in `xarray.open_zarr`. |

The aliases `"table"`, `"group"` and `"array"` need double quotes because
`TABLE`, `GROUP` and `ARRAY` are SQL keywords.

The columns of a table are its dimensions, in order, then its data variables.
In a table read, a column is named after the variable within its group
(`foo`), not its path in the store (`simulation/fine/foo`); with `array_path
:=`, the path is the name. When the only data column's name would be a number
or a path, as with an OME-Zarr resolution level `0`, it is named `value`
instead, so you do not have to quote it:

```sql
SELECT c, AVG(value) AS mean_intensity
FROM read_zarr('test/fixtures/bioimage/ome_zarr/synthetic_multichannel.ome.zarr',
               array_path := '0')
GROUP BY c;
```

### Querying a path directly

A string ending in `.zarr` in `FROM` is read with `read_zarr`:

```sql
SELECT lat, lon, AVG(temperature)
FROM 'test/fixtures/xarray_tutorial/float_baseline.zarr'
GROUP BY lat, lon;
```

The check ignores letter case and a trailing slash, so `s3://bucket/store.zarr/`
works too. For a local path, the extension also checks that a `zarr.json` or
`.zgroup` file exists at that path, so that a directory merely named `.zarr`
falls through to DuckDB's other readers. For URLs (`http://`, `https://`,
`s3://`, `gs://`, `az://`) the suffix alone decides, because checking would
cost a network request. Paths without the `.zarr` suffix are never claimed;
read them with `read_zarr(path)`. This form takes no parameters, so it only
works when the store's root group has a single table.

### Inspecting a store

Two table functions describe a store without reading any chunk data.

`read_zarr_metadata(store)` lists every array, one row each:

| Column | Type | Meaning |
|---|---|---|
| `name` | `VARCHAR` | Path of the array in the store |
| `dims` | `VARCHAR[]` | Declared dimension names (empty if none) |
| `dtype` | `VARCHAR` | Zarr data type |
| `shape`, `chunk_shape` | `UBIGINT[]` | Array shape and chunk shape |
| `attrs` | `VARCHAR` | Attributes, as JSON text |
| `role` | `VARCHAR` | `coord`, `data`, `aux_coord`, `bounds`, `scalar` or `unsupported` |
| `array_path_dims` | `VARCHAR[]` | The dimension names `read_zarr(store, array_path := name)` would use |

An array that the underlying Zarr library cannot open, for example because of
an unsupported data type or codec, is listed with role and dtype
`unsupported` and the error text in `attrs`, instead of failing the call.

`read_zarr_tables(store)` lists every table in every group, one row each:

| Column | Type | Meaning |
|---|---|---|
| `group_path` | `VARCHAR` | The group, as a path (`/`, `/simulation/fine`) |
| `schema_name` | `VARCHAR` | The schema the group mounts as (`main` for the root) |
| `table_name` | `VARCHAR` | The table's name, or `NULL` if it cannot be read as one table |
| `dims` | `VARCHAR[]` | Dimension names |
| `shape` | `UBIGINT[]` | Length of each dimension |
| `chunk_shape` | `UBIGINT[]` | The chunk grid the scan uses (see [Mixed chunk shapes](#mixed-chunk-shapes)) |
| `data_vars`, `coord_vars` | `VARCHAR[]` | Paths of the arrays (or AnnData encoded groups) that supply its columns |

The list columns can be searched with DuckDB's list functions:

```sql
SELECT table_name
FROM read_zarr_tables('test/fixtures/xarray_tutorial/era5_like.zarr')
WHERE list_contains(data_vars, 'temperature');
```

A `NULL` `table_name` means the row is listed for inspection but `read_zarr`
cannot read it by name: its group is misaligned, it shares dimension names
with an array of a different shape (as the levels of an OME-Zarr pyramid do),
it contains an array that cannot be opened, it mixes CSR and CSC sparse
matrices, or its name collides with another table's after case folding. Only
the last case can be read with `dims :=`; for the others, read arrays one at a
time with `array_path :=`.

Both functions accept `group_path :=` and `array_path :=` to narrow the
listing. `read_zarr_groups` is the older name of `read_zarr_tables` and still
works.

### Mounting a store as views

For repeated queries it is convenient to give each table a short, stable
name. `read_zarr_views(store, database)` returns the SQL statements that
mount a store as a DuckDB database: one `ATTACH` of an in-memory database, one
`CREATE SCHEMA` per nested group that has a named table, and one `CREATE
VIEW` per named table. You
run the statements yourself. The views store no data; each one calls
`read_zarr` when queried.

```sql
SELECT sql FROM read_zarr_views('test/fixtures/xarray_tutorial/era5_like.zarr', 'era5',
  view_names := MAP {'time_latitude_longitude': 'surface',
                     'time_level_latitude_longitude': 'atmosphere'});
-- ATTACH ':memory:' AS "era5";
-- CREATE OR REPLACE VIEW "era5"."main"."surface" AS SELECT * FROM read_zarr('...', table_name := 'time_latitude_longitude');
-- CREATE OR REPLACE VIEW "era5"."main"."atmosphere" AS SELECT * FROM read_zarr('...', table_name := 'time_level_latitude_longitude');
```

`view_names` renames views. Its keys are table names for tables in the root
group and `group/path/table_name` for the others. A key that names no table,
or a rename that gives two views in one schema the same name, is an error.
Tables with a `NULL` `table_name` get no view. The `ATTACH` fails if the
database name is already in use; detach the old mount first or pick another
name.

From Python, run each returned row:

```python
mount = """
SELECT sql FROM read_zarr_views('test/fixtures/xarray_tutorial/era5_like.zarr', 'era5',
  view_names := MAP {'time_level_latitude_longitude': 'atmosphere'})
"""
for (sql,) in con.sql(mount).fetchall():
    con.execute(sql)
```

In the DuckDB command-line shell, write the rows to a file and read it back:

```
.bail on
.mode list
.headers off
.once mount.sql
SELECT sql FROM read_zarr_views('test/fixtures/xarray_tutorial/era5_like.zarr', 'era5',
  view_names := MAP {'time_level_latitude_longitude': 'atmosphere'});
.read mount.sql
```

After mounting, `SELECT level, AVG(temperature) FROM era5.atmosphere GROUP BY
level` works like a query on any other table. Without `view_names`, the same
view is `era5.main.time_level_latitude_longitude`.

### Selecting coordinate ranges

`bounds :=` is a workaround. The C API the extension is built on gives a
table function no access to a query's `WHERE` conditions, so a `WHERE` clause
on a coordinate column gives the right answer without making the read
smaller: the extension reads every chunk and DuckDB discards the rows that do
not match (see [Filters and pruning](#filters-and-pruning)). Until the
extension moves to an API with filter pushdown (see [Roadmap](#roadmap)),
pass the ranges to `read_zarr` with `bounds :=` to read less. Once pushdown
ships, the same `WHERE` clause will skip the same chunks, and `bounds :=` will
not be needed.

```sql
SELECT time, lat, lon, value
FROM read_zarr('test/fixtures/xarray_tutorial/bounds_grid.zarr',
  bounds := [['time', '2020-01-01 02:00:00', '2020-01-01 02:00:00'],
             ['lat', '-30', '30']]);
```

Each entry is `[dimension, low, high]`. Both ends are inclusive and written as
strings, because a SQL list cannot mix a dimension name with numbers. `NULL`
leaves an end open. Times are written `YYYY-MM-DD`, optionally followed by
`HH:MM`, seconds and fractional seconds. The scan skips every chunk that holds
no coordinate in range, then drops the rows outside the range from the chunks
it does read, so the result is exact.

Some ranges are refused rather than answered approximately. A dimension needs
a stored numeric or time coordinate: positional `0..N-1` columns and string
labels (such as AnnData cell names) cannot be bounded, so filter those with
`WHERE`. Ranges compare values as 64-bit floats, so for integer and time
coordinates a range is rejected if an endpoint or any coordinate value has a
magnitude of 2^53 or more, which a float cannot represent exactly. DuckDB
counts timestamps in microseconds since 1970, so this rules out time axes that
reach before about 1685 or after about 2255.

With `decode_times := false`, time ranges use the raw stored numbers. Tables
made only of sparse matrices are filtered row by row but not pruned by chunk.

## How a query runs

DuckDB runs a table function in three phases: *bind*, once per query, decides
the output columns; *init* prepares the scan; the *scan* function is then
called repeatedly, each call filling one batch of rows, until it returns an
empty batch.

### Bind: from a path to a schema

1. **Open the store.** Pick a storage backend from the path (see
   [Where stores can live](#where-stores-can-live)).
2. **List the arrays.** For a local store, walk the directory tree, looking
   for `zarr.json` (v3) or `.zarray` (v2). For a remote store, read the list
   from consolidated metadata, since object stores cannot list directories
   cheaply.
3. **Build the tree of tables.** For each group, read each array's dimension
   names from `dimension_names` (v3) or `_ARRAY_DIMENSIONS` (v2), then from
   OME-Zarr or AnnData layouts where those apply. Set aside bounds arrays,
   auxiliary coordinates, 0-dimensional arrays and arrays without names.
   Identify coordinates: a 1-D array whose only dimension has its own name.
   Group the remaining arrays by dimension names and shape, resolve each
   dimension's coordinate from the group or its nearest ancestor, and check
   alignment with the parent group.
4. **Choose one table** from `table_name`, `dims`, `array_path`, or the only
   table in the group.
5. **Load the coordinates.** Each coordinate array is read whole into memory.
   Coordinates are 1-D and small next to the data (ERA5's latitude, longitude
   and time axes together fit in a few megabytes).
6. **Decide each column's type and decoding.** Each column gets a DuckDB type
   and a decoding rule: plain, packed integer, CF time, or AnnData categorical,
   plus a missing-value sentinel (see [Reading values
   correctly](#reading-values-correctly)).

### Plan: work units

The scan is divided into **work units**, one per cell of the table's chunk
grid. If the table's chunks are `24 × 10 × 10` over `(time, lat, lon)`, there
is one work unit for every block of 24 time steps, 10 latitudes and 10
longitudes, and the work units are listed in row-major order (the last
dimension varies fastest). With `bounds :=`, work units with no coordinate in
range are dropped here, before any data is read.

### Scan: from a chunk to rows

Each call to the scan function takes the current work unit, decodes the chunk
of each data variable the query uses, and writes rows until the batch is full
or the chunk runs out. Then it moves to the next work unit.

Writing rows from a decoded chunk is the core of the extension. A chunk of
shape `(s₀, s₁, …, s_{D-1})` holds `s₀ × s₁ × … × s_{D-1}` cells, stored in
row-major order. The scan walks them with a flat row counter `i` and recovers
each cell's position along every dimension with the row-major strides:

```
stride[k] = s[k+1] × s[k+2] × … × s[D-1]     (stride[D-1] = 1)
index[k]  = (i / stride[k]) mod s[k]         (integer division)
```

For a chunk of shape `(2, 3)`, rows 0 to 5 have positions `(0,0) (0,1) (0,2)
(1,0) (1,1) (1,2)`. Each data column reads the cell at those positions from
its decoded chunk. Each coordinate column looks up its coordinate array at the
chunk's starting offset plus `index[k]`. The coordinate columns are never
stored repeated; they are produced from the 1-D arrays as each batch is
written. The batch size is read from DuckDB at run time rather than assumed to
be 2,048.

The chunks at the far edge of an array can be cut short by the array's end.
The Zarr library still returns the chunk at its full size, padded with fill
values. The scan then uses two sets of strides: those of the clipped shape,
inside the array, to enumerate rows, and those of the full chunk shape to find
each cell in the buffer.

Data variables the query does not mention are not decoded. `SELECT time, lat,
lon, temperature` on a table that also has `humidity` never reads a
`humidity` chunk. Coordinates are loaded at bind regardless.

### Mixed chunk shapes

Variables in one table do not always share a chunk shape. In the
`air_temperature_gradient` dataset from the xarray tutorial, `Tair` is chunked
`(730, 13, 27)` and `dTdx` `(730, 7, 27)`, because they came from NetCDF
variables stored differently. AnnData tables hit the same problem, since
zarr-python picks each array's chunks from its data type: in a store of
500,000 cells, an `int8` column might be chunked `(250000,)` and a `float64`
column `(62500,)`.

The work-unit grid is therefore the largest chunk length in each dimension
over the table's dense arrays; `read_zarr_tables` shows it as `chunk_shape`. An
array chunked like the grid reads one chunk per work unit. Any other array
reads the work unit's region as a subset, which decodes each of its chunks the
region touches. When the smaller chunks fit evenly inside the grid, as
power-of-two chunk sizes do, each is decoded once. When they do not (7 against
13), a chunk that straddles two work units is decoded twice.

### Sparse tables

A table made only of sparse matrices has a different plan: it produces one
row per stored entry, by walking the matrix's compressed row (CSR) or column
(CSC) structure, rather than one row per cell. When a table has several sparse
matrices, the rows are the union of their stored positions, with `0` for a
matrix that has nothing at a position another one stores. A sparse matrix in a
table that also has a dense array is read densely, one row per cell, because
the dense array needs every row anyway.

### Filters and pruning

Pruning means skipping chunks that cannot contribute to the result. It
matters: in one measurement, a one-day slice of the ARCO-ERA5 store needed 3
of its 69,033 chunks.

Today the scan does not see a query's `WHERE` conditions. DuckDB applies them
to the rows the scan returns, so results are correct, but the scan reads every
chunk of the table. The `bounds :=` parameter is the explicit substitute.

The reason is the interface the extension is built on. DuckDB's own readers,
such as its Parquet reader, receive `WHERE` conditions through DuckDB's C++
interface. Extensions written in Rust use DuckDB's C API instead, and the
extension currently uses version 1.5.6 of it, which has no such hook, so that
one binary loads in both DuckDB 1.5.6 and 2.0. DuckDB 2.0 adds a second C API,
the v2 API, which does have one: during optimization, the scan is offered each
`WHERE` condition and can *accept* the ones it will apply itself; DuckDB keeps
applying the rest. Moving to it is the planned route to pushdown (see
[Roadmap](#roadmap)). The range logic behind `bounds :=` is kept separate from
the parameter so that pushed conditions can reuse it.

Projection pushdown, telling the scan which columns a query uses, works in
both C APIs, and the extension uses it as described above.

### Threads

The extension asks DuckDB for as many threads as there are work units, and
threads take work units from a shared counter. In the current version, all
threads also share one lock that is held while a chunk is decoded, so in
practice chunks are decoded one at a time. See [Known limits](#known-limits).

## Reading values correctly

### Data types

| Zarr data type | DuckDB type |
|---|---|
| `bool` | `BOOLEAN` |
| `int8`, `int16`, `int32`, `int64` | `TINYINT`, `SMALLINT`, `INTEGER`, `BIGINT` |
| `uint8`, `uint16`, `uint32`, `uint64` | `UTINYINT`, `USMALLINT`, `UINTEGER`, `UBIGINT` |
| `float32`, `float64` | `FLOAT`, `DOUBLE` |
| `string` (v3), or `\|O` with the `vlen-utf8` filter (v2) | `VARCHAR` |
| any integer type with CF packing attributes | `DOUBLE` |
| any numeric type with CF time attributes | `TIMESTAMP` |
| AnnData categorical of distinct strings | `ENUM` |

Other types are not supported yet, including NumPy `datetime64`, fixed-width
byte and Unicode strings, complex numbers and structured types. If the Zarr
library can open the array, `read_zarr` fails at bind with `unsupported
dtype`; if it cannot, the array is listed as `unsupported` (see [Inspecting a
store](#inspecting-a-store)).

Arrays can be little- or big-endian on disk; the Zarr library converts them
to the machine's byte order when it decodes a chunk. The codecs available are
those of the [zarrs](https://zarrs.dev/) library this extension builds on:
gzip, zstd, Blosc (on all platforms except Windows with the GNU toolchain),
CRC32C checksums, sharding and transpose.

### Missing values

A cell becomes SQL `NULL` when its stored value equals the variable's
missing-value sentinel. The sentinel is chosen once per variable:

1. The `_FillValue` attribute, if present and readable as the array's type. If it is `NaN` and a
   `missing_value` attribute also exists, `missing_value` is used instead,
   because xarray writes `_FillValue = NaN` for data whose real sentinel it
   has already replaced in memory.
2. Otherwise the `missing_value` attribute.
3. Otherwise the array's Zarr `fill_value`, unless all its bytes are zero (so
   `0` and `0.0` are not sentinels). Boolean and string fill values are never
   used.

A `NaN` sentinel matches any `NaN`, since `NaN` never equals itself. Without a
`NaN` sentinel, a `NaN` in a float column stays `NaN`, not `NULL`. String
columns are never masked, and AnnData's encoded columns use their own masks
instead.

xarray writes a floating-point `_FillValue` in Zarr v3 as a base64 string of
the value's 8 little-endian bytes (`NaN` is `"AAAAAAAA+H8="`). The extension
decodes these.

Rule 3 means that, in an array without CF attributes, a real value equal to a
nonzero `fill_value` also reads as `NULL`. See [Known limits](#known-limits).

### Packed integers

A variable is packed when its stored type is an integer and it has a
`scale_factor` or `add_offset` attribute (a missing one defaults to 1 or 0).
Its column is `DOUBLE`, and each cell is computed as `stored × scale_factor +
add_offset`. The comparison against the missing-value sentinel happens before
scaling, as CF requires: the sentinel is a stored integer, and scaling it
first would change it.

The integer requirement matters. Some float arrays carry a `scale_factor`
attribute as leftover metadata, and scaling values that are already real
numbers would corrupt them.

### CF time

A coordinate or variable whose `units` attribute reads `"<unit> since
<reference date>"` is a CF time axis. The extension converts it to
`TIMESTAMP`, so time conditions read naturally:

```sql
SELECT date_trunc('month', time) AS month, AVG(air) AS mean_air
FROM 'test/fixtures/xarray_tutorial/air_temperature.zarr'
WHERE time >= TIMESTAMP '2014-01-01'
GROUP BY month ORDER BY month;
```

Without the conversion, ERA5's `time` would read as a count of hours since
1900, and every query would need to look up that attribute to write
`TIMESTAMP '1900-01-01' + INTERVAL (time) HOUR`.

Only calendars that match the real-world calendar are converted:
`proleptic_gregorian`, and `standard` or `gregorian` (the default when no
calendar is given) from 15 October 1582, when the Gregorian calendar began.
Dates before then in a `standard` axis follow the older Julian calendar in the
file's own terms. If the reference date is that early, the axis keeps its
stored numbers; if only some values are, those values read as `NULL` rather
than as dates about ten days off.

Model calendars such as `noleap`, `all_leap`, `360_day` and `julian` have
dates that a real calendar lacks, such as 30 February, so those axes keep
their stored numbers. Their `units` and `calendar` attributes are visible
in `read_zarr_metadata`.

Some details of the conversion:

- Units can be `weeks`, `days`, `hours`, `minutes`, `seconds`,
  `milliseconds`, `microseconds` or `nanoseconds`, with the usual
  abbreviations. `months` and `years` are refused, as xarray refuses them,
  because they have no fixed length. Bare `m` means metres in the UDUNITS
  library that defines these names, so it is refused too.
- Reference dates can be written `1900-01-01`, `1800-01-01 00:00:00`,
  `1970-01-01T00:00:00Z` or `0001-1-1 0:0:0`, with an optional time-zone
  offset. The offset's sign is optional, because UDUNITS writes a zero offset
  as `0:00`.
- Integer offsets are converted with integer arithmetic, and floating-point
  offsets are split into whole and fractional parts before scaling. The
  fractional part matters at scale: `days since 1800-01-01` reaches about
  7 × 10^15 microseconds, beyond the range where a single float multiply is
  exact to the microsecond.
- A missing-value sentinel is checked against the stored number before
  conversion. `NaN`, and values that would fall outside DuckDB's timestamp
  range, become `NULL`.
- `decode_times := false` turns the conversion off for the whole query.

When an integer variable has both packing attributes and time units, it is
decoded as packed; no store in practice packs its time axis. A float variable
with both is decoded as time, and its `scale_factor` is ignored.

### Strings

Variable-length UTF-8 strings, as AnnData and zarr-python write for text
columns such as gene symbols, read as `VARCHAR`. zarrs reads both the v3
`string` type and the v2 `vlen-utf8` encoding.

## Where stores can live

| Path | How it is read |
|---|---|
| Local path | Directly from the file system |
| `http://`, `https://` | With the zarrs HTTP client |
| `s3://`, `gs://`, `az://` | Through DuckDB's own file system, so credentials come from DuckDB's [secrets manager](https://duckdb.org/docs/stable/configuration/secrets_manager) (`CREATE SECRET ... TYPE S3`) |

Routing cloud storage through DuckDB means the extension inherits DuckDB's
credentials, retries and configuration instead of reimplementing them. The
DuckDB file system is obtained from the query's client context during bind.

A remote store needs consolidated metadata to be listed. Most published
datasets have it; xarray writes it with `to_zarr(..., consolidated=True)`. The
extension reads it once per query and answers every later metadata lookup from
memory, so opening a store with a hundred arrays costs a few requests rather
than hundreds. Without consolidated metadata, a remote store can still
be read one array at a time with `array_path :=`. This is the usual case for
public OME-Zarr images.

The extension also builds for WebAssembly and loads in
[DuckDB-Wasm](https://duckdb.org/docs/stable/clients/wasm/overview), which runs
DuckDB in a web browser. There, every path, HTTP included, goes through
DuckDB's file system, and the Zarr library's internal thread pool runs on a
single thread. Local paths do not work in the browser, because listing a
local store walks the host's file system.

## Design decisions

The decisions are numbered so that code comments and tests can refer to them.

### 1. Follow declared dimension names, never guess them

Dimension names come only from metadata: xarray's attribute or field,
OME-Zarr's axes, or AnnData's layout. The alternative was to infer roles from
shape, so that 1-D arrays are coordinates and the rest are data, as the
zarr-datafusion project does. Declared names were chosen because xarray writes
most of the stores in this ecosystem and its metadata already gives the right
dimension names and order. Shape-based inference was dropped altogether
because of the failure described in decision 7.

### 2. One table per set of dimensions

The alternatives were one wide table for the whole store, which only works
when every variable has the same dimensions, and one table per variable,
which makes users join variables they usually want side by side. Grouping by
dimensions gives tables with no padding `NULL`s and keeps related variables in
one row. It matches how xarray-sql splits and names tables, so a store gets
the same tables in both projects.

### 3. Decode CF time, but only on real-world calendars

Converting time to `TIMESTAMP` is opinionated, and the obvious library for it,
the Rust port of cftime, is AGPL-licensed, which rules it out for an extension
distributed as a binary. What made it possible was splitting the problem. The
calendars that match a wall clock need only ordinary date arithmetic, which
the `chrono` library (already a dependency of DuckDB's Rust crate) provides,
and those calendars cover nearly all widely queried data, including ERA5, GPCP
and the NCEP reanalyses. The model calendars stay as raw numbers rather than
becoming plausible-looking wrong dates. `decode_times := false` restores the
numbers everywhere.

### 4. Claim only `.zarr` paths in `FROM`

DuckDB offers every string in a `FROM` clause to each registered replacement
scan. Checking the file system for every string would slow down unrelated
queries, so the extension considers only strings ending in `.zarr`, and for
local paths confirms with one file check. Other Zarr paths remain reachable
through `read_zarr(path)`.

### 5. Read synchronously

zarrs has an asynchronous interface, but DuckDB's table-function callbacks are
synchronous, and bridging the two would mean running an async runtime inside
each scan. Reads are synchronous. This can be revisited if network latency
comes to dominate remote queries.

### 6. Mixed chunk shapes share a work-unit grid

Three options were considered: refuse tables whose variables are chunked
differently, plan on the largest chunks and read the other arrays as subsets,
or iterate row by row. Refusing was the first implementation, and it turned
out to block common data, including NCEP reanalysis stores and the `obs`
table of any large AnnData store. Row-by-row iteration would give up chunked
reads. Planning on the largest chunks keeps the scan's structure and lets
zarrs do the subset reads, at the cost of decoding some chunks twice when
chunk boundaries do not line up (see [Mixed chunk
shapes](#mixed-chunk-shapes)).

### 7. Give placeholder names only to a single selected array

AnnData and plain zarr-python write no dimension names. Naming such axes
`dim_0`, `dim_1`, ... everywhere would let two unrelated arrays of the same
length share `dim_0`, land in one table and be joined row by row, returning
wrong data with no error. So placeholder names are given only when
`array_path :=` selects one array, where nothing can be misaligned, and a
placeholder name never matches a coordinate array, even one called `dim_0`.
Group reads leave unnamed arrays out. Arrays that name only some of their axes
do not follow this decision yet (see [Known limits](#known-limits)). `read_zarr_metadata` shows both the
declared names (`dims`) and the names an `array_path` read would use
(`array_path_dims`), so neither column changes meaning between calls.

### 8. Each group is a node, and a store maps to a database

Earlier versions pooled the arrays of every group and grouped them by
dimension names alone. That failed with a shape mismatch when two groups
reused a dimension name with different lengths, and silently merged them when
the lengths agreed. Reading one group at a time, with xarray's rules for
inheriting coordinates and checking alignment, is the `DataTree` model, and
following it lets xarray-sql and this extension agree on any store. Store maps
to database, group to schema and table to table, the same catalog shape that
DataFusion, the engine under xarray-sql, uses. `read_zarr_tables` publishes
that mapping, and it is the whole contract between the two projects.

The natural way to mount a store would be `ATTACH 'era5.zarr' AS era5 (TYPE
ZARR)`. The C API has no hook for adding a new kind of database to `ATTACH`,
and no way for an extension to run SQL on the caller's connection. A procedure
that created the views through a second connection was prototyped and
rejected: the extra connection kept DuckDB from ever releasing the database,
which left database files locked after closing, for every user of the
extension. `read_zarr_views` returns the statements instead, which costs the
user one extra step and avoids the problem. DuckDB 2.0's v2 C API adds
neither hook, and `read_zarr_views` is the supported way to mount a store;
`ATTACH ... (TYPE ZARR)` is not planned.

### 9. AnnData: named axes, decoded encodings, stored entries

Read as plain Zarr, an AnnData store gives no tables at all (it has no
dimension names), integer codes instead of category labels, empty strings
instead of `NULL` for nullable columns, and three unaligned arrays instead of
each sparse matrix. Users can decode all of that in SQL, but the queries are
long, and two easy mistakes return wrong data without an error. So the
extension applies an AnnData layer inside every AnnData group. The main
choices:

- `obs` and `var` sit beside `X`. As groups of their own they would give
  tables named `obs.obs` and `var.var`, and a root with no `obs` coordinate
  for `X` to inherit. Beside `X` they are what an xarray `Dataset` of an
  AnnData object would hold: `X(obs, var)`, `cell_type(obs)`,
  `gene_symbol(var)`.
- Names, not positions, label the axes. With each frame's index as the
  coordinate, every table can be filtered and joined by cell or gene name
  without a lookup join. Joins then compare strings, and a store with
  duplicate names gives extra rows; AnnData warns about duplicates and
  `obs_names_make_unique()` is a standard step, so the guide documents the
  risk rather than switching to positions.
- Embeddings stay long. The second axis of an `obsm` entry (the components
  of a PCA or UMAP) has a different length in each entry, so each gets its own
  dimension, `<key>_component`. DuckDB array columns were considered and
  rejected, to keep every value scalar.
- Sparse matrices read as stored entries. Expanding them to every cell
  would turn a modest dataset into tens of millions of rows of zeros, and real
  atlases are a thousand times larger. One row per stored entry is how
  single-cell users already treat `X` in SQL. The cost is that averages over
  cells must add the missing zeros back, for example by starting from the
  `obs` table with a `LEFT JOIN` and averaging `COALESCE(X, 0)`. Two sparse
  matrices with the same dimensions must share a table (decision 2), so the
  table's rows are the union of their entries.
- Any AnnData group, not just the root. SpatialData keeps its tables as
  AnnData objects beside OME-Zarr images and Parquet files. Reading those
  tables needs nothing SpatialData-specific, and DuckDB's `read_parquet`
  already reads the rest, so the extension stays a Zarr reader.

Not covered yet: `awkward-array` elements, data frames inside `uns`, and the
global `obs` and `var` of a MuData store, which lie outside its AnnData
modalities.

### 10. Explicit coordinate ranges until `WHERE` can be pushed down

Pushing `WHERE` conditions into the scan is the goal: it is what users of any
database expect, and both xarray-sql and zarr-datafusion do it. The C API the
extension uses has no hook for it, so `bounds :=` was added as an explicit
stopgap. Its range logic resolves each bound against the coordinate values
themselves, so descending axes (ERA5's latitude runs from 90 to -90) and
unevenly spaced ones (pressure levels) need no special cases. Ranges are
inclusive and checked row by row inside kept chunks. Integer and time values
too large for a float to hold exactly are rejected rather than compared
approximately.

The plan for real pushdown, on DuckDB 2.0's v2 C API, reuses that logic: each
accepted condition on a coordinate becomes a set of kept indices, which
selects chunks and filters rows exactly as `bounds :=` does. The scan accepts
only conditions it can apply exactly (comparisons with a constant, and `IN`
lists), and uses any others only to skip chunks, leaving DuckDB to apply them.
That second group includes `BETWEEN` and other two-sided ranges, which the v2
API passes without saying whether their ends are inclusive
([duckdb/duckdb#25391](https://github.com/duckdb/duckdb/issues/25391), open
as of November 2026).
With pushdown in place, a `WHERE` clause does everything `bounds :=` does, so
the parameter is no longer needed. It stays for at least one release after
pushdown ships, so that existing queries keep working, and is then
deprecated.

### 11. Keep the Zarr reader independent of DuckDB

Reading Zarr (store layout, DataTree and AnnData rules, decoding, CF time,
range pruning) lives in a separate crate, `zarr-core`, whose public types
contain no DuckDB or Arrow types. The DuckDB extension is an adapter on top.
This keeps the conventions testable on their own and lets other engines reuse
them.

## Known limits

The [Roadmap](#roadmap) covers the planned fixes for most of these.

- No pushdown of `WHERE`. Every query reads every chunk of its table
  unless it uses `bounds :=` (see [Filters and pruning](#filters-and-pruning)).
- No statistics. The extension reports no row count, so DuckDB's optimizer
  plans joins on its default guess, and no column statistics.
- Chunks are decoded one at a time. A lock shared by all scan threads is
  held during decoding.
- A nonzero Zarr `fill_value` reads as `NULL` in arrays without CF
  missing-value attributes, even where it is a real value.
- `NaN` is not `NULL` unless the variable's sentinel is `NaN`.
- Partly named arrays get placeholder names in table reads. A `null` entry in
  `dimension_names` becomes `dim_<position>`, so such arrays can share a
  table, contrary to decision 7.
- Unsupported data types: NumPy `datetime64`, fixed-width strings, complex
  and structured types.
- Auxiliary coordinates are dropped. A 2-D `lat(y, x)` named in a
  `coordinates` attribute does not become a column.
- Model calendars stay numeric. `noleap`, `360_day`, `julian` and similar
  time axes are not converted.
- Coordinates are reloaded for every query, as is consolidated metadata.
- Remote stores need consolidated metadata, except for single-array reads
  with `array_path :=`.
- Local paths do not work in DuckDB-Wasm.
- A few arrays can drop out of a table without an error: an array with an
  unsupported data type or codec whose metadata gives no readable shape, and
  an AnnData encoded column whose values array zarrs cannot open. `read_zarr_metadata` still lists them.
- `group_path :=` on a group with no arrays in or below it reports the group
  as not found.
- Read-only. The extension does not write Zarr.

## Roadmap

The aim is to work like any SQL table and to reach rough parity with
[xarray-sql](https://github.com/alxmrs/xarray-sql) and
[zarr-datafusion](https://github.com/jayendra13/zarr-datafusion). The items
below are planned but not built. Each says which DuckDB C API it needs: the
v1.5.6 API the extension uses today, or DuckDB 2.0's v2 API.

The v2 API changes the plan, so its terms come first. In DuckDB 2.0 every v2
function is marked unstable, so it may change between releases, and an
extension built on it is tied to the DuckDB version it was built for. It also
does not load in DuckDB 1.5. The
`duckdb` Rust crate for 2.0 (version `1.20000.0`) includes bindings to it for
loadable extensions, but its safe wrapper for table functions still covers
only the v1 API, so the port means calling the v2 functions directly.

### Expected of any database

- Filter pushdown on coordinates (v2 API). Comparisons, `IN` and ranges on
  dimension columns select chunks and rows, as decision 10 describes. Pushed
  conditions could later select index ranges inside a chunk too, as
  zarr-datafusion does, so that a narrow slice reads less than whole chunks.
- Row counts for the optimizer (v1 and v2 APIs). DuckDB uses a table's row
  count to plan joins and choose algorithms. A dense table's count is the
  product of its shape, known exactly at bind; a sparse table's is the number
  of stored entries, known after reading the matrices' index arrays. Both
  sibling projects report exact counts. The extension reports none today.
- Column statistics (not available in either C API). xarray-sql and
  zarr-datafusion report exact minimum and maximum values for coordinates,
  and xarray-sql reports null counts. The extension knows coordinate minima
  and maxima exactly from the coordinate arrays, but neither DuckDB C API lets
  a table function report per-column statistics. This needs a change in
  DuckDB.
- Parallel scans (v1 and v2 APIs). Both APIs support per-thread scan state,
  which removes the shared lock described in [Threads](#threads). On the v2
  API the scan can also declare that its rows have no particular order, which
  lets DuckDB run its output in parallel.
- Grouped aggregates by coordinate (v2 API). The v2 API lets a scan tell
  DuckDB that its rows are already partitioned by some columns, so `GROUP BY`
  on a coordinate could skip hashing. zarr-datafusion goes further and answers
  `COUNT`, `MIN` and `MAX` from metadata.

### Parity with xarray-sql and zarr-datafusion

- `NaN` as `NULL`. xarray-sql returns floating-point `NaN` as `NULL`, which
  matches how xarray users treat missing data. The extension keeps `NaN`
  unless it is the variable's sentinel.
- More data types: NumPy `datetime64` and `timedelta64`, and fixed-width byte
  and Unicode strings. Both siblings read `datetime64`.
- Dictionary-encoded coordinate columns (v1 API). Coordinate values repeat
  heavily, and zarr-datafusion encodes them as dictionaries. The v1 API can
  build dictionary vectors by slicing a vector with a selection vector.
- Writing Zarr with `COPY ... TO 'out.zarr'` (v1 API copy functions), as
  zarr-datafusion supports.
- A `chunks :=` parameter to set the work-unit size, as xarray-sql's
  `chunks=` sets its partitions.
- Packed `float32` output. xarray decodes integers packed with `float32`
  scale factors to `float32`; the extension always uses `DOUBLE`.
- Model calendars (`noleap`, `360_day`, ...). xarray-sql keeps them as
  numbers and offers a `cftime()` function for them.
- Icechunk and virtual Zarr, which zarr-datafusion reads. Icechunk adds
  Git-like versioning to Zarr; its Rust library is asynchronous only and opens
  repositories and sessions rather than paths, so it needs an asynchronous
  storage layer and a way to name snapshots in SQL. Virtual Zarr (Kerchunk and
  VirtualiZarr manifests) presents existing NetCDF, HDF5 or GRIB2 files as
  Zarr; their chunks live in other files at offsets given by a manifest, and
  are often compressed in ways that are not Zarr codecs.

### Smaller improvements

- Cache coordinates and consolidated metadata across queries, keyed by store,
  instead of reloading them in every bind. A configurable chunk cache (a
  `SET` option, available in both APIs) is a possible follow-up.
- Auxiliary coordinates as columns. Neither sibling exposes them (xarray-sql
  drops them, zarr-datafusion treats them as data), but xarray's own
  `to_dataframe()` does.

### Blocked on DuckDB

- Column statistics, above.

### Not planned

- `ATTACH 'store.zarr' (TYPE ZARR)`. Neither C API lets an extension add a
  kind of database to `ATTACH`, and `read_zarr_views` already mounts a store
  as views (decision 8).
- Substrait, a format for passing query plans between engines. A `read_zarr`
  call is specific to DuckDB, so plans that use it cannot run elsewhere;
  mounted views, which other engines could resolve by name, are a closer fit.
- `variables :=` and `coords :=` parameters, from an early draft of this
  design. Selecting columns in `SELECT` already limits what is decoded, and
  neither sibling has an override for coordinate roles.

## Related projects

- [xarray-sql](https://github.com/alxmrs/xarray-sql) queries xarray datasets
  with SQL through Apache DataFusion. This extension follows its table layout
  and naming.
- [zarr-datafusion](https://github.com/jayendra13/zarr-datafusion) reads Zarr
  into DataFusion from Rust.
- [zarrs](https://zarrs.dev/) is the Rust Zarr library this extension uses
  for metadata, codecs and chunk decoding.

## Where things live in the code

| Concept | Location |
|---|---|
| Extension entry point, function registration | `src/lib.rs` |
| `read_zarr`: bind, work units, scan, row writing | `src/read_zarr.rs` |
| `read_zarr_metadata`, `read_zarr_tables`, `read_zarr_views` | `src/read_zarr_metadata.rs`, `src/read_zarr_tables.rs`, `src/read_zarr_views.rs` |
| `group_path`, `array_path` parameter handling | `src/selection.rs` |
| `FROM 'store.zarr'` | `src/replacement_scan.rs` |
| Opening stores; DuckDB file system adapter | `src/zarr_reader/meta.rs`, `src/zarr_reader/duckdb_store.rs` |
| Writing decoded values into DuckDB vectors | `src/zarr_reader/scan.rs`, `src/zarr_reader/types.rs` |
| Groups, tables, coordinate inheritance, alignment, table names | `crates/zarr-core/src/tree.rs` |
| Dimension names, CF roles, sentinels, packing | `crates/zarr-core/src/meta.rs` |
| AnnData layout and encodings | `crates/zarr-core/src/anndata.rs`, `crates/zarr-core/src/sparse.rs` |
| CF time | `crates/zarr-core/src/cftime.rs` |
| `bounds :=` range pruning | `crates/zarr-core/src/bounds.rs` |
| Consolidated metadata cache | `crates/zarr-core/src/consolidated_store.rs` |
| SQL tests and fixtures | `test/sql/`, `scripts/generate_fixtures.py` |

A few terms in the code differ from this document. A *node* is a group. A
`Variable` is one column's source, which is one array or, for AnnData encoded
columns, a group of arrays; its `Storage` says which. A `ScanPlan` is either
one row per cell or one row per stored sparse entry. A `ColumnEncoding` is a
decoding rule: plain, packed, CF time or categorical.
