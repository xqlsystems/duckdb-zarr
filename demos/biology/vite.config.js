import { defineConfig } from 'vite';
import { existsSync } from 'node:fs';

export default defineConfig({
  base: './',
  plugins: [{
    name: 'remote-datasets-only',
    buildStart() {
      if (existsSync(new URL('./public/data', import.meta.url))) throw new Error('Remove public/data: this demo loads original remote datasets and must not bundle prepared copies.');
    },
  }],
  // Zarr probes must receive real 404s, never the SPA's index.html.
  appType: 'mpa',
  optimizeDeps: { exclude: ['@duckdb/duckdb-wasm'] },
});
