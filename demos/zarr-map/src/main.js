import * as duckdb from "@duckdb/duckdb-wasm";
import duckdbWasm from "@duckdb/duckdb-wasm/dist/duckdb-mvp.wasm?url";
import duckdbWorker from "@duckdb/duckdb-wasm/dist/duckdb-browser-mvp.worker.js?url";
import { Deck, MapView } from "@deck.gl/core";
import { ScatterplotLayer } from "@deck.gl/layers";

// Public GPCP precipitation store (time, latitude, longitude).
const DEFAULT_URL =
  "https://ncsa.osn.xsede.org/Pangeo/pangeo-forge/gpcp-feedstock/gpcp.zarr";
const EXTENSION_URL = new URL("zarr.duckdb_extension.wasm", location.href).href;

const $ = (id) => document.getElementById(id);
const setStatus = (msg, isError = false) => {
  $("status").textContent = msg;
  $("status").className = isError ? "error" : "";
};

$("url").value = new URLSearchParams(location.search).get("url") ?? DEFAULT_URL;
$("sql").value = `-- One day of GPCP precipitation; lon/lat are coordinate columns.
SELECT longitude AS lon, latitude AS lat, precip AS value
FROM read_zarr('{url}', dims=['time','latitude','longitude'])
WHERE time >= TIMESTAMP '2020-01-01' AND time < TIMESTAMP '2020-01-02'
  AND precip IS NOT NULL`;

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
