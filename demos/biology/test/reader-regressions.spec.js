import { test, expect } from '@playwright/test';
import { fixtureStore, readerFixture } from './fixture-store.js';
import { PBMC_SOURCE_URL } from '../src/remote_anndata.js';
import { OME_URL } from '../src/remote_ome.js';
import { labelsSQL } from '../src/queries.js';

test('the real WASM reader handles omitted chunks and named nested arrays', async ({ page }) => {
  const store = await fixtureStore();
  try {
    // Explicit failure injection avoids downloading the demonstration datasets.
    // The subsequent source change is to an openly named synthetic format
    // fixture served over real HTTP, never a fabricated response from S3.
    await page.route(PBMC_SOURCE_URL, (route) => route.abort('failed'));
    await page.route(`${OME_URL}/**`, (route) => route.abort('failed'));
    await page.goto('/');
    await expect(page.locator('#runtime')).toContainText('zarr extension loaded');
    await expect(page.locator('#ome-status')).toContainText('No local dataset fallback');
    await page.locator('#ome-source').fill(store.url);
    await page.locator('#ome-load-source').click();
    await expect(page.locator('#ome-status')).toContainText('8 rows');
    await expect(page.locator('#ome-status')).toContainText('plot updated');
    await expect(page.locator('#ome-provenance')).toContainText('4 original remote objects');
    const sql = `SELECT c, z, y, x, value FROM read_zarr('${store.url}', array_path := '0') ORDER BY c, z, y, x`;
    await page.locator('#ome-sql').fill(sql);
    await page.locator('#ome-run').click();
    await expect(page.locator('#ome-run')).toBeEnabled();
    const intensities = await page.locator('#ome-results tbody tr').evaluateAll((rows) => rows.map((row) => row.cells[4].textContent === 'NULL' ? null : Number(row.cells[4].textContent)));
    // parse_zarr_fill_sentinel maps nonzero Zarr fill sentinels to SQL NULL;
    // prefetch must defer to that reader behavior, not manufacture zero bytes.
    expect(intensities).toEqual(readerFixture.expected.intensities.map((value) => value === readerFixture.expected.fill_value ? null : value));
    expect(intensities.slice(0, 4)).toEqual(Array(4).fill(null));
    expect(store.requests).toContain(readerFixture.expected.missing_chunk);

    await page.locator('#ome-sql').fill(labelsSQL(store.url, { zmin: 0, zmax: 1, arrayPath: 'labels/nuclei' }) + '\nORDER BY z, y, x');
    await page.locator('#ome-run').click();
    await expect(page.locator('#ome-run')).toBeEnabled();
    await expect(page.locator('#ome-status')).not.toHaveClass('error');
    const labels = await page.locator('#ome-results tbody tr').evaluateAll((rows) => rows.map((row) => Number(row.cells[3].textContent)));
    expect(labels).toEqual(readerFixture.expected.labels);
  } finally { await store.close(); }
});
