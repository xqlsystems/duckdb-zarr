# zarr-map demo

Query a Zarr store with `read_zarr` inside DuckDB-Wasm and draw the result on a
deck.gl map. There is no JS Zarr reader: the extension fetches chunks through
DuckDB-Wasm's HTTP filesystem.

## Tested runtime

The demo pins `@duckdb/duckdb-wasm` to `1.33.1-dev110.0`, which reports
`DuckDB v2.0.0-dev88614`. This is a published **preview** browser runtime, not
an assertion that the stable DuckDB-Wasm package has moved to DuckDB 2.0.
The older `1.33.1-dev57.0` runtime reports DuckDB 1.5.4 and rejects the released
extension's v1.5.6 C API requirement.

The current package exports `duckdb-base.wasm` and
`duckdb-browser-base.worker.js`; the old `duckdb-eh.wasm` imports no longer
exist. Use the extension's `wasm_eh` artifact with this base browser bundle.
Both loading and queries are tested in headless Chromium, including visible
WebGL output. Safari and Firefox have not been verified.

## Run

From the repository root, get a `wasm_eh` artifact from a successful main CI run:

```sh
gh run list --repo xqlsystems/duckdb-zarr --branch main \
  --workflow MainDistributionPipeline.yml --status success
# Replace <run-id> with a run from the list above.
gh run download <run-id> --repo xqlsystems/duckdb-zarr \
  --name zarr-v2.0.0-extension-wasm_eh --dir build/demo-wasm
mkdir -p demos/zarr-map/public
cp build/demo-wasm/zarr.duckdb_extension.wasm demos/zarr-map/public/
```

Alternatively, build with `make wasm_eh` using emsdk 6.0.9, then copy
`build/wasm_eh/extension/zarr/zarr.duckdb_extension.wasm` into the same directory.
CI artifacts expire; a fresh successful CI run or a local build is needed then.

Prepare the small local sample and start the demo:

```sh
make generate_fixtures
cd demos/zarr-map
npm ci --ignore-scripts
uv run scripts/make_sample.py
npm run dev
```

Open the local URL printed by Vite. The sample writer takes the first 40 time
steps from the real air-temperature fixture and writes a consolidated v2 store.
Generated data, Wasm binaries, build output and test reports stay ignored by Git.

The default query reads one hour of ARCO-ERA5 through the dev server's `/gcs`
proxy. It uses `bounds :=` to prune time chunks before fetching them; the SQL
`WHERE` clause only thins the spatial grid. A time predicate in `WHERE` alone
is not automatic chunk pruning. The selected timestamp is July 1, 2021: this
store ends on December 31, 2021, despite `1959-2022` in its URL.

## Local sample query

Set the store URL to `http://localhost:5173/data/air.zarr` (adjust the host and
port to match Vite), then use:

```sql
SELECT lon, lat, air AS value
FROM read_zarr('{url}', dims := ['time','lat','lon'])
WHERE time = TIMESTAMP '2013-01-01';
```

This returns 1,325 points. The query box requires `lon`, `lat` and `value`
columns. `{url}` is replaced with the store URL field, with SQL quotes escaped.
The extension is unsigned, so the database enables `allowUnsignedExtensions`.

## Browser checks

After the setup above:

```sh
npx playwright install chromium
npm test                 # local data, actual rendering, and error cases
npm run test:remote      # real ARCO-ERA5 data; needs network access
npm run build
# Check the built site too (POSIX shell):
DEMO_PREVIEW=1 npm test
DEMO_PREVIEW=1 npm run test:remote
```

The remote check asserts that the query returns 16,380 points, requests exactly
one temperature chunk, and draws colored pixels on the WebGL canvas. It is kept
separate from the local suite so external service availability does not affect
ordinary UI checks.

## Serving notes

- Remote stores need consolidated metadata because HTTP stores cannot be listed.
- ARCO-ERA5's bucket has no CORS headers. Vite's dev and preview servers proxy
  `/gcs` to Google Cloud Storage. A separately hosted static `dist/` directory
  needs an equivalent proxy or a dataset that allows CORS; the proxy is not
  bundled into the JavaScript.
- Missing metadata files must return HTTP 404, not an HTML page. Vite uses
  `appType: "mpa"` because `read_zarr` probes `zarr.json` before v2 metadata.
- The map wraps 0–360 longitudes. Run is disabled until initialization succeeds
  and while a query is in progress. Empty results and missing columns give
  explicit messages rather than an invalid color range or a JavaScript error.
- Vite may warn that DuckDB's worker source map is absent from the published
  package. The worker still loads; that warning does not prevent the checks.
