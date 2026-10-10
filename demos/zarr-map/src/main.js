import * as duckdb from "@duckdb/duckdb-wasm";
import duckdbWasm from "@duckdb/duckdb-wasm/dist/duckdb-eh.wasm?url";
import duckdbWorker from "@duckdb/duckdb-wasm/dist/duckdb-browser-eh.worker.js?url";
import { Deck, MapView } from "@deck.gl/core";
import { ScatterplotLayer } from "@deck.gl/layers";

// ARCO-ERA5 (https://github.com/google-research/arco-era5), proxied through the
// dev server at /gcs because the bucket has no CORS headers (see vite.config.js).
// A tiny local sample is at data/air.zarr (scripts/make_sample.py).
const DEFAULT_URL = new URL(
  "gcs/gcp-public-data-arco-era5/ar/1959-2022-full_37-1h-0p25deg-chunk-1.zarr-v2",
  location.href,
).href;
const EXTENSION_URL = new URL("zarr.duckdb_extension.wasm", location.href).href;

const $ = (id) => document.getElementById(id);
const setStatus = (msg, isError = false) => {
  $("status").textContent = msg;
  $("status").className = isError ? "error" : "";
};

$("url").value = new URLSearchParams(location.search).get("url") ?? DEFAULT_URL;
$("sql").value = `-- ERA5 2m temperature (K) at one hour, thinned to a 2 degree grid.
-- lon/lat are coordinate columns; only the chunk for this hour is read.
SELECT longitude AS lon, latitude AS lat, "2m_temperature" AS value
FROM read_zarr('{url}', dims=['time','latitude','longitude'])
WHERE time = TIMESTAMP '2022-07-01 12:00:00'
  AND round(latitude * 4)::INT % 8 = 0
  AND round(longitude * 4)::INT % 8 = 0`;

const db = await (async () => {
  const worker = new Worker(duckdbWorker);
  const instance = new duckdb.AsyncDuckDB(new duckdb.ConsoleLogger(), worker);
  await instance.instantiate(duckdbWasm);
  // The locally built extension is unsigned.
  await instance.open({ allowUnsignedExtensions: true });
  return instance;
})();
const conn = await db.connect();

try {
  const [{ v }] = (await conn.query("SELECT version() AS v")).toArray();
  await conn.query(`LOAD '${EXTENSION_URL}'`);
  setStatus(`DuckDB ${v}; zarr extension loaded.`);
} catch (e) {
  setStatus(
    `Could not load the zarr extension from ${EXTENSION_URL}\n${e.message}\n\n` +
      "Copy a wasm_mvp build into public/ (see README) and check that the\n" +
      "extension was built for this duckdb-wasm's DuckDB version.",
    true,
  );
}

const deck = new Deck({
  parent: $("map"),
  views: new MapView({ repeat: true }),
  initialViewState: { longitude: 0, latitude: 20, zoom: 1 },
  controller: true,
  layers: [],
});

async function run() {
  const url = $("url").value.trim();
  const sql = $("sql").value.replaceAll("{url}", url.replaceAll("'", "''"));
  setStatus("Running…");
  const t0 = performance.now();
  try {
    const table = await conn.query(sql);
    const lon = table.getChild("lon").toArray();
    const lat = table.getChild("lat").toArray();
    const value = Float64Array.from(table.getChild("value").toArray(), Number);
    let min = Infinity, max = -Infinity;
    for (const v of value) { if (v < min) min = v; if (v > max) max = v; }
    const span = max - min || 1;
    deck.setProps({
      layers: [
        new ScatterplotLayer({
          id: "zarr",
          pickable: true,
          wrapLongitude: true, // datasets with 0-360 longitudes
          data: { length: table.numRows },
          getPosition: (_, { index }) => [lon[index], lat[index]],
          getFillColor: (_, { index }) => {
            const t = (value[index] - min) / span;
            return [255 * t, 80, 255 * (1 - t), 200];
          },
          radiusUnits: "pixels",
          getRadius: 3,
        }),
      ],
    });
    setStatus(
      `${table.numRows} rows in ${((performance.now() - t0) / 1000).toFixed(2)}s ` +
        `(value ${min.toFixed(2)}…${max.toFixed(2)})`,
    );
  } catch (e) {
    setStatus(e.message, true);
  }
}

$("run").addEventListener("click", run);
