// Run after `uv run scripts/build_docs.py` from the repository root.
// Uses the app's existing Playwright dependency; no scientific data are mocked.
import { chromium } from '@playwright/test';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { createReadStream } from 'node:fs';
import { readFile, readdir, stat, mkdir } from 'node:fs/promises';
import { resolve, relative, join, extname, sep } from 'node:path';
import { fileURLToPath } from 'node:url';

const site = resolve(process.argv[2] ?? 'site');
const evidence = fileURLToPath(new URL('../test-results/docsite/', import.meta.url));
const prefix = '/duckdb-zarr/';
const mime = { '.html': 'text/html', '.css': 'text/css', '.js': 'text/javascript', '.json': 'application/json', '.wasm': 'application/wasm', '.svg': 'image/svg+xml', '.png': 'image/png', '.txt': 'text/plain' };
const server = createServer(async (request, response) => {
  try {
    const url = new URL(request.url, 'http://localhost');
    const base = url.pathname.startsWith(prefix) ? prefix : '/';
    let target = resolve(site, decodeURIComponent(url.pathname.slice(base.length)));
    if (target !== site && !target.startsWith(site + sep)) { response.writeHead(403).end(); return; }
    const info = await stat(target);
    if (info.isDirectory()) {
      if (!url.pathname.endsWith('/')) { response.writeHead(301, { Location: url.pathname + '/' + url.search }).end(); return; }
      target = join(target, 'index.html');
    }
    await stat(target);
    response.writeHead(200, { 'Content-Type': mime[extname(target)] ?? 'application/octet-stream' });
    createReadStream(target).pipe(response);
  } catch { response.writeHead(404, { 'Content-Type': 'text/plain' }).end('Not found'); }
});
await new Promise((done) => server.listen(0, '127.0.0.1', done));
const origin = `http://127.0.0.1:${server.address().port}`;
const base = `${origin}${prefix}`;
let browser;

/** List generated HTML pages recursively for internal-link and fragment validation. */
async function htmlFiles(directory) {
  const all = [];
  for (const entry of await readdir(directory, { withFileTypes: true })) {
    if (entry.isDirectory()) all.push(...await htmlFiles(join(directory, entry.name)));
    else if (entry.name.endsWith('.html')) all.push(join(directory, entry.name));
  }
  return all;
}

try {
  const files = await htmlFiles(site);
  let checked = 0;
  for (const file of files) {
    const html = await readFile(file, 'utf8');
    const current = new URL(relative(site, file).split(sep).join('/'), base);
    for (const [, raw] of html.matchAll(/(?:href|src)="([^\"]+)"/g)) {
      const url = new URL(raw.replaceAll('&amp;', '&'), current);
      if (url.origin !== origin) continue;
      assert.ok(url.pathname.startsWith(prefix), `Link escaped the project prefix: ${file} → ${raw}`);
      let target = resolve(site, decodeURIComponent(url.pathname.slice(prefix.length)));
      assert.ok(target === site || target.startsWith(site + sep));
      const targetInfo = await stat(target);
      if (targetInfo.isDirectory()) target = join(target, 'index.html');
      assert.ok((await stat(target)).isFile(), `Broken link: ${file} → ${raw}`);
      if (url.hash && extname(target) === '.html') {
        const id = decodeURIComponent(url.hash.slice(1));
        const content = await readFile(target, 'utf8');
        assert.ok(content.includes(`id="${id}"`), `Missing fragment: ${file} → ${raw}`);
      }
      checked++;
    }
  }
  console.log(`Checked ${checked} internal links/assets/fragments across ${files.length} HTML pages under ${prefix}`);
  assert.equal((await fetch(base + 'missing.zarr/.zarray')).status, 404);
  assert.equal((await fetch(base + 'biology/data/manifest.json')).status, 404);

  browser = await chromium.launch({ headless: true, args: ['--enable-webgl', '--use-gl=angle', '--use-angle=swiftshader', '--enable-unsafe-swiftshader'] });
  const page = await browser.newPage({ viewport: { width: 1440, height: 1100 } });
  const errors = [];
  page.on('pageerror', (error) => errors.push(error.message));
  await page.goto(base);
  assert.equal((await page.locator('main h1').textContent()).replace(/¶$/, ''), 'duckdb-zarr');
  const docsPalette = await page.evaluate(() => ({ color: getComputedStyle(document.documentElement).color, background: getComputedStyle(document.documentElement).backgroundColor, heading: getComputedStyle(document.querySelector('h1')).fontFamily }));
  await mkdir(evidence, { recursive: true });
  await page.screenshot({ path: join(evidence, 'docs-desktop.png'), fullPage: false });
  await page.getByRole('searchbox', { name: 'Search documentation' }).fill('AnnData');
  await page.locator('#search-results a').first().waitFor();
  await page.locator('#search-results a').first().click();
  assert.ok(page.url().startsWith(base + 'anndata/'), `Search lost the prefix: ${page.url()}`);
  assert.ok((await page.locator('main h1').textContent()).includes('AnnData'));
  await page.getByRole('navigation', { name: 'Project navigation', exact: true }).getByRole('link', { name: 'Examples', exact: true }).click();
  assert.equal(page.url(), base + 'examples/');
  assert.equal((await page.locator('main h1').textContent()).replace(/¶$/, ''), 'Examples');
  assert.equal(await page.getByRole('navigation', { name: 'Project navigation', exact: true }).getByRole('link', { name: 'Examples', exact: true }).getAttribute('aria-current'), 'page');
  assert.equal(await page.locator('main canvas').count(), 0, 'The index should not load interactive datasets.');
  await page.locator('main').getByRole('link', { name: 'For biologists', exact: true }).click();
  assert.equal(page.url(), base + 'biology/');
  assert.equal(await page.getByRole('navigation', { name: 'Project navigation', exact: true }).getByRole('link', { name: 'Examples', exact: true }).getAttribute('href'), '../examples/');
  const appPalette = await page.evaluate(() => ({ color: getComputedStyle(document.documentElement).color, background: getComputedStyle(document.documentElement).backgroundColor, heading: getComputedStyle(document.querySelector('h1')).fontFamily }));
  assert.deepEqual(appPalette, docsPalette, 'The docs and app should retain the same palette and heading font.');
  assert.equal(await page.getByRole('link', { name: 'AnnData guide', exact: true }).getAttribute('href'), '../anndata/');
  assert.equal(await page.getByRole('link', { name: 'OME-Zarr guide', exact: true }).getAttribute('href'), '../ome-zarr/');
  await page.waitForFunction(() => document.getElementById('ome-status')?.textContent.includes('plot updated') || document.getElementById('ome-status')?.classList.contains('error'), null, { timeout: 180_000 });
  const status = await page.locator('#ome-status').textContent();
  assert.ok(status.includes('35,175,800 rows') && status.includes('plot updated'), status);
  assert.ok((await page.locator('#anndata-status').textContent()).includes('700 rows'));
  assert.equal(await page.locator('#ome-plot .biology-visualization').getAttribute('data-channel-ids'), '0,1');
  await page.getByRole('link', { name: 'OME-Zarr guide', exact: true }).click();
  assert.equal(page.url(), base + 'ome-zarr/');
  await page.getByRole('navigation', { name: 'Project navigation', exact: true }).getByRole('link', { name: 'Documentation', exact: true }).click();
  assert.equal(page.url(), base);

  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto(base + 'anndata/');
  assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth));
  const guides = page.getByRole('button', { name: 'Browse guides', exact: true });
  await guides.click();
  assert.equal(await guides.getAttribute('aria-expanded'), 'true');
  await page.getByRole('navigation', { name: 'Documentation guides', exact: true }).getByRole('link', { name: 'OME-Zarr', exact: true }).click();
  assert.equal(page.url(), base + 'ome-zarr/');
  assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth));
  await page.screenshot({ path: join(evidence, 'docs-mobile.png'), fullPage: false });
  await page.getByRole('navigation', { name: 'Project navigation', exact: true }).getByRole('link', { name: 'Examples', exact: true }).click();
  assert.equal(page.url(), base + 'examples/');
  assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth));
  await page.goto(origin + '/');
  assert.equal((await page.locator('main h1').textContent()).replace(/¶$/, ''), 'duckdb-zarr');
  await page.getByRole('navigation', { name: 'Project navigation', exact: true }).getByRole('link', { name: 'Examples', exact: true }).click();
  assert.equal(page.url(), origin + '/examples/');
  assert.equal(await page.locator('main').getByRole('link', { name: 'For biologists', exact: true }).getAttribute('href'), '../biology/');
  assert.deepEqual(errors, []);
  console.log('Passed: docs search, examples index → live remote biology → format guide → docs, matched theme, mobile navigation, root and project-prefix hosting.');
  console.log(`Screenshot evidence: ${evidence}`);
} finally {
  await browser?.close();
  await new Promise((done) => server.close(done));
}
