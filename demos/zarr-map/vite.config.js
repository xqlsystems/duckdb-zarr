import { defineConfig } from "vite";

// duckdb-wasm ships its own worker/wasm bundles; keep them out of dependency
// pre-bundling so the `?url` imports in src/main.js resolve to real files.
export default defineConfig({
  // No SPA fallback: a missing file must be a 404, not index.html. read_zarr
  // probes for zarr.json first and chokes on an HTML 200.
  appType: "mpa",
  server: {
    // gs://gcp-public-data-arco-era5 sends no CORS headers, so the browser can't
    // read it directly. Proxy it same-origin; ranges and 404s pass through.
    proxy: {
      "/gcs": {
        target: "https://storage.googleapis.com",
        changeOrigin: true,
        rewrite: (path) => path.replace(/^\/gcs/, ""),
      },
    },
  },
  optimizeDeps: { exclude: ["@duckdb/duckdb-wasm"] },
});
