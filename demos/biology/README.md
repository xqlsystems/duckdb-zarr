# duckdb-zarr for biologists

One static page, two real remote datasets, editable SQL, and interactive plots.
DuckDB-Wasm runs SQL in a browser worker. There is no query server, dataset
preparation step, local dataset directory, or bundled scientific data payload.

## Run locally

Requires Node.js 20.19+ or 22.12+ and `gh` to fetch the tested extension:

```sh
cd demos/biology
npm ci --ignore-scripts
# Tested wasm_eh extension from main at 473cb3b, compatible with this WASM engine.
gh run download 40233921996 --repo xqlsystems/duckdb-zarr \
  --name zarr-v2.0.0-extension-wasm_eh --dir public
npm run dev
```

Open Vite's printed address. The first load fetches the original datasets over
HTTPS. Both examples execute real `read_zarr` SQL automatically. Edit a cell and
click Run SQL, or press Cmd/Ctrl+Enter. Filters replace and execute the editor's
SQL; Run SQL executes it as written. Aggregate queries clear the plot and show a table.
An internet connection and CORS-enabled upstream sources are required; failures
are reported explicitly, never replaced by a local or invented dataset.

The tested package is `@duckdb/duckdb-wasm@1.33.1-dev110.0`; the embedded engine
reports `v2.0.0-dev88614`. The extension is unsigned, so
`allowUnsignedExtensions` is deliberately enabled. If the artifact expires,
build/download a matching `wasm_eh` extension and rerun the browser checks.
Do not substitute the older binary in `demos/zarr-map/public` blindly.

## Original remote sources

### AnnData: PBMC 68k reduced

700 cells × 765 genes of single-cell RNA-seq expression, not raw counts. The
source already contains the UMAP and `bulk_labels` annotations. No coordinates,
expression values, or cell types are generated or fitted by this page.

The original Scanpy Zarr ZIP is pinned to commit
`1a421f222c65987e010ccd395d6065cf5a9682ce` in `src/remote_anndata.js`. Its URL
and SHA-256 digest are checked into that module. The browser fetches the
approximately 1.65 MB archive directly from `raw.githubusercontent.com`, verifies
its digest, and registers its original Zarr entries in DuckDB-Wasm memory.
Neither metadata nor compressed chunks are rewritten. The source is Zarr v3,
including its original sharding/Zstd codecs, decoded by duckdb-zarr.

This source is published as an archive rather than an established public
directory endpoint. Its SQL-only `https://duckdb-wasm-memory.invalid/...`
namespace is explicitly a virtual browser-memory handle, not a remote host:
no requests to it reach the network. Original entries are discarded when the
source is replaced or the worker terminates. Duplicate ZIP metadata uses the
last central-directory entry, matching Python's ZipFile semantics.

`pbmc_obs` preserves `bulk_labels` and adds only a SQL alias, `cell_type`.
`pbmc_coordinates` exposes the stored UMAP. Queries pivot the embedding
components and join by the actual `obs` identifier, not row order. These labels
come from expression-profile matching to reference sorted immune-cell
populations, not an independently measured cell-type assay.

### OME-Zarr: IDR idr0062A, image 6001240

The original S3-compatible public directory is:

```text
https://uk1s3.embassy.ebi.ac.uk/idr/zarr/v0.4/idr0062A/6001240.zarr
```

The default is the full-resolution level `0`, not a downsampled derivative:
shape `[c,z,y,x] = [2,236,275,271]`, XYZ spacing approximately
`[0.3604,0.3604,0.5002]` µm. Both source channels are loaded and displayed
together: **LaminB1 blue**, **Dapi yellow**, using the source's OME colors.
The channel selector can isolate either one. There is no segmentation overlay
or per-cell color key.

The browser fetches the 472 original compressed chunks plus three metadata
objects with bounded concurrency (12), then registers their exact original
HTTPS URLs in DuckDB-Wasm memory. SQL continues to reference the real S3 URL.
This downloads approximately 32.5 MiB, not a repacked store; the original
Blosc/LZ4 bytes are decoded by duckdb-zarr. Nothing is uploaded, served by the
site, written to OPFS/IndexedDB, or persisted by application code. Source
fetches opt out of the browser's persistent HTTP cache.

An earlier codec-hang diagnosis was incorrect. Unprefetched reads were making
hundreds of serial requests, including chunks excluded by SQL's channel
filter. All pyramid levels have 472 chunks, so a coarse level reduces bytes
but not round trips. Parallel original-object prefetch fixes transport without
changing the extension or inventing Zarr data. Some optional metadata misses
still incur extra remote round trips. Initial loading therefore takes tens of
seconds; subsequent queries reuse registered RAM buffers.

Source: Blin et al. (2019), *NesSys: a novel method for accurate nuclear
segmentation in 3D*, PLoS Biology 17(8), e3000388,
DOI `10.1371/journal.pbio.3000388`; IDR dataset DOI `10.17867/10000125`,
CC BY 4.0. The page links to IDR and the ome-arrow VTK-rendering inspiration.

## Volume display

vtk.js reconstructs an interleaved multi-channel texture directly from Arrow
record batches. Avoiding `table.toArray()` prevents millions of retained JS row
objects. Compact SQL coordinate types keep the full 35,175,800 intensity rows
manageable. Channel identity comes from the returned `c` column, not a possibly
stale selector value. Channels share a single physical grid and additively mix;
SQL gaps remain zero rather than being compacted into the wrong positions.

Full native XY resolution, linear texture interpolation, and fixed full-pixel
ray rendering replace the coarse default. Per-channel 70th/99.5th-percentile
nonzero intensity windows suppress diffuse background haze. This is display
contrast only: SQL values and physical spacing remain original. It is an
exploratory view, not a quantitatively normalized channel comparison.

Drag to orbit, scroll to zoom, double-click to fit, or pause/resume the gentle
automatic rotation. Animation stops offscreen, in hidden tabs and during manual
interaction; reduced-motion preferences start it paused. Brightness, view and
cutoff controls remain removed. WebGL2 is required.

Safety limits: 40 million SQL rows, four channels, 512 voxels per axis,
20 million spatial voxels, 256 MiB of reconstructed float texture data, and
256 MiB of compressed source downloads. The full sample fits these limits.
Large SQL selections fail explicitly rather than silently losing data.

## Change the data source

Each example has a remote source input and Load source / Reset to sample.
AnnData directory sources must have compatible `obs`, `bulk_labels` and
`obsm/X_umap` tables; they pass through to `read_zarr` directly. The pinned
Scanpy ZIP is supported in memory; additional ZIP URLs require an explicit
SHA-256 digest through the loader API, rather than trusting arbitrary archives.
Stored, unencrypted, non-ZIP64 entries are supported; other ZIP formats fail
explicitly.

OME sources must be public OME-NGFF Zarr v2 directories with a compatible
`c,z,y,x` level `0` and metadata channel colors. Metadata are read from the new
source, not inherited from the sample. Object counts, download size and render
bounds are budgeted. CORS is required for both metadata and chunks. Use the
bucket's public HTTPS endpoint rather than an authenticated `s3://` URL. This
page neither asks for nor handles storage credentials.

A chunk-key HTTP 404 can represent a valid omitted fill-value chunk. The
prefetcher leaves that object absent, without retrying it or registering
fabricated bytes, so `read_zarr` applies its normal fill handling (including
SQL NULL for nonzero fill sentinels). Metadata 404s, other HTTP errors and
network failures still fail the load and clean up prior registrations.

SpatialData/tissue remains paused and is not loaded or shown on the page.

## Static deployment

`npm run build` creates `dist/` with only the application, logos, DuckDB engine
and zarr extension. It must not contain `data/`. No Python or data preparation
is needed. Preserve real HTTP 404s; do not use an SPA fallback. Relative asset
paths (`base: './'`) support a GitHub Pages project subdirectory.

Standalone app development does not require the documentation build.
For the cohesive docs + examples site, run `uv run scripts/build_docs.py` from
the repository root and serve `site/`. The docs live at `/` and this app at
`/biology/`, with two-way navigation to the AnnData and OME-Zarr guides. See
[Documentation website](../../docs/website.md).

`.github/workflows/biology-demo-pages.yml` downloads an existing compatible
wasm_eh artifact, installs JS dependencies, builds the combined site, and deploys
`site/` with GitHub's Pages artifact actions. It runs for documentation/example changes on `main`
or via manual dispatch, reusing a successful main extension distribution run. It does not download or rehost datasets.
`public/.nojekyll` is retained for other static deployment methods.

## Verify

```sh
npm run test:unit
npx playwright install chromium
npm test
npm run build
DEMO_PREVIEW=1 npm test
```

Browser tests use real remote data. The live workflow checks original source
requests (and no site `/data/` or virtual-host requests), full-resolution grid,
both channel colors in actual rendered pixels, per-channel intensity totals,
UMAP coordinates, filters, SQL errors, empty results, source reset, grouped
statistics, depth selection, orbit and motion accessibility. Fault tests
intercept only deliberate missing-source/extension failures; successful SQL
responses are never fabricated. A consolidated workflow avoids downloading the
same 472 chunks for every UI assertion. Tests need network access and upstream
availability. Screenshot evidence is written under ignored `test-results/`.
Chromium is verified; Safari/Firefox and a deployed public host need separate
checks. The large vtk.js bundle warning is nonblocking.
