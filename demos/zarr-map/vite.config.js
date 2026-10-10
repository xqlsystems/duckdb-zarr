import { defineConfig } from "vite";

// duckdb-wasm ships its own worker/wasm bundles; keep them out of dependency
// pre-bundling so the `?url` imports in src/main.js resolve to real files.
export default defineConfig({
  optimizeDeps: { exclude: ["@duckdb/duckdb-wasm"] },
});
