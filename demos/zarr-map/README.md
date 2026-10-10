# zarr-map demo

Query a Zarr store with `read_zarr` inside DuckDB-Wasm and draw the result on a
deck.gl map. There is no JS Zarr reader: the extension fetches chunks through
duckdb-wasm's HTTP filesystem.

Tested in headless Chromium against duckdb-wasm `1.33.1-dev57.0` (reports
DuckDB `v1.5.4`) with the `wasm_eh` build of the extension, which loads even
though it was built for DuckDB `v1.5.5`. Not yet tried in Safari.

## Run

1. Put a `wasm_eh` build of the extension in `public/` (download it from the
   `zarr-v1.5.5-extension-wasm_eh` artifact of a CI run, or `make wasm_eh` from
   the repo root with emsdk 3.1.71 active):

   ```sh
   cp build/wasm_eh/extension/zarr/zarr.duckdb_extension.wasm demos/zarr-map/public/
   ```

2. Write the small sample store (needs `make generate_fixtures` first, and `uv`):

   ```sh
   uv run scripts/make_sample.py
   ```

3. Start the page:

   ```sh
   npm install
   npm run dev          # add -- --host to open it from a phone on your LAN
   ```

The query box takes any SQL that returns `lon`, `lat` and `value` columns. The
URL placeholder `{url}` is replaced with the store URL field. The extension is
unsigned, so the page opens the database with `allowUnsignedExtensions: true`.

## Notes

- Use the `eh` flavour. The `mvp` bundle fails with `_setThrew is not defined`
  as soon as the extension raises a C++ exception.
- Remote stores must allow CORS and expose consolidated metadata
  (`.zmetadata` or a consolidated `zarr.json`), since HTTP stores can't be listed.
- The dev server must answer 404 for missing files (`appType: "mpa"` in
  `vite.config.js`). `read_zarr` probes `zarr.json` first and fails on Vite's
  default `index.html` fallback.
- Layers use `wrapLongitude` because many datasets use 0–360 longitudes.
