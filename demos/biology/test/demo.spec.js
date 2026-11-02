import { test, expect } from '@playwright/test';
import { PBMC_SOURCE_URL } from '../src/remote_anndata.js';
import { OME_URL } from '../src/remote_ome.js';

/** Wait for actual remote example startup and verify loaded data plus unlocked query controls. */
async function ready(page) {
  await page.goto('/');
  await page.waitForFunction(() => document.querySelector('#runtime').classList.contains('error') || document.querySelector('#ome-status').textContent.includes('plot updated') || document.querySelector('#ome-status').classList.contains('error'), null, { timeout: 180_000 });
  await expect(page.locator('#runtime')).toContainText('zarr extension loaded');
  await expect(page.locator('#anndata-status')).toContainText('700 rows');
  await expect(page.locator('#ome-status')).toContainText('plot updated', { timeout: 1000 });
  await expect(page.locator('#ome-run')).toBeEnabled();
}
/** Run SQL through the real editor and wait until the query controls are unlocked. */
async function query(page, kind, sql) {
  await page.locator(`#${kind}-sql`).fill(sql);
  await page.locator(`#${kind}-run`).click();
  await expect(page.locator(`#${kind}-run`)).toBeEnabled();
}
/** Read channel hues from a synchronously redrawn WebGL buffer before it is discarded. */
async function pixelColors(canvas) {
  // Trigger a synchronous VTK render and read its original drawing buffer in
  // the same task, before a non-preserved WebGL buffer can be discarded.
  return canvas.evaluate((el) => {
    el.dispatchEvent(new MouseEvent('dblclick', { bubbles: true }));
    const gl = el.getContext('webgl2');
    const pixels = new Uint8Array(el.width * el.height * 4);
    gl.readPixels(0, 0, el.width, el.height, gl.RGBA, gl.UNSIGNED_BYTE, pixels);
    let blue = 0, yellow = 0;
    for (let i = 0; i < pixels.length; i += 4) {
      if (pixels[i + 2] > 50 && pixels[i + 2] > pixels[i] * 1.5 && pixels[i + 2] > pixels[i + 1] * 1.5) blue++;
      if (pixels[i] > 50 && pixels[i + 1] > 50 && pixels[i] > pixels[i + 2] * 1.5 && pixels[i + 1] > pixels[i + 2] * 1.5) yellow++;
    }
    return { blue, yellow };
  });
}

// One live workflow avoids re-downloading the same 472 original S3 chunks for
// every UI assertion. No success-path data requests or SQL results are mocked.
test('original remote datasets, native-resolution composite, SQL and interaction work together', async ({ page }, info) => {
  test.setTimeout(300_000);
  const errors = [], requests = [], remoteChunks = new Set();
  page.on('pageerror', (error) => errors.push(error.message));
  page.on('request', (request) => {
    requests.push(request.url());
    if (request.url().startsWith(`${OME_URL}/`) && /^0\/\d+\/\d+\/\d+\/\d+$/.test(request.url().slice(OME_URL.length + 1))) remoteChunks.add(request.url());
  });
  await ready(page);
  await test.step('remote provenance and full native grid, no site dataset payload', async () => {
    await expect(page.locator('#anndata-source')).toHaveValue(PBMC_SOURCE_URL);
    await expect(page.locator('#ome-source')).toHaveValue(OME_URL);
    await expect(page.locator('#ome-status')).toContainText('35,175,800 rows');
    await expect(page.locator('#ome-dataset-label')).toContainText('271×275×236');
    await expect(page.locator('#ome-channel')).toHaveValue('all');
    await expect(page.getByRole('button', { name: 'Image statistics', exact: true })).toHaveCount(0);
    await expect(page.locator('#anndata-stats')).toBeEnabled();
    expect(remoteChunks.size).toBe(472);
    expect(requests.some((url) => url.includes('/data/'))).toBe(false);
    expect(requests.some((url) => url.includes('duckdb-wasm-memory.invalid'))).toBe(false);
    const geometry = page.locator('.volume-geometry');
    await expect(geometry).toHaveAttribute('data-voxel-dimensions', '271,275,236');
    const physical = (await geometry.getAttribute('data-physical-size')).split(',').map(Number);
    const spacing = [0.3603981534640209, 0.3603981534640209, 0.5002025531914894];
    for (let i = 0; i < 3; i++) expect(physical[i]).toBeCloseTo([271, 275, 236][i] * spacing[i], 6);
  });
  const volume = page.locator('#ome-plot .biology-visualization');
  const canvas = page.locator('#ome-plot canvas').first();
  await test.step('rotation, pause, real composite pixels, and a clean channel key', async () => {
    await canvas.scrollIntoViewIfNeeded();
    await expect(volume).toHaveAttribute('data-rotation-state', 'running');
    const before = await volume.getAttribute('data-camera-position');
    await expect.poll(() => volume.getAttribute('data-camera-position')).not.toBe(before);
    await page.getByRole('button', { name: 'Pause rotation', exact: true }).click();
    await expect(volume).toHaveAttribute('data-rotation-state', 'paused');
    const paused = await volume.getAttribute('data-camera-position');
    await page.waitForTimeout(400);
    expect(await volume.getAttribute('data-camera-position')).toBe(paused);
    await expect(volume).toHaveAttribute('data-channel-ids', '0,1');
    const key = page.getByRole('list', { name: 'Channel colors' });
    await expect(key.locator('[data-active="true"]')).toHaveCount(2);
    await expect(key.getByRole('listitem')).toHaveText(['LaminB1', 'Dapi']);
    await expect(page.locator('.volume-label-legend, #ome-plot input, #ome-plot select')).toHaveCount(0);
    const pixels = await pixelColors(canvas);
    expect(pixels.blue).toBeGreaterThan(100);
    expect(pixels.yellow).toBeGreaterThan(100);
    await canvas.screenshot({ path: info.outputPath('native-both-channels.png') });
    const imageBefore = await canvas.screenshot();
    await canvas.evaluate((el) => {
      const b = el.getBoundingClientRect(), x = b.x + b.width / 2, y = b.y + b.height / 2;
      const fire = (type, dx, dy, buttons) => el.dispatchEvent(new PointerEvent(type, { clientX: x + dx, clientY: y + dy, bubbles: true, button: 0, buttons, pointerId: 1 }));
      fire('pointerdown', 0, 0, 1);
      fire('pointermove', 90, 40, 1);
      fire('pointerup', 90, 40, 0);
    });
    expect((await canvas.screenshot()).equals(imageBefore)).toBe(false);
  });
  await test.step('single-channel isolation uses actual c values and actual source colors', async () => {
    for (const [index, color] of ['#0000ff', '#ffff00'].entries()) {
      await page.locator('#ome-channel').selectOption(String(index));
      await expect(page.locator('#ome-run')).toBeEnabled();
      await expect(volume).toHaveAttribute('data-channel-ids', String(index));
      await expect(volume).toHaveAttribute('data-channel-color', color);
      const pixels = await pixelColors(canvas);
      expect(index ? pixels.yellow : pixels.blue).toBeGreaterThan(100);
      expect(index ? pixels.yellow : pixels.blue).toBeGreaterThan(index ? pixels.blue : pixels.yellow);
    }
  });
  await test.step('full-resolution SQL statistics match independent source readback', async () => {
    // These fixed references were independently decoded from the original
    // source, not generated from this page's SQL or display-normalized texture.
    const reference = [[0, 17587900, 3, 4095, 1151058505], [1, 17587900, 24, 2380, 1566262661]];
    const sql = `SELECT c, COUNT(*) AS n, MIN(value) AS minimum, MAX(value) AS maximum, SUM(value)::BIGINT AS total FROM read_zarr('${OME_URL}', array_path := '0') GROUP BY c ORDER BY c`;
    await query(page, 'ome', sql);
    const rows = await page.locator('#ome-results tbody tr').evaluateAll((rows) => rows.map((row) => [...row.cells].map((cell) => Number(cell.textContent))));
    expect(rows).toEqual(reference);
    await expect(volume).toHaveAttribute('data-rotation-state', 'empty');
  });
  await test.step('original PBMC annotations and stored UMAP survive remote ZIP transport', async () => {
    await query(page, 'anndata', "SELECT X_umap_component, X_umap FROM pbmc_coordinates WHERE obs = 'AAAGCCTGGCTAAC-1' ORDER BY X_umap_component");
    const coords = await page.locator('#anndata-results tbody tr').evaluateAll((rows) => rows.map((row) => Number(row.cells[1].textContent)));
    expect(coords[0]).toBeCloseTo(-1.9918625454649166, 4);
    expect(coords[1]).toBeCloseTo(8.57641706171008, 4);
    await page.locator('#anndata-filter').selectOption({ label: 'CD14+ Monocyte' });
    await expect(page.locator('#anndata-run')).toBeEnabled();
    await expect(page.locator('#anndata-status')).toContainText('129 rows');
    await query(page, 'anndata', 'SELECT 1 AS count');
    await expect(page.locator('#anndata-plot [role=status]')).toContainText('No SQL rows');
    await query(page, 'anndata', 'SELECT * FROM absent_table');
    await expect(page.locator('#anndata-status')).toHaveClass('error');
    await query(page, 'anndata', "SELECT 0 AS x, 0 AS y, 'empty' AS label WHERE false");
    await expect(page.locator('#anndata-status')).toContainText('0 rows');
    await page.locator('#anndata-source').fill('not a url');
    await page.locator('#anndata-load-source').click();
    await expect(page.locator('#anndata-status')).toHaveClass('error');
    await page.locator('#anndata-source-reset').click();
    await expect(page.locator('#anndata-status')).toContainText('700 rows');
    await expect(page.locator('#anndata-source')).toHaveValue(PBMC_SOURCE_URL);
    await page.locator('#anndata-stats').click();
    await expect(page.locator('#anndata-status')).toContainText('table result');
    await expect(page.locator('#anndata-results tbody tr')).toHaveCount(10);
  });
  await test.step('SQL depth selection updates both channels', async () => {
    await page.locator('#ome-channel').selectOption('all');
    await expect(page.locator('#ome-run')).toBeEnabled();
    await page.locator('#ome-zmax').fill('140');
    await page.locator('#ome-zmin').fill('90');
    await page.locator('#ome-reset').click();
    await expect(page.locator('#ome-status')).toContainText('plot updated');
    await expect(page.locator('.volume-geometry')).toHaveAttribute('data-voxel-dimensions', '271,275,51');
    await expect(volume).toHaveAttribute('data-channel-ids', '0,1');
    await page.getByRole('button', { name: 'Resume rotation', exact: true }).click();
    await canvas.scrollIntoViewIfNeeded();
    await expect(volume).toHaveAttribute('data-rotation-state', 'running');
    await page.locator('.intro').scrollIntoViewIfNeeded();
    await expect(volume).toHaveAttribute('data-rotation-state', 'offscreen');
  });
  expect(errors).toEqual([]);
});

test('reduced-motion preference starts the remote composite still and allows opting in', async ({ page }) => {
  test.setTimeout(180_000);
  await page.emulateMedia({ reducedMotion: 'reduce' });
  await ready(page);
  const volume = page.locator('#ome-plot .biology-visualization');
  await page.locator('#ome-plot canvas').first().scrollIntoViewIfNeeded();
  await expect(volume).toHaveAttribute('data-rotation-state', 'paused');
  const before = await volume.getAttribute('data-camera-position');
  await page.waitForTimeout(400);
  expect(await volume.getAttribute('data-camera-position')).toBe(before);
  await page.getByRole('button', { name: 'Resume rotation', exact: true }).click();
  await expect.poll(() => volume.getAttribute('data-camera-position')).not.toBe(before);
});

test('missing extension disables query controls with actionable guidance', async ({ page }) => {
  await page.route('**/zarr.duckdb_extension.wasm', (route) => route.fulfill({ status: 404, body: '' }));
  await page.goto('/');
  await expect(page.locator('#runtime')).toContainText('Could not load');
  for (const kind of ['anndata', 'ome']) await expect(page.locator(`#${kind}-run`)).toBeDisabled();
});

test('remote AnnData failure is explicit, with no local fallback', async ({ page }) => {
  const local = [];
  page.on('request', (request) => { if (request.url().includes('/data/')) local.push(request.url()); });
  await page.route(PBMC_SOURCE_URL, (route) => route.abort('failed'));
  await page.goto('/');
  await expect(page.locator('#anndata-status')).toContainText('No local dataset fallback');
  await expect(page.locator('#anndata-run')).toBeDisabled();
  expect(local).toEqual([]);
});

test('a remote chunk network failure is reported rather than rendered as invented zeros', async ({ page }) => {
  test.setTimeout(120_000);
  await page.route(`${OME_URL}/0/0/0/0/0`, (route) => route.abort('failed'));
  await page.goto('/');
  await expect(page.locator('#ome-status')).toContainText('No local dataset fallback');
  await expect(page.locator('#ome-run')).toBeDisabled();
  await expect(page.locator('#ome-load-source')).toBeEnabled();
  await expect(page.locator('#anndata-status')).toContainText('700 rows');
});

test('no prepared dataset is served, and missing keys remain real HTTP 404s', async ({ request }) => {
  for (const key of ['data/manifest.json', 'data/pbmc.zarr/.zgroup', 'data/idr0062A.ome.zarr/2/.zarray']) {
    const response = await request.get(`/${key}`);
    expect(response.status()).toBe(404);
  }
});

test('quiet two-item index, visible logos and biology copy work on mobile', async ({ page }) => {
  // This is a layout-only case: do not spend another remote volume download.
  await page.route('**/zarr.duckdb_extension.wasm', (route) => route.abort('failed'));
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto('/');
  const links = page.getByRole('navigation', { name: 'On this page', exact: true }).getByRole('link');
  await expect(links).toHaveCount(2);
  const layout = await links.evaluateAll((items) => items.map((el) => ({ height: el.getBoundingClientRect().height, background: getComputedStyle(el).backgroundColor, radius: getComputedStyle(el).borderRadius })));
  for (const item of layout) {
    expect(item.background).toBe('rgba(0, 0, 0, 0)');
    expect(item.radius).toBe('0px');
    expect(item.height).toBeGreaterThanOrEqual(44);
    expect(item.height).toBeLessThanOrEqual(58);
  }
  const images = await page.locator('.contents-logo').evaluateAll((items) => items.map((image) => ({ loaded: image.complete && image.naturalWidth > 0, width: image.getBoundingClientRect().width })));
  expect(images).toHaveLength(2);
  for (const image of images) { expect(image.loaded).toBe(true); expect(image.width).toBe(32); }
  await expect(page.locator('#anndata')).toContainText('single-cell RNA sequencing');
  await expect(page.locator('#spatialdata')).toHaveCount(0);
  await links.filter({ hasText: 'fluorescence' }).click();
  await expect(page).toHaveURL(/#ome$/);
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
});
