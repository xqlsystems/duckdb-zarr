# Design: native Zarr in DuckDB

A design for `duckdb-zarr` — a Rust DuckDB extension that lets users query Zarr stores with SQL, in the same spirit as [xarray-sql](https://github.com/alxmrs/xarray-sql) (Python/DataFusion) and [zarr-datafusion](https://lib.rs/crates/zarr-datafusion) (Rust/DataFusion), but as a first-class DuckDB extension with no external query engine in the loop.

## Goals

- `SELECT * FROM 'path/to/store.zarr'` works for any Zarr v2 or v3 store that follows xarray-style conventions.
- Read remote stores (S3, GCS, HTTP) via DuckDB's existing `httpfs`/`secrets` machinery.
- Push projection and coordinate-range filters down so that only the chunks we need are touched.
- Use DuckDB's native parallel scan (one Zarr chunk ≈ one morsel of work).
- Ship as a community extension installable with `INSTALL zarr FROM community`.

## Non-goals (for the first cut)

- Writes. Read-only.
- Automatic relational joins across nested groups. Arrays are discovered recursively and can be selected by store-relative path, but each `read_zarr` scan still operates on one compatible dimension group or one explicitly selected array.
- Replacing xarray. Users who need lazy array operations should keep using xarray; we just want a SQL handle on the same data.
- Custom codecs beyond what `zarrs` already supports.
- WebAssembly. The current scaffold ships a `wasm_lib.rs` target; it is not a supported build because `zarrs` and `ndarray` use threading and I/O patterns that don't trivially compile to `wasm32`. 

## Future integration boundaries (deferred)

Three integration targets come up whenever a Zarr reader ships in the Pangeo orbit: IceChunk, VirtualiZarr/Kerchunk, and Substrait. None are in scope for v0.1–v0.4. This section names each so future contributors aren't surprised by the gap, and is deliberate about *not* designing the seam upfront for them. Three integrations with three very different shapes can't share one general abstraction without producing one that's wrong for all three.

**IceChunk** (Git-like versioning over a Zarr-compatible repository). The only viable Rust path is `zarrs_icechunk`, which is async-only and exposes a Repository + Session model rather than the open-a-path model of plain Zarr. Adding IceChunk would require (a) an async-aware boundary at the `zarr_reader` seam, breaking decision 5; (b) snapshot / branch identifiers on the SQL surface — named arguments on `read_zarr` and `ATTACH` would absorb this without breaking the existing API; (c) a probe step in the replacement scan to distinguish IceChunk repos from plain Zarr roots. None of this is started.

**VirtualiZarr / Kerchunk** (Zarr-shaped views over archival NetCDF/HDF5/GRIB2). The chunk manifest lives at the Zarr path but the actual chunk bytes live in a foreign URL. The current storage adapter is "key → DuckDB filesystem at `store_root + '/' + key`" — wrong shape for virtual chunks, which need "key → manifest lookup → byte-range fetch from a different URL." Compounding this, virtual chunks usually point into HDF5/NetCDF/GRIB2 files whose internal compression is *not* a Zarr codec, so the non-goal "no custom codecs beyond `zarrs`" excludes most archival VirtualiZarr stores by construction. The only Rust ecosystem path today is IceChunk-backed VirtualiZarr (Kerchunk JSON/Parquet has no Rust parser), so VirtualiZarr is doubly gated on IceChunk landing first.

**Substrait** (cross-engine query plan exchange). DuckDB's separate `substrait` community extension serializes plans, but a `read_zarr(...)` call serializes as a DuckDB-specific extension function that no other engine can execute. The `ATTACH ... (TYPE ZARR)` path is closer to Substrait-friendly because it presents the Zarr store as a named table — Substrait's `ReadRel` references a table by name, which a hypothetical DataFusion-side reader could resolve through a shared catalog. We're not designing for Substrait, but the existence of the `ATTACH` form means we're not painting ourselves into a corner either.

The `zarr_reader` seam (§Architecture) is correct for plain Zarr v2/v3 today. None of these three integrations fit through it without modification. That's by design — getting plain Zarr right is the v1 product, and a seam pre-shaped for three speculative integrations would be wrong-shaped for all three. The seam stays small; future work re-evaluates it then, with concrete user evidence.

## Implementability risks (spike resolved 2026-05-04)

The pinned `duckdb` crate (`=1.10504.0`) exposes the `VTab` trait (bind/init/func) and scalar-function registration that v0.1 requires. Below are the resolved findings for the four APIs that were open questions:

**(a) Replacement-scan registration** — `Connection::register_replacement_scan` does **not** exist in duckdb-rs. However, `libduckdb-sys` (the underlying C FFI layer that ships with the crate) exposes `duckdb_add_replacement_scan`, `duckdb_replacement_scan_set_function_name`, `duckdb_replacement_scan_add_parameter`, and `duckdb_replacement_scan_set_error`. Replacement scan for `.zarr` path interception lands behind a thin `unsafe` FFI wrapper calling these symbols directly. This unblocks the v0.2 work.

**(b) ATTACH / storage-extension hooks** — `duckdb_register_storage_extension` is **absent** from `libduckdb-sys`. ATTACH as a proper storage engine is not achievable via the C extension API at this version. The v0.3 ATTACH milestone must use a different mechanism — most likely a macro-style shim (a SQL `ATTACH` wrapper that mounts each dimension group as a named view). This weakens the ATTACH UX slightly (no native `FROM zarr.temperature`) but keeps the rest of the design intact.

**(c) Dictionary-vector construction** — `duckdb_create_dictionary_vector` is **absent**. Coordinate columns are emitted as flat `FlatVector` values gathered from the cached coord array; the dictionary-encoding optimization is off the table. Correctness is unchanged; memory use per scan is higher for high-cardinality coord columns (rare in practice — time and lat/lon coords repeat heavily within a chunk but the duplication is at the chunk-buffer level, not the SQL vector level).

**(d) Extension config-variable registration** — `duckdb_register_config_option`, `duckdb_create_config_option`, `duckdb_config_option_set_{name,description,type,default_value,default_scope}`, and `duckdb_client_context_get_config_option` are **all present** in `libduckdb-sys`. `SET zarr_chunk_cache_mb = 512` lands behind a thin FFI wrapper; the setting is readable from `InitInfo` via the client context pointer.

**(e) Predicate filter pushdown** — `duckdb_bind_get_filter` and `duckdb_table_function_set_pushdown_filter` are **absent**. DuckDB will evaluate `WHERE lat > 30` by scanning all rows and filtering post-scan. Filter pushdown remains a v0.3 item; if the C API exposes a path at that point it can be added, otherwise the optimizer handles it outside the scan.

**(f) Projection pushdown** — `duckdb_table_function_supports_projection_pushdown`, `duckdb_init_get_column_count`, and `duckdb_init_get_column_index` are **present** in `libduckdb-sys`. The high-level `VTab::supports_pushdown()` method (returns `false` by default) controls this on the duckdb-rs side. Both paths work; we use the high-level API for v0.1 (all columns, no pushdown), add projection pushdown in v0.2 when it reduces unnecessary coord materialization.

## The pivot, in DuckDB terms

A Zarr store with coordinates `lat(L)`, `lon(M)`, `time(T)` and data variables `temperature[T,L,M]`, `humidity[T,L,M]` is exposed as a single DuckDB table:

```
| time      | lat   | lon   | temperature | humidity |
|-----------|-------|-------|-------------|----------|
| 2024-01-01| 0.0   | 0.0   | 273.15      | 0.81     |
| 2024-01-01| 0.0   | 0.5   | 273.42      | 0.79     |
| ...       |       |       |             |          |
```

Logical row count is `T × L × M`. The nD → 2D mapping is the same `ravel()`/metadata reshape that xarray-sql relies on: we never materialize the cartesian product in memory; we generate it row-major on the fly inside each chunk-sized scan.

Coordinate columns within a single chunk are highly repetitive. `duckdb-rs 1.10504.0` does not expose dictionary-vector construction (confirmed spike — see §Implementability risks), so coord columns are emitted as flat `FlatVector` values gathered from the cached coord array. Correctness is identical to the dictionary-encoding approach zarr-datafusion uses via Arrow `DictionaryArray`; the cost is more bytes per scan for high-cardinality coord columns, which in practice are rare since time/lat/lon repeat heavily within each chunk.

### One table per dimension group

A Zarr store often holds variables with *different* dimension sets. ERA5 is the canonical example: surface variables (`t2m`, `sp`, `tcwv`) have dims `(time, lat, lon)`, while pressure-level variables (`t`, `u`, `v`, `q`) have dims `(time, level, lat, lon)`. A single wide table can't hold both without padding NULLs across millions of rows.

We split the store into **one table per distinct dimension set**. The ERA5 example surfaces as two tables:

- a surface table over `(time, lat, lon)` with one column per surface variable
- an atmosphere table over `(time, level, lat, lon)` with one column per pressure-level variable

Tables get a default name derived from the sorted dim names (`t_lat_lon`, `level_t_lat_lon`); users can override via the `ATTACH` syntax below. `read_zarr_metadata` enumerates them so users can discover groupings before issuing the scan.

## SQL surface

Four entry points, in priority order.

### 1. Replacement scan (the headline UX)

```sql
SELECT lat, lon, AVG(temperature)
FROM 'gs://my-bucket/era5.zarr'
GROUP BY lat, lon;
```

DuckDB's [replacement scan API](https://duckdb.org/docs/api/c/replacement_scans) claims paths via a two-step probe:

1. **Normalized suffix check** — strip a trailing slash, lowercase the suffix, accept `.zarr`. Handles macOS case-insensitive filesystems and the `s3://bucket/store.zarr/` form that cloud consoles love to produce. URL-decoding is DuckDB's job; we accept whatever string we're handed.
2. **Metadata stat** — single read of `zarr.json` (v3) or `.zgroup` (v2) at the path. Only if this succeeds do we claim the path; otherwise we let DuckDB's normal replacement chain take over.

Suffix-less Zarr groups are reachable via `read_zarr(path)` directly; auto-claim of suffix-less paths is deferred because it would force a stat on every string literal DuckDB hands us.

If the store contains exactly one dimension group, the replacement scan returns that table directly. If it contains multiple, the scan errors with a message listing the available groups and tells the user to `ATTACH` or call `read_zarr` with an explicit `dims :=`.

### 2. `read_zarr` table function

```sql
SELECT * FROM read_zarr('path/to/store.zarr');

-- Pick a subset of variables
SELECT * FROM read_zarr('store.zarr', variables := ['temperature']);

-- Pick a dimension group when the store has more than one
SELECT * FROM read_zarr('era5.zarr', dims := ['time', 'lat', 'lon']);

-- Override coordinate-to-variable mapping when conventions don't apply
SELECT * FROM read_zarr(
  'store.zarr',
  coords := ['t', 'y', 'x'],
  variables := ['u', 'v']
);

-- Pick one array, including a nested OME-Zarr level
SELECT * FROM read_zarr('image.ome.zarr', array_path := 'labels/nuclei/0');

```

Named arguments stay close to xarray's vocabulary (`variables`, `coords`, `chunks`, `dims`). Variables that share the requested dim set become one output column each, joined on coordinate index — exactly the xarray-sql `pivot()` shape. Variables outside that dim set are silently excluded; the user picks them up by querying a different `dims` group.

`array_path` adds an unambiguous selector for multiscale OME-Zarr levels and nested labels. The same selector is accepted by `read_zarr_metadata` and `read_zarr_groups`. A quoted `"array"` alias is also registered; the quotes are required because `ARRAY` is a DuckDB keyword.

### 3. `ATTACH` for multi-group stores

```sql
ATTACH 'era5.zarr' AS era5 (TYPE ZARR);

SELECT * FROM era5.surface          -- (time, lat, lon)
WHERE time >= '2024-01-01';

SELECT level, AVG(t)
FROM era5.atmosphere                -- (time, level, lat, lon)
GROUP BY level;
```

`ATTACH ... (TYPE ZARR)` mounts the store as a DuckDB schema with one view per dimension group. Group names default to a slugified join of the dim names; users can rename with `ALTER VIEW`. This is the recommended UX for ERA5-class stores where you'll be issuing many queries and want stable table names.

### 4. `read_zarr_metadata` and `read_zarr_groups`

Two introspection table functions, each with a homogeneous schema (split because mixing array rows and group rows in one table forces NULLs across most columns of the group rows):

```sql
SELECT * FROM read_zarr_metadata('store.zarr');
-- name | kind (coord|data) | dims | dtype | shape | chunks | compressor | attrs

SELECT * FROM read_zarr_groups('store.zarr');
-- group_name | dims | n_variables | variables | n_rows
```

Both are read-no-chunks. `read_zarr_metadata` enumerates arrays for inspection and tooling; `read_zarr_groups` shows what `ATTACH` would mount and is what the multi-group error message points users at.

## Architecture

```
┌──────────────────────────────────────────────────────────────────┐
│                        DuckDB query engine                       │
│   replacement scan ─▶ TableFunction(bind/init/scan)              │
└────────────────────────┬─────────────────────────────────────────┘
                         │
              ┌──────────▼──────────┐
              │  duckdb-zarr crate  │  this repository
              │  ─────────────────  │
              │  extension entry    │
              │  read_zarr          │
              │  read_zarr_metadata │
              │  read_zarr_groups   │
              │  ATTACH (TYPE ZARR) │
              │  replacement scan   │
              └──────────┬──────────┘
                         │  no DuckDB types cross this seam
              ┌──────────▼──────────┐
              │   zarr_reader mod   │  pure Rust; zarrs + ndarray
              │  open / metadata /  │
              │  decode / storage   │
              └─────────────────────┘
```

We follow DuckDB extension conventions: a flat top-level layout with one Rust module per registered SQL entry point and a thin extension entrypoint that wires them in (against `duckdb` pinned to `=1.10504.0` in Cargo.toml).

The `zarr_reader` module is the natural seam — it depends on [`zarrs`](https://lib.rs/crates/zarrs) and `ndarray` and exposes a small interface (open store, read schema, decode chunk into `ndarray::ArrayD`, storage adapter trait) that has no DuckDB types in its public API. Keeping that seam clean costs us nothing today and means the reader could be lifted into a shared crate later if it becomes useful to other Rust projects (e.g. zarr-datafusion). We're not designing for that reuse up front — DataFusion is async-first and DuckDB is sync-with-morsels, and trying to factor across that boundary on day one is more design overhead than it's worth.

### Bind phase

1. Open the store with `zarrs` and read group metadata.
2. Classify arrays as coordinates vs data variables. **Honor xarray's metadata first**, but read it from the right place — Zarr v2 puts `_ARRAY_DIMENSIONS` in `.zattrs` (per-array attrs), while Zarr v3 makes `dimension_names` a first-class field in the array's `zarr.json` metadata, *not* in attrs. The bind code must branch on format version: `zarrs` exposes the v3 field via `ArrayMetadataV3::dimension_names`. Falling through to attrs for v3 stores would silently miss the metadata on every array and degrade to the heuristic. Fall back to "1D = coord, nD = data" only when both metadata locations come up empty.
3. Read the `coordinates` attribute on each data variable. xarray uses this to mark *non-dimension* coordinates — typically a 2D `lat(y, x) / lon(y, x)` mesh on satellite swath data, where the coord variable is itself nD. v1 cannot represent these as scalar columns; if encountered, error at bind ("non-dimension coordinate `lat` has shape (1024, 1024); 2D coords are deferred"). **This step must run before step 5 (dim-group enumeration).** An nD array identified here as a non-dimension coord must be excluded from the data-variable pool before grouping. If step 3 runs after grouping, a 2D coord like `xc(y, x)` is indistinguishable from a data variable over `(y, x)` and silently lands in a spurious dim group. The `rasm` fixture demonstrates this: `Tair` carries `coordinates='yc xc'`; without pre-grouping exclusion, `xc` and `yc` form a spurious `(y, x)` group with two float64 columns, no error, and wrong output.
4. Suppress CF metadata variables that aren't real data:
   - **Bounds variables** (CF §7.1) — a coord with attrs like `time.attrs['bounds'] == 'time_bnds'` declares `time_bnds` as its cell-boundary descriptor. `time_bnds` then has shape `(N, 2)` and an extra `nbnds` dimension. Without filtering, `time_bnds` becomes a spurious dim group with one meaningless table — the `ersstv5` tutorial dataset triggers this exact case. Identification: parent coord has a `bounds` attribute pointing at the variable name. Fallback when the attr is missing: variable name matches `<dim>_bnds` / `<dim>_bounds` *and* shape is `(N, 2)`. Suppressed bounds are exposed in `read_zarr_metadata.attrs` on the parent coord, not as a top-level array.
   - **Scalar (0-dim) coordinates** — ROMS-style stores attach physical constants as Zarr arrays with `shape = []` (e.g. `hc = critical depth`, `Vtransform = 2`). Including them as columns would repeat the same constant on every row; they're metadata, not data. Surface in `read_zarr_metadata.attrs` and exclude from the row schema. The schema-inference code must guard the strides computation against `shape = []` so bind doesn't panic.
5. Group data variables by their dimension set. Each distinct dim set becomes a candidate output table. **Within a dim group, all selected variables must share the same chunk shape** — if `Tair` is chunked `[730, 13, 27]` and `dTdx` is chunked `[730, 7, 27]` (the `air_temperature_gradient` case — packed `int16` vs native `float32` produces different on-disk chunks), the chunk plan in init can't enumerate one cartesian product that aligns both. We error at bind with a message that names the offending pair, explains the common cause (packed-vs-native source storage, see §Type mapping > Packed integer decoding), and tells the user to query them in separate `read_zarr` calls. (See decision 6 — the v0.3 plan relaxes this restriction.)
6. Materialize coordinate arrays eagerly (they are 1D and small — ERA5 lat/lon/time fits in a few MB) so we have them for both schema metadata and later filter pruning. Coordinates are cached at the **store** level, keyed by array name. An `ATTACH` that mounts multiple views shares one cache, so `time` is loaded exactly once even if it appears in three groups. **Unindexed dimensions** — dimensions that appear in a data variable's dim list but have no backing coordinate array (the `tiny` tutorial dataset has `dim_0(5)` with no `dim_0` array) — are handled by synthesizing a `0..N` integer range as the cached coord. The synthesis is one `arange` call at bind; the scan kernel doesn't need to know whether a coord came from disk or `arange`.
7. The `read_zarr` call resolves to exactly one dim group (via `dims :=`, the variables list, or — if unambiguous — the only group present); `ATTACH` materializes all of them as views, each with its own `BindData` but a shared coord cache.
8. Build a DuckDB schema for the chosen group:
   - one column per coordinate in that group, typed from its Zarr dtype (or `INTEGER` for synthesized RangeIndex coords).
   - one column per selected data variable, typed from its Zarr dtype — *unless* the variable carries `scale_factor` / `add_offset` attrs, in which case the output type is `DOUBLE` (or `FLOAT` if both attrs are `f4`) per §Type mapping > Packed integer decoding.
9. Stash a `BindData` containing: store handle, chosen dim group, common chunk shape, projected columns *with per-column encoding tag* (plain / packed / range), the active CF sentinel per data variable (per §Type mapping > NULL masking precedence), captured DuckDB `FileSystem` handle (see §Storage backends), and a reference to the store-level coord cache.

### Init phase (chunk plan)

Compute the cartesian product of *chunk indices* across the dim group's common chunk shape (validated in bind). Each chunk-index tuple becomes a parallel scan unit. If the group's chunk shape is `[t=24, lat=10, lon=10]`, init enumerates `⌈T/24⌉ × ⌈L/10⌉ × ⌈M/10⌉` units.

Filter pushdown (see below) prunes this list before it ever runs.

### Scan phase (per chunk)

For each work unit:

1. Decode each selected data variable's chunk from `zarrs` into an `ndarray::ArrayD`. Sparse stores have **implicit chunks** — chunk keys with no backing storage object simply don't exist; `zarrs` returns a "chunk not found" error variant for these. We catch it and synthesize a fill-value chunk using the **zarr-level `fill_value`** from `zarr.json` (distinct from the CF-level `_FillValue` attr — see §Type mapping > NULL masking precedence). Cells matching the CF-level sentinel still flow through the masking step in (3) and become SQL `NULL`. Treating missing chunks as errors would break sparse satellite-swath data and is wrong by Zarr spec.
2. Compute the row-major iteration order over the chunk's logical extent.
3. Fill DuckDB's output `DataChunk`. The exact transform per data column depends on the variable's encoding tag (set at bind time):
   - **Plain numeric/string variable** — copy `data_arrays[v][row_start..row_end]` into the output flat vector. While copying, mask cells whose raw value equals the active CF sentinel (`_FillValue` or `missing_value`, per §Type mapping > NULL masking precedence) by clearing the validity bit.
   - **Packed integer variable** (carries `scale_factor`/`add_offset` attrs) — three steps per cell, in this order, per CF §8.1: (a) compare raw integer to the active sentinel; if match, clear validity bit and skip; (b) compute `decoded = raw * scale_factor + add_offset` as `f64`; (c) cast to the bind-time output type (`DOUBLE`/`FLOAT`) and write. Mask-before-decode is mandatory: applying `scale * sentinel + offset` first would shift the sentinel and the equality check would miss.
   - **Coordinate column** — gather from the cached coord arrays into a flat `FlatVector`. For unindexed dimensions (no backing coord array — see §Bind phase), `coord_values[k]` is a synthesized `0..s_k` integer range computed at bind, and the gather step treats it identically to a real coord. Dictionary-vector construction is not available in this crate version (see §Implementability risks).
4. Yield up to `STANDARD_VECTOR_SIZE` (2048) rows at a time; resume on the next call.

The local scan state holds the current chunk's decoded buffers and a cursor; the global init state holds the immutable work-unit list. This matches DuckDB's morsel-driven model and gives us free intra-query parallelism.

## The pivot algorithm

The "pivot" is the heart of this extension: the operation that turns a decoded N-dimensional Zarr chunk into a stream of 2-D rows DuckDB can scan. xarray-sql's [`iter_record_batches`](https://github.com/alxmrs/xarray-sql/blob/3adf6af86e26cfdda771dfa7df0b70c8dd636e3a/xarray_sql/df.py#L231) (entered through [`read_xarray_table`](https://github.com/alxmrs/xarray-sql/blob/3adf6af86e26cfdda771dfa7df0b70c8dd636e3a/xarray_sql/reader.py#L188)) is the reference implementation; this section translates that algorithm into our Rust + DuckDB context so the v0.1 scan callback is a port, not a fresh design.

### Inputs per work unit

For one work unit (one chunk-index tuple within one dim group):

- **`shape`** — the chunk's logical shape `(s₀, s₁, …, s_{D-1})` in the dim group's canonical order. ERA5 surface chunk: `shape = (24, 10, 10)` over `(time, lat, lon)`.
- **`coord_values[k]`** — the 1-D slice of the k-th dimension's coordinate array covering this chunk (length `s_k`). Read once at bind into the store-level cache, then sliced per chunk by index. For unindexed dims (a dim that appears in a data variable's dim list but has no backing coord array), `coord_values[k]` is synthesized at bind as `arange(0, s_k)` — see §Bind phase. The kernel doesn't care which form a coord came from.
- **`data_arrays[v]`** — the decoded chunk for each selected data variable, one `ndarray::ArrayD<T_v>` per variable, total size `prod(shape)`. After `zarrs` decode the buffer is C-contiguous, so a flat `ArrayView1<T_v>` is a zero-copy view.

The chunk holds `total_rows = prod(shape)` logical rows. For an ERA5 surface chunk that's `24 × 10 × 10 = 2,400`.

### The strided-index trick

Walking the chunk in row-major order maps each flat row index `i ∈ [0, total_rows)` to a multi-index via:

```
strides[k] = product of shape[k+1..D]      // C-order strides
i_k = (i / strides[k]) % shape[k]          // dim-k position for row i
```

This is the only piece of arithmetic that has to be exactly right. Row 0 picks coord index 0 in every dim; row 1 advances the innermost dim; row `s_{D-1}` resets the innermost and advances the next-outermost; and so on. It mirrors how `ndarray::ArrayD` lays out its memory after `zarrs` decode — which is why the data-variable side is a slice and the coordinate side is a gather.

### Per-vector loop (inner)

DuckDB scan callbacks fill one `DataChunk` per call. The per-call row budget is queried at runtime via `duckdb_vector_size()` (the C API) or the equivalent duckdb-rs output-vector size — **not** hardcoded as 2048. The default is 2048, but DuckDB can be compiled with a different `STANDARD_VECTOR_SIZE`, and an extension that hardcodes the constant will silently overflow the DataChunk buffer if the value is ever larger. (xarray-sql's default `batch_size` is 65,536 — DataFusion tolerates much larger batches than DuckDB's vectorised executor wants. Same algorithm, smaller increment.)

For each `DataChunk` covering rows `[row_start, row_end)`:

1. Build a row-index range `i = row_start..row_end`.
2. For each output column in schema order:
   - **Coordinate column k** — compute `coord_idx[i] = (i / strides[k]) % shape[k]`, then gather `coord_values[k][coord_idx[i]]` into the output flat `FlatVector`. Dictionary-vector construction is not available (see §Implementability risks).
   - **Data-variable column v** — copy the slice `data_arrays[v][row_start..row_end]` into the output flat vector. Zero-copy where DuckDB's vector layout permits, `memcpy` otherwise. For *packed* and *NULL-masked* encodings the copy becomes a transform — see §Scan phase > step 3 for the exact pipeline.
3. Apply `_FillValue` masking: any cell whose raw value equals the array's `_FillValue` becomes SQL `NULL` via the DataChunk validity bitmap.

The data-variable copy is where the bytes move; coord gathers are negligible since coords are 1-D and tiny.

### Per-chunk loop (outer)

A single chunk yields `⌈total_rows / STANDARD_VECTOR_SIZE⌉` calls to the scan callback. The local scan state holds:

- the work-unit's chunk-index tuple
- the decoded `data_arrays[v]` for each selected variable (lifetime: one work unit)
- a cursor `next_row_start` that survives across scan calls

When `next_row_start == total_rows`, the local state advances to the next work unit from the global init list; the previous chunk's `data_arrays` are dropped.

### What the algorithm does NOT compute

Three things that look like they should be in the inner loop but aren't:

- **The cartesian product of coordinate values.** xarray-sql never materialises an N-dimensional grid of coord tuples; the strided-index trick reconstructs each row's coord values on demand from the 1-D coord arrays. We don't build any intermediate "long-format" table either.
- **Coord broadcast over the chunk shape.** xarray-sql's simpler `dataset_to_record_batch` uses `np.broadcast_to(coord.reshape(reshape), shape).ravel()` — broadcast is zero-copy, the `ravel()` forces a copy. The streaming `iter_record_batches` skips even that by computing `coord_idx` per batch. We use the streaming form because DuckDB scans incrementally; the all-at-once form has no advantage when the consumer pulls 2,048 rows at a time.
- **Re-decoding chunks.** Each Zarr chunk is decoded exactly once per work unit. The `data_arrays` stay live in the local scan state until the chunk is fully drained; the inner per-vector loop only slices.

### Invariants bind enforces, scan assumes

The pivot is correct only if the chunk is internally consistent. Bind enforces, before any work unit runs:

- All selected data variables in the dim group share the same `shape` (decision 6 — uniform chunks per dim group).
- The dim group's canonical dim order matches the data variable's `dimension_names` from xarray's metadata. xarray-sql derives it from `first_var.dims`; our bind picks the same source so coord broadcast and data ravel use the same axis ordering.
- Coordinate arrays are 1-D and length-matched to their dim's chunk size — pulled from the store-level cache, not re-read per chunk.

If any of these drift, the algorithm silently produces wrong rows. They're checked once at bind, not repeatedly at scan, because chunks within a Zarr group don't change shape mid-query.

### Filter pushdown plugs in here

xarray-sql's `_block_metadata` computes per-partition `(min, max)` for each dim from the actual coord array slice — using `np.min/max`, not first/last, so descending lat (ERA5: 90 → -90) gives the right bounds. We do the same in the chunk planner: for each candidate work unit, `(min, max)` per dim is computed once from the cached coord array (cheap, since coords are tiny) and any work unit whose `(min, max)` doesn't intersect the predicate is dropped. Pruning happens between init and the first scan call, so the scan callback only ever sees work units it must execute.

## Type mapping

The base dtype mapping is mechanical:

| Zarr dtype                         | DuckDB type                              | Notes                                       |
| ---------------------------------- | ---------------------------------------- | ------------------------------------------- |
| `i1/i2/i4/i8`                      | `TINYINT`..`BIGINT`                      |                                             |
| `u1/u2/u4/u8`                      | `UTINYINT`..`UBIGINT`                    |                                             |
| `f4/f8`                            | `FLOAT`/`DOUBLE`                         | NaN preserved                               |
| `bool`                             | `BOOLEAN`                                |                                             |
| `M8[ns]` / `M8[us]`                | `TIMESTAMP_NS` / `TIMESTAMP`             | native NumPy datetimes; mapped directly     |
| CF-encoded time (`f4/f8/i4/i8`)    | `FLOAT`/`DOUBLE`/`INTEGER`/`BIGINT`      | raw on-disk dtype; CF decoding deferred     |
| `S<n>` (fixed bytes)               | `BLOB`                                   |                                             |
| `U<n>` (UTF-32)                    | `VARCHAR`                                | decoded                                     |
| structured / object                | unsupported v1                           | error at bind                               |

CF-encoded time appears in real stores as `int32`, `int64`, `float32`, or `float64` depending on the source NetCDF — RASM uses `f8` + `noleap`, `air_temperature` uses `f4` + `gregorian`, ERA5-style stores use `i8`. We surface the raw on-disk dtype; decoding is deferred (see Phased plan / Later and decision 3). The `units` and `calendar` attrs ride along into `read_zarr_metadata.attrs` so users know what they're decoding against.

### Packed integer decoding (CF §8.1)

A data variable whose **on-disk dtype is an integer type** (i8/u8/i16/u16/i32/u32/i64/u64) *and* that carries `scale_factor` and/or `add_offset` attrs is *packed*: the on-disk integer is a quantization of a real-valued measurement. **The integer dtype is required.** A float array that incidentally carries `scale_factor` as legacy metadata (measurement precision, grid resolution) must NOT be decoded — applying `scale * value + offset` to already-decoded floats would corrupt them by a factor of ~100×. The trigger condition is `integer_dtype AND (has scale_factor OR has add_offset)`, not the presence of attrs alone.

Real example from the `air_temperature_gradient` xarray tutorial dataset:

```
Tair: dtype=int16, attrs={scale_factor: 0.01, add_offset: 0.0}
```

Reading the raw `int16` and exposing it as DuckDB `SMALLINT` would produce `27315` where the user expects `273.15` — a silent correctness bug across roughly a third of real-world atmospheric data. We must decode at scan time:

1. Read the raw integer chunk via `zarrs`.
2. Mask sentinel cells (`_FillValue`, `missing_value`, or zarr `fill_value`) **on the raw integer values** — CF §8.1 mandates this order, because applying `scale * sentinel + offset` would shift the sentinel and the equality check would miss.
3. For non-NULL cells, apply `value * scale_factor + add_offset` and emit the decoded value.
4. Output column type is `DOUBLE` (or `FLOAT` if both `scale_factor` and `add_offset` are `f4`) — never the on-disk integer type. Bind overrides the type mapping for any variable carrying these attrs.

### NULL masking precedence

`_FillValue` and `missing_value` are both used as masking sentinels in real Zarr stores; CF §2.5.1 documents both. They coexist and have separate semantics — the `basin_mask` tutorial dataset has `missing_value = -100` with no `_FillValue`, so checking only `_FillValue` would silently leak the sentinel into query results. Bind resolves the active sentinel for each variable in this order:

1. `_FillValue` from attrs, if present.
2. Otherwise `missing_value` from attrs, if present.
3. Otherwise no CF-level NULL masking.

This is **separate** from the array-level `fill_value` in `zarr.json`, which controls what an *implicit* (missing) chunk decodes to (see Scan phase). Conflating the two corrupts both behaviors: `fill_value` is for chunks that don't exist on disk; `_FillValue` / `missing_value` is for sentinel cells within decoded chunks.

**Base64-encoded sentinel values (xarray convention).** When xarray writes a Zarr v3 store it encodes the `_FillValue` array attribute as a **base64 byte string** regardless of the actual value — including non-NaN floats. Specifically: xarray base64-encodes the 8 raw IEEE 754 float64 bytes of the sentinel in little-endian order. Example: `_FillValue = NaN` → `"AAAAAAAA+H8="`, `_FillValue = -9.97e36` → `"AAAAAAAAnsc="`. By contrast, `missing_value` (also a CF attr) is written as a plain JSON number, and zarr.json's own `fill_value` field uses zarr-spec string literals (`"NaN"`, `"Infinity"`) — neither is base64.

The Rust reader must therefore handle `_FillValue` specially: if the JSON value is a string, base64-decode it as 8 bytes (little-endian float64) and cast to the array's on-disk dtype for comparison. If the decoded sentinel is NaN, the equality test `value == sentinel` always returns false (IEEE 754 NaN ≠ NaN); use `value.is_nan()` instead. `missing_value` is read as a plain numeric JSON value and compared normally. Integer sentinels are never NaN, so the base64 path only arises for float arrays.

### Endianness

Zarr dtypes carry an explicit byte-order marker (`<f4` little-endian, `>f4` big-endian, `=f4` native). `zarrs` decodes both orientations into native-byte-order `ndarray` buffers, so the copy into DuckDB's `DataChunk` is a `memcpy` with no swap. Round-trip correctness is verified against a big-endian fixture — most modern hardware is little-endian, so the bug would otherwise only surface on legacy machines.

## Storage backends

DuckDB already speaks S3, GCS, Azure, and HTTP through its `httpfs` extension and the secrets manager. Rather than implement our own object-store layer, the storage adapter is a thin shim that:

- detects the URI scheme,
- for `file://` and bare paths, uses `zarrs`'s local store directly,
- for remote schemes, delegates per-key reads to a DuckDB-backed `ReadableStorageTraits` impl that calls into DuckDB's `FileSystem` API.

`zarrs` is built around an abstract `ReadableStorageTraits` trait, so swapping in a DuckDB-backed store is a few hundred lines, not a fork.

**Capturing the FileSystem handle.** DuckDB's `FileSystem` is owned by the `ClientContext` of the active connection, not by any global. We capture a handle to it during the bind callback (the `ClientContext` is one of the bind arguments DuckDB hands us), wrap it in an `Arc`, and stash it inside `BindData` next to the coord cache. The init and scan callbacks pull the handle out of `BindData` and clone the `Arc` into each thread's local state. The Rust storage adapter then forwards each `get(key)` and `get_partial(key, range)` call to the captured `FileSystem` over FFI — credentials, retries, and filesystem-level caching all stay inside DuckDB and don't get re-implemented here. The exact `duckdb-rs` symbols for grabbing the `FileSystem` from the `ClientContext` are confirmed during v0.4 implementation; the spike (§Implementability risks) did not cover this path since remote storage is a v0.4 concern.

## Predicate & projection pushdown

DuckDB's `TableFunction` exposes `pushdown_filters` and `pushdown_projection`. We exploit both:

- **Projection pushdown** — only the data variables referenced by the query (or computed-on after pushdown of `SELECT` columns) are decoded. This is the same easy win zarr-datafusion already takes.
- **Coordinate-range filter pushdown** — for any conjunctive filter on a coordinate column (`time >= '2024-01-01' AND lat BETWEEN 30 AND 60`), we use the cached 1D coord arrays to translate the filter into an index range per dimension, then compute which chunk-index tuples intersect that range. Non-intersecting chunks are dropped from the work-unit list before init returns.
- **Statistics** — we expose per-coordinate min/max as table statistics so DuckDB's optimizer can also reason about ordering and joins.

Filters on data variables cannot be pushed (Zarr is dense, no chunk-level statistics by default); they fall back to DuckDB's normal filter execution after the scan.

**Correctness rigor — required test cases.** Filter pushdown is easy to get wrong on the boundary, and the failure mode is silently-wrong results. The implementation must pass:

- **Decreasing coordinate arrays** — ERA5 latitude runs 90.0 → -90.0; the index translation can't assume monotonic-increasing.
- **Non-uniform spacing** — gaussian grids and pressure levels are not evenly spaced; chunk index can't be `coord_value / chunk_size`. The translation goes through the cached coord array (binary-searchable since coords are sorted-by-spec), not arithmetic.
- **Exact chunk-boundary predicates** — `time = '2024-01-15T00:00:00'` where the value sits exactly on a chunk seam must include the owning chunk once, not zero or two.
- **Empty-result predicates** — `lat > 100` returns zero rows without panic and without scheduling any chunk reads.
- **Inclusive vs exclusive bounds** — `BETWEEN`, `<`, `<=`, `>`, `>=` each must intersect the chunk grid correctly.

## Concurrency & memory

- One decoded chunk per active scan thread. With ERA5-style 24×10×10 chunks at `f4` that's a few hundred KB resident per thread.
- Decode is CPU-heavy; we let DuckDB schedule across cores.
- Coordinate arrays are loaded once in bind and shared (Arc) across threads.
- An optional small LRU of decoded chunks helps when many queries hit the same data. Configuration is exposed as a DuckDB extension config variable (`SET zarr_chunk_cache_mb = 256`) rather than a custom `PRAGMA` — `SET` is the documented mechanism for loadable extensions to register tunables and is the API actually exposed by `duckdb-rs`.

## Phased plan

0. **Spike (v0.0, complete)** — verified what `duckdb-rs` exposes for replacement-scan registration, storage-extension `ATTACH` hooks, dictionary-vector construction, extension-config-variable registration, and predicate/projection pushdown. Findings recorded in §Implementability risks.
1. **MVP (v0.1)** — local-filesystem only, Zarr v3, `read_zarr` + `read_zarr_metadata` + `read_zarr_groups`, single dim group only, no pushdown beyond projection. Includes packed-integer decoding (`scale_factor` / `add_offset`) and full NULL masking precedence (`_FillValue` → `missing_value` → none) — both are correctness, not optimizations. Goal: end-to-end demo against the xarray tutorial datasets in `test/fixtures/xarray_tutorial/`.
2. **v0.2** — Zarr v2 + Blosc/LZ4 codecs (free with `zarrs`), replacement scan (via `libduckdb-sys` FFI), projection pushdown, type mapping for native datetime/string dtypes.
3. **v0.3** — Multi-group stores via `ATTACH ... (TYPE ZARR)`; coordinate-range filter pushdown; parallel scan; statistics; **coarsest-grid chunk planning** (relaxes decision 6's uniform-chunk requirement now that the scan engine is being reworked anyway). Goal: beat naive `xarray + pandas` (with dask, on the same thread budget) on a real ERA5 query. Benchmark must capture chunks-decoded vs total chunks, not just wall-clock.
4. **v0.4** — Remote stores via DuckDB filesystem FFI, secrets integration, community-extension submission.
5. **Later** — CF time UDFs (deferred until a permissively-licensed implementation path exists; nice-to-have, not blocking), chunk-level statistics (when present), aggregate pushdown, write support, 2D non-dimension coordinates, async `zarrs` if remote latency demands it.

## Open questions and decisions

Each subsection keeps the original tradeoff visible so future readers can see what was on the table, then records the call we made and why.

### 1. Convention vs. config

zarr-datafusion infers the coord/data split from dimensionality alone; xarray reads `_ARRAY_DIMENSIONS` (v2) or `dimension_names` (v3). We can either commit to one or layer them.

> **Decision:** Honor xarray's metadata first; fall back to the "1D = coord, nD = data" heuristic only when the metadata is missing.
>
> **Rationale:** xarray is the de facto producer of stores in this ecosystem, and its metadata already encodes the right answer — using it gets us correct dim names and ordering for free. The dimensionality heuristic exists for ad-hoc Zarr stores written by tools that don't follow the xarray convention.

### 2. Multi-variable layout

When a store contains many data variables, do we expose them as one wide table, one table per variable, or something in between?

> **Decision:** One wide table **per dimension group** — i.e. per distinct set of dimensions across the data variables. ERA5 → two tables: surface (`time, lat, lon`) and atmosphere (`time, level, lat, lon`).
>
> **Rationale:** A single wide table only works when all variables share dims. Real scientific stores routinely mix surface (3D) and atmospheric (4D) fields, and forcing them together would mean pad-NULL-or-bust. One-table-per-variable goes the opposite direction and wastes the natural locality. Grouping by dim set keeps schemas tight, row counts honest, and queries readable. `ATTACH` with one view per group is what makes this UX tolerable for stores with several groups.

### 3. CF-time decoding

CF-encoded time (e.g. `int64` + `units = "hours since 1970-01-01"` + `calendar = "noleap"`) needs explicit conversion. We can decode at scan time (transparent but opinionated), expose a UDF (explicit but a tiny extra hop), or defer the whole thing.

> **Decision:** Defer. CF-encoded time columns are exposed raw as `BIGINT`; users handle the conversion app-side (xarray, pandas, or a follow-up SQL macro) until a permissively-licensed implementation path is identified.
>
> **Rationale:** Be honest about what's central. The product win — SQL on a Zarr store, with chunk pruning and parallel scan — does not depend on CF time. The originally-proposed `cftime-rs` is AGPL-3.0 (incompatible with community-extension binary distribution) and unmaintained since October 2023, so the easy path is closed. Implementing CF math in-tree is doable but is several hundred lines of calendar code that would gate every other v0.2 deliverable on its testing burden. Keeping CF support on the deferred list lets the v0.2 milestone land cleanly and lets us pick this up properly when the right dependency exists, without rushing the call.

### 4. Replacement scan ambiguity

`.zarr` is a directory, not a file. DuckDB's replacement scan fires on any string literal in `FROM`, so we need to claim only the strings that actually point at a Zarr store without stat'ing every random path.

> **Decision:** Two-step probe — (a) **normalized** suffix check (case-insensitive `.zarr`, trailing slash stripped to handle `s3://bucket/store.zarr/` cloud-console paths), then (b) a single stat for `zarr.json` (v3) or `.zgroup` (v2) at the path. Only if both pass do we claim the path; otherwise fall through to DuckDB's normal scan chain. Suffix-less Zarr groups are reachable via `read_zarr(path)` directly; auto-claim of suffix-less paths is deferred to avoid stat'ing every string literal.
>
> **Rationale:** A single stat call against a known filename is essentially free, and the suffix gate keeps us from probing every Parquet path the user types. The `FROM 'foo.zarr'` UX is the headline pitch of this extension; spending one stat per query to make it seamless is the right trade. The normalization is enumerated explicitly because the original wording missed the trailing-slash and case-insensitive cases.

### 5. `zarrs` async story

`zarrs` has an async API behind a feature flag; the DuckDB scan callback is sync.

> **Decision:** Sync. One chunk decoded per scan call; intra-query parallelism comes from DuckDB's morsel scheduler dispatching across threads.
>
> **Rationale:** DuckDB's table-function callbacks are sync; bridging async would force a `tokio` runtime per scan or `block_on`, both of which add complexity for unclear gain. zarr-datafusion ships sync today, and it's still well ahead of any non-DataFusion alternative. We can revisit if remote-store I/O latency starts dominating wall-clock time on real workloads.

### 6. Mismatched chunk shapes within a dim group

Within a dim group, two data variables might be chunked differently — e.g. `temperature[24,10,10]` and `humidity[1,20,20]`, both over `(time, lat, lon)`. The init phase enumerates a single cartesian product of chunk indices and so cannot align both grids without finer-grained iteration. Three options: (a) require uniform chunk shape per dim group, error at bind on mismatch; (b) plan against the coarsest chunk grid and re-read finer-chunked variables multiple times per work unit; (c) iterate at the row level rather than the chunk level.

> **Decision (v1):** (a) — require uniform chunk shape across all selected variables in a dim group. Bind checks and fails with a message naming the offending pair, explaining the common cause (packed `int16` vs native `float32` storage from the source NetCDF), and pointing at the workaround (`read_zarr('store.zarr', variables := ['humidity'])`). The new single-array equivalent is `read_zarr('store.zarr', array_path := 'humidity')`.
>
> **Empirical correction:** the original framing said this would "rarely be observed in practice." That was wrong. The `air_temperature_gradient` xarray tutorial dataset (NCEP reanalysis — same provenance as ERA5) has `Tair` chunked `[730, 13, 27]` while `dTdx`/`dTdy` share dims but are chunked `[730, 7, 27]`, because xarray preserves source-NetCDF chunking per variable and packed-vs-native storage routinely produces different chunks. This is common in atmospheric reanalysis stores, not exotic.
>
> **Rationale:** Option (a) is still right for v1 — it's correct, the implementation is one comparison at bind, and it forces the user toward an explicit query that does work. The bind error is ergonomically painful when it fires often, but the alternatives are heavier than they look:
> - Option (b) — coarsest-grid plan — means each work unit re-reads finer-chunked variables N times (once per finer chunk that fits inside the coarse cell). That doubles the scan engine's complexity and breaks the "one chunk decoded per work unit" invariant the rest of the pivot kernel rests on.
> - Option (c) — row-level iteration — defeats the parallel-scan model entirely.
>
> Option (b) is queued for v0.3 alongside the multi-group `ATTACH` work, where the scan engine is being touched anyway. v1 ships the bind-error path; v0.3 relaxes it.

## Why this is worth building

Every team running a Pangeo-style pipeline already has DuckDB installed for the tabular side of their workload. Today they shuttle data through Parquet exports or notebook glue to bridge the two worlds. A native extension collapses that bridge: ad-hoc SQL on a Zarr store with no copy, no external service, and the same DuckDB session that already holds their joins, dashboards, and BI tooling.

It's also the smallest piece of the xarray-sql / zarr-datafusion family, because DuckDB does the optimizer, scheduler, and SQL frontend for us. Our job is just the scan.
