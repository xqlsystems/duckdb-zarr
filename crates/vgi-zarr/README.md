# vgi-zarr

`read_zarr` as a [VGI](https://query.farm/vgi/) worker, built on
[`zarr-core`](../zarr-core). Experimental; see
[docs/vgi-wasm-demo.md](../../docs/vgi-wasm-demo.md) for the design, results and caveats.

Not a member of the root Cargo workspace: it depends on Query.Farm's VGI SDK,
which is under the Query.Farm Source-Available License.

| | |
|---|---|
| `src/read_zarr.rs` | `read_zarr`, `describe_dims`, `last_scan` table functions; pushdown → chunk pruning |
| `src/http_store.rs` | zarrs store over ranged GETs (sync XHR in the browser, reqwest natively) |
| `wasm/build.sh` | emscripten build of the browser worker (`wasm/dist`) |
| `web/` | the demo page, a dev server with COOP/COEP, and `build-site.sh` for static hosting |

Functions (in the catalog `zarr` once attached):

```sql
ATTACH 'zarr' (TYPE vgi, LOCATION '<path>/target/release/vgi-zarr');  -- browser: 'worker:<url>/vgi-worker-boot.js'
SELECT * FROM zarr.describe_dims('https://data.dynamical.org/noaa/gfs/analysis/latest.zarr');
SELECT time, temperature_2m
FROM zarr.read_zarr('https://data.dynamical.org/noaa/gfs/analysis/latest.zarr')
WHERE time BETWEEN TIMESTAMP '2025-07-01' AND TIMESTAMP '2025-07-31'
  AND latitude = 40.75 AND longitude = -74;
SELECT * FROM zarr.last_scan();  -- what the last scan pruned to
```

Logging: natively, `VGI_LOG=info VGI_ZARR_LOG=<file>` (a file, because the
`vgi` extension discards worker stderr). In the browser, info logs go to the
console.
