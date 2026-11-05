// Run against Vite on a dedicated port (never the shared demo test port):
// node test/remote_anndata.browser.mjs http://127.0.0.1:4192 /absolute/scratch/evidence
import assert from 'node:assert/strict';
import { mkdir, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { chromium } from '@playwright/test';

const [baseURL, evidence] = process.argv.slice(2);
if (!baseURL || !evidence || !path.isAbsolute(evidence) || Number(new URL(baseURL).port) < 4192) {
  throw new Error('Supply a Vite URL on port 4192+ and an absolute scratch evidence directory.');
}
await mkdir(evidence, { recursive: true });
const browser = await chromium.launch({ headless: true });
const context = await browser.newContext();
const page = await context.newPage();
const requests = [], responses = [], errors = [];
page.on('request', (request) => requests.push(request.url()));
page.on('response', (response) => responses.push({ url: response.url(), status: response.status() }));
page.on('pageerror', (error) => errors.push(error.message));
// Only the probe HTML is supplied by the test. Data fetches are never mocked.
await page.route('**/__remote_anndata_test.html', (route) => route.fulfill({
  contentType: 'text/html', body: '<!doctype html><title>Live remote AnnData verification</title><pre id="report">Loading original Scanpy PBMC…</pre>',
}));
let timer;
try {
  await page.goto(new URL('/__remote_anndata_test.html', baseURL).href);
  const result = await Promise.race([
    page.evaluate(async () => {
      const remote = await import('/src/remote_anndata.js');
      const { scatterSQL } = await import('/src/queries.js');
      const duckdb = await import('/node_modules/@duckdb/duckdb-wasm/dist/duckdb-browser.mjs');
      const worker = new Worker('/node_modules/@duckdb/duckdb-wasm/dist/duckdb-browser-base.worker.js');
      const db = new duckdb.AsyncDuckDB(new duckdb.ConsoleLogger(), worker);
      const start = performance.now();
      let conn, handle;
      window.testStep = 'instantiate';
      const check = (condition, message) => { if (!condition) throw new Error(message); };
      const query = async (sql) => {
        const started = performance.now();
        let table;
        try { table = await conn.query(sql); }
        catch (error) { throw new Error(`Query failed: ${sql}\n${error.message || error}`, {cause:error}); }
        return { sql, ms: performance.now() - started, rows: table.toArray().map((row) => JSON.parse(JSON.stringify(row, (_, value) => typeof value === 'bigint' ? value.toString() : value))) };
      };
      try {
        await db.instantiate('/node_modules/@duckdb/duckdb-wasm/dist/duckdb-base.wasm');
        await db.open({ allowUnsignedExtensions: true });
        conn = await db.connect();
        const version = await query('SELECT version() AS version');
        await conn.query(`LOAD '${location.origin}/zarr.duckdb_extension.wasm'`);
        handle = await remote.loadRemoteAnnData(db);
        const queries = [];
        for (const sql of remote.remoteAnnDataViewSQL(handle.storeURL)) await conn.query(sql);
        queries.push(await query('DESCRIBE pbmc_obs'));
        queries.push(await query('DESCRIBE pbmc_coordinates'));
        const counts = await query('SELECT COUNT(*) AS cells, COUNT(DISTINCT obs) AS identifiers, COUNT(DISTINCT bulk_labels) AS annotations, COUNT(*) FILTER (WHERE bulk_labels IS DISTINCT FROM cell_type) AS alias_mismatches FROM pbmc_obs');
        check(JSON.stringify(counts.rows) === JSON.stringify([{cells:'700',identifiers:'700',annotations:'10',alias_mismatches:'0'}]), 'Original cell identifiers/annotations did not survive');
        queries.push(counts);
        queries.push(await query('SELECT cell_type, COUNT(*) AS cells FROM pbmc_obs GROUP BY ALL ORDER BY cells DESC'));
        const coords = await query('SELECT COUNT(*) AS coordinates, COUNT(DISTINCT obs) AS cells, MIN(X_umap_component) AS first_component, MAX(X_umap_component) AS last_component FROM pbmc_coordinates');
        check(JSON.stringify(coords.rows) === JSON.stringify([{coordinates:'1400',cells:'700',first_component:'0',last_component:'1'}]), 'UMAP shape mismatch');
        queries.push(coords);
        const scatter = await query(scatterSQL('anndata'));
        check(scatter.rows.length === 700 && scatter.rows.every((row) => Number.isFinite(row.x) && Number.isFinite(row.y)), 'Full scatter query returned missing positions');
        const filtered = await query(scatterSQL('anndata', {filter:'CD14+ Monocyte'}));
        check(filtered.rows.length === 129 && filtered.rows.every((row)=>row.label==='CD14+ Monocyte'), 'Filtered scatter mismatch');
        const empty = await query(scatterSQL('anndata', {filter:'not-an-annotation'}));
        check(empty.rows.length === 0, 'Empty-filter query returned invented rows');
        const expression = await query(`SELECT COUNT(*) AS measurements, MIN(X) AS minimum, MAX(X) AS maximum, SUM(X) AS sum FROM read_zarr('${handle.storeURL}', table_name := 'obs_var')`);
        window.testStep = 'expression complete';
        check(expression.rows[0].measurements === '535500', 'Expression matrix was incomplete');
        queries.push(expression);
        let integrityError;
        try { await remote.loadRemoteAnnData(db, {sha256:'0'.repeat(64)}); } catch (error) { integrityError=error.message; }
        check(integrityError?.includes('SHA-256 mismatch'), 'Integrity mismatch was not rejected');
        // Read back exactly a metadata object and its real compressed shard.
        const readback = [];
        for (const key of ['zarr.json','obsm/X_umap/c/0/0']) {
          window.testStep = `readback ${key}`;
          const data = await db.copyFileToBuffer(`${handle.storeURL}/${key}`);
          readback.push({key,bytes:data.byteLength,sha256:[...new Uint8Array(await crypto.subtle.digest('SHA-256',data))].map(v=>v.toString(16).padStart(2,'0')).join('')});
        }
        const report = { version: version.rows[0].version, handle:{...handle,dispose:undefined}, queries,
          scatter:{rows:scatter.rows.length,ms:scatter.ms,first:scatter.rows[0]},
          filtered:{rows:filtered.rows.length,ms:filtered.ms},emptyRows:empty.rows.length,readback,integrityError,
        };
        const registeredBefore = (await db.globFiles('*')).filter((file)=>file.fileName?.startsWith(`${handle.storeURL}/`));
        check(registeredBefore.length === handle.fileCount, 'Cannot enumerate registered sample files before disposal');
        await conn.query('DROP VIEW pbmc_coordinates');
        await conn.query('DROP VIEW pbmc_obs');
        window.testStep = 'dispose';
        await handle.dispose();
        await handle.dispose(); // idempotent
        window.testStep = 'verify disposal';
        const remaining = (await db.globFiles('*')).filter((file) => file.fileName?.startsWith(`${handle.storeURL}/`));
        check(remaining.length === 0, 'Disposed source files remain registered');
        report.cleanup = {registeredBefore:registeredBefore.length,remainingFiles:remaining.length};
        window.testStep = 'reload';
        const replacement = await remote.loadRemoteAnnData(db);
        try {
          check(replacement.storeURL !== handle.storeURL,'Reload reused its virtual namespace');
          for(const sql of remote.remoteAnnDataViewSQL(replacement.storeURL)) await conn.query(sql);
          const reload = await query('SELECT COUNT(*) AS cells FROM pbmc_obs');
          check(reload.rows[0].cells === '700','Reload failed');
          report.reload = {storeURL:replacement.storeURL,cells:reload.rows[0].cells};
          await conn.query('DROP VIEW pbmc_coordinates');
          await conn.query('DROP VIEW pbmc_obs');
        } finally { await replacement.dispose(); }
        report.totalMs = performance.now()-start;
        report.storage = {localStorageEntries:localStorage.length,sessionStorageEntries:sessionStorage.length,cacheEntries:await caches.keys()};
        document.getElementById('report').textContent=JSON.stringify(report,null,2);
        return report;
      } finally {
        await handle?.dispose();
        await conn?.close();
        await db.terminate();
      }
    }),
    new Promise((_, reject) => { timer=setTimeout(()=>reject(new Error('Live remote AnnData test exceeded 90 seconds')),90000); }),
  ]);
  assert.equal(errors.length,0,JSON.stringify(errors));
  assert.equal(requests.filter(url=>new URL(url).pathname.startsWith('/data/')).length,0,'Site data payload was requested');
  assert.equal(requests.filter(url=>new URL(url).hostname.endsWith('.invalid')).length,0,'Virtual file handle caused an actual HTTP request');
  assert.ok(responses.some(response=>response.url.includes('raw.githubusercontent.com/scverse/scanpy/') && response.status===200),'No successful live upstream fetch');
  const report={result,requests,responses,errors};
  await writeFile(path.join(evidence,'remote-anndata-browser.json'),JSON.stringify(report,null,2));
  await page.screenshot({path:path.join(evidence,'remote-anndata-browser.png'),fullPage:true});
  console.log(JSON.stringify({version:result.version,shape:result.handle.shape,cells:result.scatter.rows,filtered:result.filtered.rows,expression:result.queries.find(q=>q.sql.includes('measurements')).rows,cleanup:result.cleanup,reload:result.reload,totalMs:result.totalMs,evidence},null,2));
} catch(error) {
  const step = await page.evaluate(()=>window.testStep).catch(()=>null);
  console.error('Failure step:',step);
  await writeFile(path.join(evidence,'remote-anndata-failure.json'),JSON.stringify({error:error.stack,step,requests,responses,errors},null,2));
  throw error;
} finally {
  clearTimeout(timer);
  await browser.close();
}
