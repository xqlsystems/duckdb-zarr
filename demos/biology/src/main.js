import * as duckdb from '@duckdb/duckdb-wasm';
import duckdbWasm from '@duckdb/duckdb-wasm/dist/duckdb-base.wasm?url';
import duckdbWorker from '@duckdb/duckdb-wasm/dist/duckdb-browser-base.worker.js?url';
import { createScatter } from './visualizations.js';
import { createVolume } from './vtk_volume.js';
import { scatterSQL, sqlString, volumeSQL } from './queries.js';
import { PBMC_SOURCE_URL, loadRemoteAnnData, remoteAnnDataViewSQL } from './remote_anndata.js';
import { OME_URL, loadOME } from './remote_ome.js';
import { prefetchOME } from './prefetch_ome.js';

const $ = (id) => document.getElementById(id);
const kinds = ['anndata', 'ome'];
const plots = {};
const sourceURLs = {};
const defaults = { anndata: PBMC_SOURCE_URL, ome: OME_URL };
const sourceHandles = {};
const sourceMetadata = {};
let conn, db, busy = false;
const pendingResets = {};
function cancelReset(kind) { clearTimeout(pendingResets[kind]); delete pendingResets[kind]; }
function scheduleReset(kind) {
  cancelReset(kind);
  pendingResets[kind] = setTimeout(() => {
    delete pendingResets[kind];
    if (busy) scheduleReset(kind);
    else reset(kind);
  }, 200);
}
const status = (id, text, error = false) => {
  $(id).textContent = text;
  $(id).classList.toggle('error', error);
};
function lock(locked) {
  for (const kind of kinds) {
    for (const control of document.querySelectorAll(`#${kind} button, #${kind} select, #${kind} input`)) {
      const sourceControl = [`${kind}-source`, `${kind}-load-source`, `${kind}-source-reset`].includes(control.id);
      control.disabled = locked || (!sourceHandles[kind] && !sourceControl);
    }
  }
}
function defaultSQL(kind, statistics = false) {
  if (kind === 'ome') {
    const cfg = sourceMetadata.ome;
    const zmax = Number($('ome-zmax').value);
    if (zmax >= cfg.shape[1]) throw new Error(`Last Z plane must be at most ${cfg.shape[1] - 1}.`);
    return volumeSQL(sourceURLs.ome, {
      channel: $('ome-channel').value, zmin: $('ome-zmin').value, zmax,
      level: cfg.level, statistics,
    });
  }
  if (statistics) return `SELECT cell_type AS annotation, COUNT(*) AS cells\nFROM pbmc_obs\nGROUP BY ALL ORDER BY cells DESC`;
  return scatterSQL(kind, { filter: $(`${kind}-filter`).value });
}
function resultTable(kind, table) {
  const host = $(`${kind}-results`);
  host.replaceChildren();
  if (!table.numRows) return;
  const el = document.createElement('table');
  const head = el.createTHead().insertRow();
  for (const field of table.schema.fields) {
    const th = document.createElement('th');
    th.textContent = field.name;
    head.append(th);
  }
  const body = el.createTBody();
  for (const row of table.slice(0, 12).toArray()) {
    const tr = body.insertRow();
    for (const field of table.schema.fields) {
      const td = tr.insertCell();
      const v = row[field.name];
      td.textContent = v == null ? 'NULL' : typeof v === 'number' && !Number.isInteger(v) ? v.toPrecision(6) : String(v);
    }
  }
  const caption = el.createCaption();
  caption.textContent = `${table.numRows.toLocaleString()} rows; showing the first ${Math.min(12, table.numRows)}.`;
  host.append(el);
}
async function run(kind) {
  if (!conn || busy) return;
  cancelReset(kind);
  busy = true;
  lock(true);
  status(`${kind}-status`, 'Running SQL in your browser…');
  const start = performance.now();
  try {
    const table = await conn.query($(`${kind}-sql`).value);
    resultTable(kind, table);
    const needed = kind === 'ome' ? ['c', 'x', 'y', 'z', 'value'] : ['x', 'y', 'label'];
    const hasCoordinates = needed.every((name) => table.getChild(name));
    if (!table.numRows || !hasCoordinates) plots[kind]?.clear();
    else {
      if (kind !== 'ome' && table.numRows > 2_000_000) throw new Error('Plot is limited to 2 million rows. Narrow the SQL query.');
      if (!plots[kind]) plots[kind] = kind === 'ome' ? createVolume($('ome-plot')) : createScatter($(`${kind}-plot`));
      let renderOpts = {};
      if (kind === 'ome') {
        renderOpts = {
          spacing: sourceMetadata.ome.spacing,
          channels: sourceMetadata.ome.channels.map((name, index) => ({ name, color: sourceMetadata.ome.channel_colors?.[index] ?? '#80dcd5' })),
        };
      }
      const rendered = await plots[kind].render(kind === 'ome' ? table : table.toArray().map((row) => Object.fromEntries(needed.concat(['cell_id', 'value']).map((name) => [name, row[name]]))), renderOpts);
      if (kind === 'ome' && !rendered) throw new Error($('ome-plot').querySelector('[role=status]')?.textContent ?? 'Volume could not be rendered.');
    }
    const elapsed = ((performance.now() - start) / 1000).toFixed(2);
    status(`${kind}-status`, `${table.numRows.toLocaleString()} rows · ${elapsed}s${!table.numRows ? ' · no data to display' : !hasCoordinates ? ' · table result (plot cleared)' : ' · plot updated'}`);
  } catch (error) {
    plots[kind]?.clear();
    $(`${kind}-results`).replaceChildren();
    status(`${kind}-status`, error.message ?? String(error), true);
  } finally {
    busy = false;
    lock(false);
  }
}
function fillOptions(id, values) {
  $(id).replaceChildren($(id).firstElementChild);
  for (const value of values) {
    const option = document.createElement('option');
    option.value = value;
    option.textContent = value;
    $(id).append(option);
  }
}
function reset(kind, stats = false) {
  cancelReset(kind);
  if (busy) { scheduleReset(kind); return; }
  try {
    $(`${kind}-sql`).value = defaultSQL(kind, stats);
    return run(kind);
  } catch (error) {
    status(`${kind}-status`, error.message, true);
  }
}
async function loadSource(kind) {
  if (!conn || busy) return;
  cancelReset(kind);
  const value = $(`${kind}-source`).value.trim();
  let url;
  try {
    url = new URL(value);
    if (!['http:', 'https:'].includes(url.protocol) || url.username || url.password) throw new Error();
  } catch { return status(`${kind}-status`, 'Enter a public HTTP(S) Zarr directory URL (or the pinned Scanpy ZIP).', true); }
  busy = true;
  lock(true);
  plots[kind]?.clear();
  $(`${kind}-results`).replaceChildren();
  let next;
  try {
    if (sourceHandles[kind]?.sourceURL !== url.href.replace(/\/$/, '')) {
      if (kind === 'anndata') {
        next = await loadRemoteAnnData(db, { url: url.href, onProgress: (message) => status('anndata-status', message) });
        await conn.query('BEGIN TRANSACTION');
        try {
          for (const sql of remoteAnnDataViewSQL(next.storeURL)) await conn.query(sql);
          await conn.query('COMMIT');
        } catch (error) { await conn.query('ROLLBACK'); throw error; }
        sourceURLs.anndata = next.storeURL;
        $('anndata-provenance').textContent = next.transport === 'remote-zip-memory'
          ? `Original Scanpy ZIP · ${(next.archiveBytes / 1024 / 1024).toFixed(2)} MB fetched remotely · integrity checked · browser RAM only`
          : 'Remote AnnData directory · queried directly; CORS and compatible obs/UMAP metadata required';
      } else {
        const metadata = await loadOME(url.href);
        next = await prefetchOME(db, metadata, { onProgress: (message) => status('ome-status', message) });
        sourceMetadata.ome = metadata;
        sourceURLs.ome = metadata.url;
        $('ome-channel').replaceChildren();
        const all = document.createElement('option'); all.value = 'all'; all.textContent = 'All channels together';
        $('ome-channel').append(all);
        metadata.channels.forEach((name, index) => {
          const option = document.createElement('option'); option.value = index; option.textContent = name;
          $('ome-channel').append(option);
        });
        for (const id of ['ome-zmin', 'ome-zmax']) $(id).max = metadata.shape[1] - 1;
        $('ome-zmin').value = 0;
        $('ome-zmax').value = metadata.shape[1] - 1;
        $('ome-dataset-label').textContent = `Original remote OME-Zarr · level ${metadata.level} · ${metadata.shape[3]}×${metadata.shape[2]}×${metadata.shape[1]} voxels per channel`;
        $('ome-provenance').textContent = `${next.objectCount} original remote objects · ${(next.bytes / 1024 / 1024).toFixed(1)} MB · browser RAM only`;
        $('ome-detail').textContent = 'Full-resolution original Blosc/LZ4 chunks are fetched from the source in parallel and decoded by duckdb-zarr. Both channels share the source physical grid. Display contrast suppresses background haze only; SQL retains original intensities. Sample: Blin et al. (2019), NesSys, PLoS Biology e3000388; IDR idr0062A/6001240, CC BY 4.0.';
      }
      const previous = sourceHandles[kind];
      sourceHandles[kind] = next;
      next = null;
      await previous?.dispose();
    }
    if (kind === 'anndata') {
      const types = await conn.query('SELECT DISTINCT cell_type AS label FROM pbmc_obs WHERE cell_type IS NOT NULL ORDER BY label');
      fillOptions('anndata-filter', types.toArray().map((row) => row.label));
    }
  } catch (error) {
    await next?.dispose();
    status(`${kind}-status`, `Could not load the remote source: ${error.message ?? error}. No local dataset fallback is used.`, true);
    return;
  } finally {
    busy = false;
    lock(false);
  }
  await reset(kind);
}
lock(true);
for (const kind of kinds) {
  $(`${kind}-run`).addEventListener('click', () => run(kind));
  $(`${kind}-reset`).addEventListener('click', () => reset(kind));
  if (kind === 'anndata') $('anndata-stats').addEventListener('click', () => reset(kind, true));
  $(`${kind}-sql`).addEventListener('keydown', (event) => {
    if ((event.ctrlKey || event.metaKey) && event.key === 'Enter') { event.preventDefault(); run(kind); }
  });
  $(`${kind}-load-source`).addEventListener('click', () => loadSource(kind));
  $(`${kind}-source`).addEventListener('keydown', (event) => {
    if (event.key === 'Enter') { event.preventDefault(); loadSource(kind); }
  });
  $(`${kind}-source-reset`).addEventListener('click', () => {
    $(`${kind}-source`).value = defaults[kind];
    loadSource(kind);
  });
  for (const control of document.querySelectorAll(`#${kind} select, #${kind} input:not([type=text])`)) control.addEventListener('change', () => scheduleReset(kind));
}
for (const kind of kinds) $(`${kind}-source`).value = defaults[kind];
try {
  const worker = new Worker(duckdbWorker);
  db = new duckdb.AsyncDuckDB(new duckdb.ConsoleLogger(), worker);
  await db.instantiate(duckdbWasm);
  await db.open({ allowUnsignedExtensions: true });
  conn = await db.connect();
  const [{ version }] = (await conn.query('SELECT version() AS version')).toArray();
  const extensionURL = new URL('zarr.duckdb_extension.wasm', location.href).href;
  try { await conn.query(`LOAD ${sqlString(extensionURL)}`); }
  catch (error) { throw new Error(`Could not load ${extensionURL}. Install the matching wasm_eh extension (README). ${error.message}`); }
  $('provenance').textContent = `Both datasets are fetched from their original remote sources into browser RAM. No dataset files, proxy or data-preparation server are shipped with this site. Scanpy ZIP bytes are SHA-256 checked; OME-Zarr chunks remain compressed and unchanged. Query runtime: ${version}; DuckDB-Wasm package 1.33.1-dev110.0 (preview).`;
  status('runtime', `DuckDB ${version} · zarr extension loaded · original remote sources, browser memory only`);
  for (const kind of kinds) await loadSource(kind);
} catch (error) {
  status('runtime', error.message ?? String(error), true);
  lock(true);
}
window.addEventListener('pagehide', () => { for (const kind of kinds) cancelReset(kind); for (const plot of Object.values(plots)) plot.dispose(); db?.terminate(); });
