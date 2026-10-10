# zarr-map demo

Query a remote Zarr store with `read_zarr` inside DuckDB-Wasm and draw the
result on a deck.gl map. There is no JS Zarr reader: the extension fetches
chunks through duckdb-wasm's HTTP filesystem.

Status: **scaffold, not yet verified in a browser.** It needs the wasm
extension from the wasm CI work (#55).

## Run

1. Build or download a `wasm_mvp` extension and put it in `public/`:

   ```sh
   # from the repo root, with emsdk 3.1.71 active
   make wasm_mvp
   cp build/wasm_mvp/extension/zarr/zarr.duckdb_extension.wasm demos/zarr-map/public/
   ```

2. Start the page:

   ```sh
   cd demos/zarr-map
   npm install
   npm run dev
   ```

The extension must be built for the same DuckDB version as the
`@duckdb/duckdb-wasm` release in `package.json` (this repo targets DuckDB
`v1.5.5`; the page prints the loaded version). The extension is unsigned, so
the page opens the database with `allowUnsignedExtensions: true`.

The store must allow cross-origin reads (CORS). Edit the URL or the SQL in the
sidebar; the query has to return `lon`, `lat` and `value` columns.
