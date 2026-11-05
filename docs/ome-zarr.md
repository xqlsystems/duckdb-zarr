# Querying OME-Zarr

OME-Zarr images commonly contain several resolution levels and nested label
arrays. The examples below run against this repo's small synthetic fixture
(`make generate_fixtures` first). Start by listing the available arrays:

```sql
SELECT name, dims, shape, dtype
FROM read_zarr_metadata('test/fixtures/bioimage/ome_zarr/synthetic_multichannel.ome.zarr');
```

The `array_path` argument is optional. Use it when a store has multiple
resolution levels, nested labels, or otherwise ambiguous array groups. The path
is the same store-relative path reported by `read_zarr_metadata`.

Then select a resolution level and aggregate by channel:

```sql
SELECT c, AVG(value) AS mean_intensity
FROM read_zarr('test/fixtures/bioimage/ome_zarr/synthetic_multichannel.ome.zarr', array_path='0')
GROUP BY c;
```

Here, `0` is the conventional path for the highest-resolution level. When you
select a single array with `array_path`, its data is exposed as a `value` column
and its xarray dimension names (`c`, `y`, `x`) become the other columns — add a
`WHERE y BETWEEN ... AND x BETWEEN ...` clause to aggregate a sub-region.

Nested label arrays use the same selector:

```sql
SELECT value AS label, COUNT(*) AS pixels
FROM read_zarr('test/fixtures/bioimage/ome_zarr/synthetic_multichannel.ome.zarr', array_path='labels/nuclei/0')
WHERE value > 0
GROUP BY label;
```

## Remote OME-Zarr

Public OME-Zarr images can be read straight from a URL. Bioimage stores usually
lack consolidated metadata, so whole-store operations (`read_zarr_metadata`, or
`read_zarr` without `array_path`) aren't available remotely — but a specific
resolution level reads by `array_path`, with `c`/`z`/`y`/`x` recovered from the
OME `multiscales.axes`:

```sql
-- A public image from the Image Data Resource (IDR)
SELECT c, z, y, x, value AS intensity
FROM read_zarr(
  'https://uk1s3.embassy.ebi.ac.uk/idr/zarr/v0.4/idr0062A/6001240.zarr',
  array_path='0'
)
LIMIT 10;
```

Filters and aggregates over a full pyramid level scan every chunk (there is no
sub-chunk filter pushdown yet), so keep remote exploratory queries bounded with
`LIMIT` or use a coarse resolution level.

The [duckdb-zarr for biologists browser example](../demos/biology/README.md)
queries this original remote image at its full-resolution level `0`, with both
channels displayed together in a rotating VTK.js volume. To avoid hundreds of
serial chunk requests, the browser fetches the unchanged compressed objects
in parallel and registers their original HTTPS URLs in DuckDB-Wasm memory.
No prepared dataset or proxy is served by the site. The original Blosc/LZ4
codec works in the tested WASM build; long unprefetched reads were transport
latency, not evidence of a codec hang.
