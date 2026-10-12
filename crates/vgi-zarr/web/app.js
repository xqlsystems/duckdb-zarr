// Zarr SQL in the browser: Haybarn-wasm (DuckDB) + the vgi-zarr worker, both in
// this page. The worker fetches chunks straight from the bucket (the stores
// below send CORS headers), so the only server is whatever hosts these files.
import * as duckdb from "./haybarn/duckdb-browser.mjs";
import { installVgiWebWorkerBridge } from "./haybarn/duckdb-browser-vgi.mjs";

const here = (p) => new URL(p, location.href).href;

// dynamical.org's public Zarr v3 stores (sharded; CORS: *). `dims` names the
// main dimension group; for a custom URL it is read from the store's metadata.
// data.dynamical.org is being retired in favour of Icechunk repositories
// (https://dynamical.org/migration-2026/); these URLs answer until then.
const DD = "https://data.dynamical.org";
const DATASETS = [
  { id: "noaa-gfs-forecast", title: "NOAA GFS forecast", url: `${DD}/noaa/gfs/forecast/latest.zarr`, dims: "init_time,lead_time,latitude,longitude" },
  { id: "noaa-gfs-analysis", title: "NOAA GFS analysis", url: `${DD}/noaa/gfs/analysis/latest.zarr`, dims: "time,latitude,longitude" },
  { id: "noaa-gefs-forecast-35-day", title: "NOAA GEFS forecast, 35 day", url: `${DD}/noaa/gefs/forecast-35-day/latest.zarr`, dims: "init_time,ensemble_member,lead_time,latitude,longitude" },
  { id: "noaa-gefs-analysis", title: "NOAA GEFS analysis", url: `${DD}/noaa/gefs/analysis/latest.zarr`, dims: "time,latitude,longitude" },
  { id: "noaa-hrrr-forecast-48-hour", title: "NOAA HRRR forecast, 48 hour", url: `${DD}/noaa/hrrr/forecast-48-hour/latest.zarr`, dims: "init_time,lead_time,y,x" },
  { id: "noaa-hrrr-analysis", title: "NOAA HRRR analysis", url: `${DD}/noaa/hrrr/analysis/latest.zarr`, dims: "time,y,x" },
  { id: "noaa-mrms-conus-analysis-hourly", title: "NOAA MRMS CONUS analysis, hourly", url: `${DD}/noaa/mrms/conus-analysis-hourly/latest.zarr`, dims: "time,latitude,longitude" },
  { id: "ecmwf-ifs-ens-forecast-15-day-0-25-degree", title: "ECMWF IFS ENS forecast, 15 day", url: `${DD}/ecmwf/ifs-ens/forecast-15-day-0-25-degree/latest.zarr`, dims: "init_time,lead_time,ensemble_member,latitude,longitude" },
  { id: "ecmwf-aifs-single-forecast", title: "ECMWF AIFS Single forecast", url: `${DD}/ecmwf/aifs-single/forecast/latest.zarr`, dims: "init_time,lead_time,latitude,longitude" },
  { id: "dwd-icon-eu-forecast-5-day", title: "DWD ICON-EU forecast, 5 day", url: `${DD}/dwd/icon-eu/forecast-5-day/latest.zarr`, dims: "init_time,lead_time,latitude,longitude" },
];

// Places for point presets; the first inside a dataset's extent wins.
const PLACES = [
  { name: "New York", lat: 40.75, lon: -74.0 },
  { name: "Berlin", lat: 52.5, lon: 13.4 },
  { name: "Denver", lat: 39.75, lon: -105.0 },
  { name: "Nairobi", lat: -1.3, lon: 36.8 },
];
const PREFERRED_VARS = ["temperature_2m", "precipitation_surface", "2m_temperature", "t2m"];
const DAY_US = 86_400e6;
// Map presets stay under this many cells: every row becomes JS work for the heat map.
const MAP_CELLS = 120_000;
// …and read at most about this many decoded bytes (network + wasm heap).
const MAP_BYTES = 128e6;

const $ = (id) => document.getElementById(id);
const status = (msg, err = false) => {
  $("status").textContent = msg;
  $("status").className = err ? "err" : "";
};
let conn;
let current; // { ds, view, dims, vars, presets }
// Pruning stats of the last scan. The worker posts them on a BroadcastChannel
// when a scan finishes (report_scan in src/read_zarr.rs) rather than the page
// calling zarr.last_scan(): an RPC right after a large result stream can fail
// with "response stream EOF" or hang the connection in Haybarn-wasm 1.5.5-rc8.
let lastStats;
let statsWaiters = [];
new BroadcastChannel("vgi-zarr").onmessage = (e) => {
  lastStats = JSON.parse(e.data);
  statsWaiters.splice(0).forEach((wake) => wake());
};

$("run").onclick = () => run();
$("sql").addEventListener("keydown", (e) => {
  if ((e.metaKey || e.ctrlKey) && e.key === "Enter") run();
});
for (const d of DATASETS) $("dataset").append(new Option(d.title, d.id));
$("dataset").append(new Option("Other Zarr URL…", "custom"));
$("dataset").onchange = () => {
  const custom = $("dataset").value === "custom";
  $("custom").style.display = custom ? "flex" : "none";
  if (!custom) choose(DATASETS.find((d) => d.id === $("dataset").value));
};
$("open-custom").onclick = () => {
  const url = $("custom-url").value.trim().replace(/\/+$/, "");
  if (url) choose({ id: "custom", title: url.split("/").slice(-2).join("/"), url });
};

// ---------------------------------------------------------------------------
// Boot
// ---------------------------------------------------------------------------

async function step(label, fn) {
  status(`${label}…`);
  try {
    return await fn();
  } catch (e) {
    throw new Error(`${label}: ${e.message || e}`);
  }
}

async function boot() {
  if (!crossOriginIsolated) {
    status("This page is not cross-origin isolated, so SharedArrayBuffer is unavailable. Serve it with COOP/COEP headers over HTTPS or localhost (see serve.py).", true);
    return false;
  }
  const t0 = performance.now();
  await step("Starting DuckDB", async () => {
    const worker = new Worker(here("haybarn/duckdb-browser-coi.worker.js"));
    installVgiWebWorkerBridge()(worker);
    const db = new duckdb.AsyncDuckDB(new duckdb.VoidLogger(), worker);
    await db.instantiate(here("haybarn/duckdb-coi.wasm"), here("haybarn/duckdb-browser-coi.pthread.worker.js"));
    conn = await db.connect();
  });
  await step("Loading the vgi extension", async () => {
    await conn.query("INSTALL vgi FROM community");
    await conn.query("LOAD vgi");
  });
  await step("Starting the zarr worker", () =>
    conn.query(`ATTACH 'zarr' (TYPE vgi, LOCATION 'worker:${here("vgi-zarr/vgi-worker-boot.js")}')`),
  );
  console.log(`AUTORUN boot ${(performance.now() - t0).toFixed(0)} ms`);
  window.conn = conn;
  return true;
}

// ---------------------------------------------------------------------------
// Datasets
// ---------------------------------------------------------------------------

const sqlStr = (s) => `'${String(s).replaceAll("'", "''")}'`;
const ident = (s) => (/^[a-z_][a-z0-9_]*$/.test(s) ? s : `"${s.replaceAll('"', '""')}"`);
// noaa-hrrr-forecast-48-hour → hrrr_forecast
const viewName = (ds) =>
  ds.id === "custom" ? "zarr_data" : ds.id.replace(/^[a-z]+-/, "").replace(/(forecast|analysis).*$/, "$1").replaceAll("-", "_");

/** Main dimension group from consolidated metadata (Zarr v3 zarr.json or v2 .zmetadata). */
async function detectDims(url) {
  const groups = new Map();
  const add = (dims) => dims && dims.length >= 2 && groups.set(dims.join(","), (groups.get(dims.join(",")) || 0) + 1);
  const v3 = await fetch(`${url}/zarr.json`).then((r) => (r.ok ? r.json() : null)).catch(() => null);
  if (v3?.consolidated_metadata?.metadata) {
    for (const m of Object.values(v3.consolidated_metadata.metadata)) if (m.node_type === "array") add(m.dimension_names);
  } else {
    const v2 = await fetch(`${url}/.zmetadata`).then((r) => (r.ok ? r.json() : null)).catch(() => null);
    for (const [k, m] of Object.entries(v2?.metadata || {})) if (k.endsWith("/.zattrs")) add(m._ARRAY_DIMENSIONS);
  }
  if (!groups.size) return undefined;
  return [...groups].sort((a, b) => b[1] - a[1] || b[0].split(",").length - a[0].split(",").length)[0][0];
}

async function choose(ds) {
  if (!conn) return;
  $("run").disabled = true;
  try {
    const dims = ds.dims ?? (await step("Reading store metadata", () => detectDims(ds.url)));
    const args = `${sqlStr(ds.url)}${dims ? `, dims := ${sqlStr(dims)}` : ""}`;
    const view = viewName(ds);
    const t0 = performance.now();
    await step(`Opening ${ds.title}`, () => conn.query(`CREATE OR REPLACE VIEW ${view} AS SELECT * FROM zarr.read_zarr(${args})`));
    const dimRows = (await conn.query(`SELECT * FROM zarr.describe_dims(${args})`)).toArray().map((r) => ({ ...r.toJSON() }));
    const cols = (await conn.query(`DESCRIBE ${view}`)).toArray().map((r) => r.column_name);
    const dimNames = new Set(dimRows.map((d) => d.dim));
    const vars = cols.filter((c) => !dimNames.has(c));
    current = { ds, view, args, dims: dimRows, vars };
    current.presets = makePresets(current);
    showDataset(current, performance.now() - t0);
    $("sql").value = current.presets[0].sql;
    status(`Opened ${ds.title}. Pick a query or write your own.`);
    history.replaceState(null, "", `?ds=${encodeURIComponent(ds.id === "custom" ? ds.url : ds.id)}`);
  } catch (e) {
    status(String(e.message || e), true);
  } finally {
    $("run").disabled = false;
  }
}

function showDataset(c, ms) {
  const fmtDim = (d) => {
    const range = d.kind === "time" ? `${isoMinute(d.first)} → ${isoMinute(d.last)}` : `${num(d.first)} → ${num(d.last)}`;
    return `<tr><td>${d.dim}</td><td>${Number(d.length).toLocaleString()}</td><td>${Number(d.read_unit).toLocaleString()}</td><td>${range}</td></tr>`;
  };
  $("schema").innerHTML =
    `<p>View <code>${c.view}</code> = <code>zarr.read_zarr(${escapeHtml(c.args)})</code> · opened in ${(ms / 1000).toFixed(1)} s</p>` +
    `<table><tr><th>dimension</th><th>length</th><th>read unit</th><th>range</th></tr>${c.dims.map(fmtDim).join("")}</table>` +
    `<p class="vars">${c.vars.map((v) => `<code>${v}</code>`).join(" ")}</p>`;
  $("presets").innerHTML = "";
  for (const p of c.presets) {
    const b = document.createElement("button");
    b.innerHTML = `${p.title}<small>${p.sub}</small>`;
    b.onclick = () => {
      $("sql").value = p.sql;
      run();
    };
    $("presets").append(b);
  }
}

// ---------------------------------------------------------------------------
// Presets that fit the store's layout
//
// Every query pins or bounds every dimension so the worker reads a handful of
// read units (chunks, or subchunks of sharded arrays). Equality on computed
// coordinates is avoided: each pin is a half-step window around a grid value,
// so float rounding and slightly irregular axes cannot empty the result.
// ---------------------------------------------------------------------------

function makePresets(c) {
  const dims = c.dims.map((d) => ({
    ...d,
    first: Number(d.first),
    last: Number(d.last),
    length: Number(d.length),
    read_unit: Number(d.read_unit),
    // Exact coordinates for short axes (irregular lead times, members); long
    // axes are treated as regular between first and last.
    values: d.values ? Array.from(d.values.toArray(), Number) : null,
  }));
  const n = dims.length;
  const Y = dims[n - 2], X = dims[n - 1];
  const lead = dims.slice(0, n - 2);
  const v = PREFERRED_VARS.find((p) => c.vars.includes(p)) ?? c.vars[0];
  const clamp = (d, i) => Math.max(0, Math.min(d.length - 1, i));
  const stepOf = (d) => (d.length > 1 ? (d.last - d.first) / (d.length - 1) : 1);
  const at = (d, i) => (d.values ? d.values[clamp(d, i)] : d.first + stepOf(d) * clamp(d, i));
  const idxOf = (d, val) => {
    if (!d.values) return clamp(d, Math.round((val - d.first) / stepOf(d)));
    let best = 0;
    d.values.forEach((x, i) => Math.abs(x - val) < Math.abs(d.values[best] - val) && (best = i));
    return best;
  };
  // Half the gap to the nearest neighbour: a window that holds exactly one value.
  const half = (d, i) => {
    if (d.length < 2) return 0.5;
    const gaps = [i - 1, i + 1].filter((j) => j >= 0 && j < d.length).map((j) => Math.abs(at(d, j) - at(d, i)));
    return Math.min(...gaps) / 2;
  };
  const lit = (d, val) => (d.kind === "time" ? `TIMESTAMP '${isoSecond(val)}'` : num(val));
  const near = (d, i) => {
    const h = half(d, clamp(d, i)), val = at(d, i);
    return `${ident(d.dim)} BETWEEN ${lit(d, val - h)} AND ${lit(d, val + h)}`;
  };
  const range = (d, i0, i1) => {
    const [lo, hi] = [clamp(d, Math.min(i0, i1)), clamp(d, Math.max(i0, i1))];
    const ends = [at(d, lo) - Math.sign(stepOf(d)) * half(d, lo), at(d, hi) + Math.sign(stepOf(d)) * half(d, hi)];
    return `${ident(d.dim)} BETWEEN ${lit(d, Math.min(...ends))} AND ${lit(d, Math.max(...ends))}`;
  };
  const isLead = (d) => /lead|step|forecast_period/.test(d.dim);
  const isMember = (d) => /ensemble|member|realization|number/.test(d.dim);
  // Two days back on time axes (recent data that has finished ingesting).
  const recent = (d) => {
    if (d.kind !== "time") return isLead(d) ? 1 : 0;
    if (d.values) return idxOf(d, d.last - 2 * DAY_US);
    return d.length - 1 - Math.ceil((2 * DAY_US) / Math.abs(stepOf(d) || DAY_US));
  };

  // A place inside the extent, else the grid centre (projected x/y grids).
  const geo = /lat/.test(Y.dim) && /lon/.test(X.dim);
  const inside = (d, val) => val >= Math.min(d.first, d.last) && val <= Math.max(d.first, d.last);
  const place = geo && PLACES.find((p) => inside(Y, p.lat) && inside(X, p.lon));
  const py = place ? idxOf(Y, place.lat) : Math.floor(Y.length / 2);
  const px = place ? idxOf(X, place.lon) : Math.floor(X.length / 2);
  const where = place ? place.name : "the grid centre";

  const pins = lead.map((d) => near(d, recent(d)));
  const pinNote = lead.map((d) => `${d.dim} ${d.kind === "time" ? isoMinute(at(d, recent(d))) : num(at(d, recent(d)))}`).join(", ");
  const presets = [];

  // 1. Map around the place, bounded twice: ≤ MAP_CELLS result cells, and a
  // grid of read units whose decoded size (a unit spans every lead dimension's
  // read unit too, e.g. 51 members × 85 lead times) stays under MAP_BYTES.
  const unitBytes = dims.reduce((p, d) => p * d.read_unit, 4);
  const maxUnits = Math.max(1, Math.min(12, Math.floor(MAP_BYTES / unitBytes)));
  const uy = Math.max(1, Math.floor(Math.sqrt(maxUnits / 1.5))), ux = Math.max(1, Math.floor(maxUnits / uy));
  const aspect = geo ? 1.5 : 1;
  const cy = Math.floor(Math.sqrt(MAP_CELLS / aspect)), cx = Math.floor(cy * aspect);
  const hy = Math.max(2, Math.floor(Math.min(cy, uy * Y.read_unit) / 2));
  const hx = Math.max(2, Math.floor(Math.min(cx, ux * X.read_unit) / 2));
  presets.push({
    title: `Map around ${where}`,
    sub: `${v}, ${pinNote || "one slice"}`,
    sql: `SELECT ${ident(Y.dim)}, ${ident(X.dim)}, ${ident(v)}
FROM ${c.view}
WHERE ${[...pins, range(Y, py - hy, py + hy), range(X, px - hx, px + hx)].join("\n  AND ")}`,
  });

  // 2. Series at the place along the axis with the longest read unit.
  const series = [...lead].sort((a, b) => b.read_unit - a.read_unit)[0];
  if (series && series.length > 1) {
    const i1 = series.kind === "time" ? recent(series) : series.length - 1;
    const i0 = Math.max(0, i1 - (series.kind === "time" ? Math.min(series.read_unit, 24 * 30 * 2) : series.length) + 1);
    const sx = isLead(series) ? `${ident(series.dim)} / 3600 AS lead_hours` : ident(series.dim);
    const others = lead.filter((d) => d !== series);
    const member = others.find(isMember);
    const otherPins = others.filter((d) => d !== member).map((d) => near(d, recent(d)));
    const filters = [...otherPins, range(series, i0, i1), near(Y, py), near(X, px)].join("\n  AND ");
    presets.push({
      title: `${series.kind === "time" ? "Time series" : "Forecast"} at ${where}`,
      sub: `${v} along ${series.dim}${member ? ", ensemble mean" : ""}`,
      sql: member
        ? `SELECT ${sx}, avg(${ident(v)}) AS mean, min(${ident(v)}) AS lo, max(${ident(v)}) AS hi
FROM ${c.view}
WHERE ${filters}
GROUP BY ALL ORDER BY 1`
        : `SELECT ${sx}, ${ident(v)}
FROM ${c.view}
WHERE ${filters}
ORDER BY 1`,
    });

    // 3. Area statistics along the same axis over a small box.
    const b = 4;
    presets.push({
      title: `Area statistics near ${where}`,
      sub: `min / mean / max of ${v} over ${2 * b + 1}×${2 * b + 1} cells`,
      sql: `SELECT ${sx}, min(${ident(v)}) AS lo, avg(${ident(v)}) AS mean, max(${ident(v)}) AS hi
FROM ${c.view}
WHERE ${[...others.map((d) => near(d, recent(d))), range(series, i0, i1), range(Y, py - b, py + b), range(X, px - b, px + b)].join("\n  AND ")}
GROUP BY ALL ORDER BY 1`,
    });
  }

  presets.push({
    title: "Dimensions and read units",
    sub: "zarr.describe_dims(): how the store is laid out",
    sql: `SELECT dim, length, read_unit, kind,
       CASE WHEN kind = 'time' THEN make_timestamp(first::BIGINT)::VARCHAR ELSE first::VARCHAR END AS first,
       CASE WHEN kind = 'time' THEN make_timestamp(last::BIGINT)::VARCHAR ELSE last::VARCHAR END AS last
FROM zarr.describe_dims(${c.args})`,
  });
  return presets;
}

// ---------------------------------------------------------------------------
// Run + render
// ---------------------------------------------------------------------------

async function run() {
  if (!conn) return;
  lastStats = undefined;
  $("run").disabled = true;
  status("Running…");
  const t0 = performance.now();
  try {
    const table = await conn.query($("sql").value);
    const ms = performance.now() - t0;
    $("t-time").textContent = ms < 1000 ? `${ms.toFixed(0)} ms` : `${(ms / 1000).toFixed(1)} s`;
    $("t-rows").textContent = table.numRows.toLocaleString();
    await showStats();
    render(table);
    status("Done.");
    return { table, ms };
  } catch (e) {
    status(String(e.message || e), true);
  } finally {
    $("run").disabled = false;
  }
}

async function showStats() {
  // The message can trail the query result slightly; queries that scan nothing send none.
  if (!lastStats) await new Promise((wake) => (statsWaiters.push(wake), setTimeout(wake, 300)));
  const s = lastStats;
  $("t-chunks").textContent = s ? Number(s.chunks_read).toLocaleString() : "–";
  $("t-total").textContent = s ? Number(s.chunks_total).toLocaleString() : "–";
  $("pushed").textContent = s ? `pushed into the worker: ${s.pushed_filters}` : "";
}

function render(table) {
  const fields = table.schema.fields.map((f) => f.name);
  const isTime = table.schema.fields.map((f) => String(f.type).startsWith("Timestamp"));
  const rows = table.slice(0, 500).toArray();
  const head = `<tr>${fields.map((f) => `<th>${escapeHtml(f)}</th>`).join("")}</tr>`;
  const body = rows
    .map((r) => `<tr>${fields.map((f, i) => `<td>${fmt(isTime[i] && r[f] != null ? new Date(Number(r[f])) : r[f])}</td>`).join("")}</tr>`)
    .join("");
  $("table").innerHTML = head + body;
  const n = current?.dims.length ?? 0;
  const [yName, xName] = n >= 2 ? [current.dims[n - 2].dim, current.dims[n - 1].dim] : ["latitude", "longitude"];
  if (fields.includes(yName) && fields.includes(xName) && fields.length >= 3) {
    hideChart();
    drawMap(table, yName, xName, fields.find((f) => f !== yName && f !== xName));
  } else {
    hideMap();
    drawChart(table, fields, isTime);
  }
}

function hideMap() {
  $("viz").style.display = "none";
  $("legend").style.display = "none";
}
function hideChart() {
  $("chart").style.display = "none";
}

// Heat map of (y, x, value), north up.
function drawMap(table, yName, xName, vname) {
  const canvas = $("viz");
  const ys = table.getChild(yName).toArray(), xs = table.getChild(xName).toArray();
  const val = table.getChild(vname).toArray();
  if (!ys.length) return hideMap();
  const uniq = (a) => [...new Set(a)].sort((p, q) => p - q);
  const Ys = uniq(ys), Xs = uniq(xs);
  const yi = new Map(Ys.map((v, i) => [v, i])), xi = new Map(Xs.map((v, i) => [v, i]));
  let min = Infinity, max = -Infinity;
  for (const v of val) if (Number.isFinite(v)) { min = Math.min(min, v); max = Math.max(max, v); }
  canvas.width = Xs.length;
  canvas.height = Ys.length;
  const ctx = canvas.getContext("2d");
  const img = ctx.createImageData(Xs.length, Ys.length);
  for (let k = 0; k < val.length; k++) {
    const x = xi.get(xs[k]), y = Ys.length - 1 - yi.get(ys[k]);
    const [r, g, b] = ramp((val[k] - min) / (max - min || 1));
    img.data.set([r, g, b, Number.isFinite(val[k]) ? 255 : 0], 4 * (y * Xs.length + x));
  }
  ctx.putImageData(img, 0, 0);
  canvas.style.aspectRatio = `${Xs.length} / ${Ys.length}`;
  canvas.style.display = "block";
  $("lmin").textContent = Number.isFinite(min) ? num(min) : "";
  $("lmax").textContent = Number.isFinite(max) ? `${num(max)} ${vname}` : `${vname}: all NULL`;
  $("lbar").style.background = `linear-gradient(90deg, ${[0, 0.25, 0.5, 0.75, 1].map((t) => `rgb(${ramp(t)})`).join(",")})`;
  $("legend").style.display = "flex";
}

// Line chart: first column on x, the other numeric columns as lines.
function drawChart(table, fields, isTime) {
  const canvas = $("chart");
  if (table.numRows < 2 || fields.length < 2) return hideChart();
  const xs = table.getChild(fields[0]).toArray();
  const xnum = Array.from(xs, (v) => Number(v));
  const series = fields.slice(1).map((f) => ({ f, v: Array.from(table.getChild(f).toArray(), Number) })).filter((s) => s.v.some(Number.isFinite));
  if (!series.length || !xnum.every(Number.isFinite)) return hideChart();
  const dpr = window.devicePixelRatio || 1;
  const W = canvas.clientWidth || 600, H = 220;
  canvas.width = W * dpr;
  canvas.height = H * dpr;
  const ctx = canvas.getContext("2d");
  ctx.scale(dpr, dpr);
  const css = getComputedStyle(document.documentElement);
  const ink = css.getPropertyValue("--muted").trim(), line = css.getPropertyValue("--line").trim();
  const colors = ["--accent", "--bad", "--good"].map((v) => css.getPropertyValue(v).trim());
  const pad = { l: 52, r: 10, t: 10, b: 24 };
  const x0 = Math.min(...xnum), x1 = Math.max(...xnum);
  let y0 = Infinity, y1 = -Infinity;
  for (const s of series) for (const v of s.v) if (Number.isFinite(v)) { y0 = Math.min(y0, v); y1 = Math.max(y1, v); }
  if (y0 === y1) { y0 -= 1; y1 += 1; }
  const sx = (v) => pad.l + ((v - x0) / (x1 - x0 || 1)) * (W - pad.l - pad.r);
  const sy = (v) => H - pad.b - ((v - y0) / (y1 - y0)) * (H - pad.t - pad.b);
  ctx.clearRect(0, 0, W, H);
  ctx.font = "11px system-ui";
  ctx.fillStyle = ink;
  ctx.strokeStyle = line;
  for (let i = 0; i <= 4; i++) {
    const v = y0 + ((y1 - y0) * i) / 4, y = sy(v);
    ctx.beginPath(); ctx.moveTo(pad.l, y); ctx.lineTo(W - pad.r, y); ctx.stroke();
    ctx.fillText(num(v), 4, y + 4);
  }
  const xl = (v) => (isTime[0] ? new Date(v).toISOString().slice(5, 16).replace("T", " ") : num(v));
  ctx.fillText(xl(x0), pad.l, H - 6);
  const r = xl(x1);
  ctx.fillText(r, W - pad.r - ctx.measureText(r).width, H - 6);
  series.forEach((s, j) => {
    ctx.strokeStyle = colors[j % colors.length];
    ctx.lineWidth = 1.5;
    ctx.beginPath();
    let pen = false;
    for (let i = 0; i < xnum.length; i++) {
      if (!Number.isFinite(s.v[i])) { pen = false; continue; }
      pen ? ctx.lineTo(sx(xnum[i]), sy(s.v[i])) : ctx.moveTo(sx(xnum[i]), sy(s.v[i]));
      pen = true;
    }
    ctx.stroke();
    ctx.fillStyle = colors[j % colors.length];
    ctx.fillText(s.f, W - pad.r - 8 - ctx.measureText(s.f).width, pad.t + 12 + 13 * j);
  });
  canvas.style.display = "block";
}

// Perceptual blue → yellow → red ramp.
function ramp(t) {
  t = Math.max(0, Math.min(1, t));
  const stops = [[49, 54, 149], [69, 117, 180], [171, 217, 233], [255, 255, 191], [253, 174, 97], [215, 48, 39], [165, 0, 38]];
  const s = t * (stops.length - 1), i = Math.min(Math.floor(s), stops.length - 2), f = s - i;
  return stops[i].map((c, j) => Math.round(c + f * (stops[i + 1][j] - c)));
}

function num(v) {
  if (!Number.isFinite(v)) return String(v);
  const a = Math.abs(v);
  return a !== 0 && (a < 1e-3 || a >= 1e7) ? v.toExponential(3) : String(Math.round(v * 1e6) / 1e6);
}
const isoSecond = (us) => new Date(us / 1000).toISOString().slice(0, 19).replace("T", " ");
const isoMinute = (us) => isoSecond(us).slice(0, 16);
const escapeHtml = (s) => String(s).replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[c]);

function fmt(v) {
  if (v === null || v === undefined) return "NULL";
  if (typeof v === "number") return Number.isInteger(v) ? String(v) : num(v);
  if (typeof v === "bigint") return v.toString();
  if (v instanceof Date) return v.toISOString().replace("T", " ").replace(".000Z", "");
  return escapeHtml(v);
}

// ---------------------------------------------------------------------------
// ?autorun: run every preset (or with ?autorun=all, every dataset's presets)
// and log timings + pruning stats. Used as a headless smoke test.
// ---------------------------------------------------------------------------

async function autorun(all) {
  const list = all ? DATASETS : [current.ds];
  for (const ds of list) {
    if (all) await choose(ds);
    for (const p of current.presets) {
      $("sql").value = p.sql;
      const r = await run();
      const s = lastStats;
      console.log(`AUTORUN ${ds.id} | ${p.title}: ${r ? `${r.table.numRows} rows in ${r.ms.toFixed(0)} ms, units ${s?.chunks_read}/${s?.chunks_total}` : `FAILED ${$("status").textContent}\n${p.sql}`}`);
    }
  }
  console.log("AUTORUN done");
}

(async () => {
  try {
    if (!(await boot())) return;
    const q = new URLSearchParams(location.search);
    const want = q.get("ds");
    const ds = DATASETS.find((d) => d.id === want) ?? (want && /^https?:/.test(want) ? { id: "custom", title: want, url: want } : DATASETS[0]);
    $("dataset").value = ds.id;
    if (ds.id === "custom") {
      $("custom").style.display = "flex";
      $("custom-url").value = ds.url;
    }
    await choose(ds);
    if (q.has("autorun")) await autorun(q.get("autorun") === "all");
    // ?sql=…(&sql=…): run raw statements and log results (debugging aid).
    for (const sql of q.getAll("sql")) {
      const t0 = performance.now();
      try {
        const t = await conn.query(sql);
        console.log(`AUTORUN sql ok ${t.numRows} rows in ${(performance.now() - t0).toFixed(0)} ms: ${JSON.stringify(t.slice(0, 3).toArray().map((r) => r.toJSON()), (k, v) => (typeof v === "bigint" ? String(v) : v))}`);
      } catch (e) {
        console.log(`AUTORUN sql FAILED ${e.message || e}`);
      }
    }
    if (q.has("sql")) console.log("AUTORUN done");
  } catch (e) {
    console.log(`AUTORUN failed: ${e.message || e}`);
    status(`Startup failed. ${e.message || e}`, true);
  }
})();
