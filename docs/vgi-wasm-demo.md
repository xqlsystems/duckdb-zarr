# Zarr SQL in the browser: a VGI worker on a shared core

Status: working spike (October 2026). Follows on from
[filter-pushdown-learnings.md](filter-pushdown-learnings.md), whose two
blockers for a browser demo were no async I/O and no filter pushdown.

## What this is

`read_zarr` served two ways from one engine-independent core:

```
crates/zarr-core   metadata, dimension groups, CF decoding, pruning (no duckdb, no arrow)
   ├── the extension (repo root)  DuckDB C API table functions → DataChunks
   └── crates/vgi-zarr            VGI worker → Arrow RecordBatches
         ├── native: a subprocess the `vgi` extension spawns (stdio)
         └── browser: wasm32-unknown-emscripten, linked into a Web Worker that
             Haybarn-wasm (Query.Farm's DuckDB-Wasm) reaches over a SharedArrayBuffer ring
```

[VGI](https://query.farm/vgi/architecture/) moves the table function out of
DuckDB's process into a worker that speaks Arrow IPC. DuckDB plans the query
and pushes projections and filters to the worker. The worker turns the filters
into per-dimension index masks, then into the chunks it needs to read. It runs
on any DuckDB that has the `vgi` extension, including the browser build.

The demo (`crates/vgi-zarr/web`) has a dataset chooser over ten
[dynamical.org](https://dynamical.org/catalog/) weather stores: GFS, GEFS,
HRRR, MRMS, ECMWF IFS ENS and AIFS, and DWD ICON-EU. You can also paste any
CORS-enabled Zarr URL. Choosing a dataset creates a view, then generates
example queries that fit its chunk layout: a map, a time or forecast series,
area statistics, and `describe_dims`. The page fetches chunks straight from the
bucket. No proxy or backend is involved, only static files.

## Results

The headless Chromium smoke test (`?autorun=all`) runs every preset of every
dataset in one tab. Typical numbers over a ~3 MB/s link:

| dataset | read unit (subchunk) | map | series at a point |
|---|---|---|---|
| GFS forecast | 1 × 105 × 121 × 121 | 15 of 1,146,240 units, 4.7 s | 2 units, 1.5 s |
| GFS analysis | 1440 × 50 × 50 | 15 of 14,790, 4.6 s | 2 units, 2.5 s |
| GEFS forecast 35-day | 1 × 31 × 64 × 17 × 16 | 21 of 25,565,220, 4.8 s | 3 units, 0.7 s |
| HRRR forecast 48 h | 1 × 49 × 265 × 300 | 4 of 289,200, 4.7 s | 1 unit, 1.3 s |
| MRMS CONUS hourly | 648 × 100 × 100 | 10 of 396,900, 2.9 s | 2 units, 0.8 s |
| ECMWF IFS ENS | 1 × 85 × 51 × 32 × 32 | 12 of 956,340, 6.2 s | 1 unit, 1.2 s |
| ECMWF AIFS single | 1 × 61 × 241 × 240 | 6 of 66,528, 2.1 s | 1 unit, 1.0 s |

All 40 presets (four per dataset) pass in one tab. The wasm heap peaks around 330 MB. Boot (DuckDB, the
`vgi` extension, the worker) takes 3–5 s.

Natively the same worker answers, for example, a one-month point query on GFS
analysis from 1 of 14,790 subchunks with exactly the 721 matching rows.

## Findings

Correctness:

- **The `vgi` extension (Haybarn 1.5.5) drops pushed filters from the plan.**
  `EXPLAIN` shows no FILTER above a scan with pushed filters, even with
  `filters_exactly_applied: false`. A worker that only prunes therefore returns
  extra rows. The worker sets `auto_apply_filters: true`, so the SDK filters
  every emitted batch. That makes it correct, not just faster.
- **BETWEEN reaches the worker as `>= AND <=`, with inclusivity intact.** The
  v2 C API's BETWEEN bug (see the pushdown doc) does not apply here.
- **Exact masks without seeing the literals.** `get_column_bounds` casts
  literals to i64, which is too coarse for a 0.01° grid. Instead, the worker
  evaluates the pushed filters on a batch holding one dimension's coordinates,
  with every other column set to NULL. Under three-valued logic a row comes out
  `false` only if that dimension alone excludes it. This applies only when every
  filter is a comparison, IN or null test, and an all-NULL row is checked first
  so that `x IS NOT NULL` cannot zero out a dimension. Otherwise the worker
  falls back to the i64 bounds, widened for floats.

Sharding and HTTP:

- **Read units are subchunks.** dynamical's stores are sharded Zarr v3, and a
  GFS shard decodes to about 440 MB. When every variable shares a subchunk
  shape, the worker plans work units on the subchunk grid and reads each one
  with `retrieve_subchunk_opt`. That costs a cached shard-index suffix read
  plus one byte range. The extension still reads whole shards, so this logic
  belongs in `zarr-core`'s work-unit planning next.
- **`zarrs_http` sends a HEAD before every range read and requires
  Content-Length.** Cloudflare (data.dynamical.org) omits that header on HEAD.
  The worker uses its own `HttpStore`: plain ranged GETs, suffix ranges for
  shard indexes, sync XHR in the browser and blocking reqwest natively, so
  native tests exercise the browser's request pattern.

Concurrency and memory in the browser:

- **Concurrency comes from threads, not async I/O.** Each rayon thread is an
  emscripten pthread, which is a Web Worker, and a sync XHR blocks only that
  worker. Eight units decode in parallel.
- **Use dlmalloc, not mimalloc.** With mimalloc under emscripten pthreads, the
  heap grew past 2 GB across a few datasets and aborted. With dlmalloc it
  stays around 330 MB. Linker settings: `MAXIMUM_MEMORY` 2 GB (4 GB failed to
  reserve on some browsers: "Out of memory" at startup), 12 pthreads.
- **Opened stores are an LRU of 4.** Shard-index caches reset past 4,096
  entries.
- **Size presets by bytes, not chunk count.** An IFS ENS read unit spans
  51 members × 85 lead times, so a 12-unit map pulls about 200 MB. The page
  caps each map at about 120k cells and about 128 MB of decoded read units.

Problems in other projects:

- **Haybarn-wasm 1.5.5-rc8 VGI bridge: an RPC right after a large result
  stream can fail ("RPC response stream EOF (no schema)") or hang the
  connection.** The page therefore receives scan stats from the worker on a
  `BroadcastChannel` instead of calling `zarr.last_scan()` after each query.
- **Emscripten 6 breaks `@query-farm/vgi-worker-runtime` 0.1.0 in two ways.**
  The pre-js needs a leading `;`, and `Module.mainScriptUrlOrBlob` was removed,
  so `wasm/build.sh` patches `pthreadMainJs`.
- **Haybarn's ESM imports `apache-arrow` by bare name.** The page maps it to a
  vendored copy of arrow 17 with an import map.

Availability and licence:

- **`vgi` is not on upstream DuckDB's community extension repository.** Every
  1.5.x URL returns 404, so VGI needs Haybarn natively and in the browser.
- **Licence.** The VGI SDK and extension are under Query.Farm's Source-Available
  License: derivatives must carry it, and competing offerings are not allowed.
  That is why `vgi-zarr` is excluded from the Cargo workspace. `zarr-core` and
  the extension do not depend on it. Haybarn itself is MIT.

## The workspace split and DuckDB v2

The root `Cargo.toml` is still the extension package, so DuckDB's extension CI
builds it unchanged. It adds `[workspace] members = ["crates/zarr-core"]`.
`make clippy` now covers the workspace, and `make test-rust` runs
`cargo test --workspace`.

How the split helps a DuckDB v2 port:

- **Only the adapter changes.** The v2 entry point, its still-moving C API ABI
  and the unpublished `duckdb-neo` bindings stay in the extension adapter. A
  v1 and a v2 adapter can ship side by side on one core.
- **Pushdown design is shared.** v2 and VGI both turn filters into
  per-dimension masks, then into units to read. The v2 adapter can prune and
  claim nothing, as VGI does here, which also avoids the BETWEEN bug.
- **The VGI worker doesn't track DuckDB versions at all.** Query.Farm rebuilds
  `vgi`, and the worker stays as it is.
- **Arrow version churn stays in the VGI adapter.** VGI uses arrow 60 and
  duckdb-rs uses arrow 58.

What it costs:

- **Two adapters can drift** on parameters (`dims`, `decode_times`, …) and type
  mapping. Shared sqllogictests should run against both.
- **The hot path writes through `ColumnValues`.** The extension downcasts to
  its `FlatVector` writers. The worker currently goes through `f64` per element
  and emits Int64/Float64, where it should map each dtype to the matching
  Arrow type.

## Next steps

- **Icechunk.** data.dynamical.org was scheduled to switch off on 2026-09-30
  ([migration notice](https://dynamical.org/migration-2026/)) and is being
  retired one dataset at a time. The replacement Icechunk repositories send
  CORS headers too
  (`dynamical-noaa-gfs.s3.us-west-2.amazonaws.com/...icechunk`). The worker
  needs a read-only Icechunk store over the same `HttpStore`.
- **Move subchunk planning and exact-mask pruning into `zarr-core`** so the
  extension and a v2 adapter get them.
- **Strings** are emitted as NULL by the worker. **Dtype-exact Arrow output**
  is listed above.
- **Hosting at wx.xql.systems.** `web/build-site.sh` assembles a static site
  with a `_headers` file for COOP/COEP. Use a host that honours it (Netlify,
  Cloudflare Pages) over HTTPS. `duckdb-coi.wasm` is about 44 MB, above
  Cloudflare Pages' 25 MiB per-file limit.

## AutoMet: natural language → SQL in the same page

The constraints are WebGPU, a model under about 2 GB, and a narrow schema (one
view, known dimensions and variables). This is a much easier problem than
general text-to-SQL.

- **Engine.** [WebLLM](https://github.com/mlc-ai/web-llm) runs MLC-compiled
  models on WebGPU, in a worker, with an OpenAI-style API.
  [Duck-UI](https://github.com/caioricciuti/duck-ui) already pairs WebLLM with
  DuckDB-Wasm for text-to-SQL.
- **Model.** `Qwen2.5-Coder-1.5B-Instruct-q4f16_1-MLC` (about 1 GB, prebuilt
  for WebLLM) is the default choice. Its 3B and 7B siblings, or a newer small
  coder, trade download size for accuracy. MotherDuck's
  [DuckDB-NSQL-7B](https://github.com/NumbersStationAI/DuckDB-NSQL) knows
  DuckDB dialect specifically, but at 7B it is too heavy for most browsers.
- **What makes it work at 1.5B.** The prompt carries everything the page
  already has: the view name, its columns, the `describe_dims` rows (ranges
  and read units), today's date, and the generated presets as few-shot
  examples. The page then checks each answer before running it: `EXPLAIN` it,
  require every dimension to be filtered (else re-ask), and feed DuckDB's
  error back for one retry.

## Running it

```sh
# native worker (needs Haybarn: pip install haybarn)
cd crates/vgi-zarr && cargo build --release
#   ATTACH 'zarr' (TYPE vgi, LOCATION '<path>/target/release/vgi-zarr');
#   SELECT * FROM zarr.read_zarr('https://data.dynamical.org/noaa/gfs/analysis/latest.zarr') WHERE ...;

# browser worker (emsdk + nightly with rust-src; @query-farm/vgi-worker-runtime)
VGI_WORKER_RUNTIME=<runtime>/wasm ./wasm/build.sh
python3 web/serve.py --haybarn <@haybarn/haybarn-wasm>/dist   # http://127.0.0.1:8443/
# ?ds=<dataset id or URL>  ?autorun[=all]  ?sql=...  (headless smoke tests)
```
