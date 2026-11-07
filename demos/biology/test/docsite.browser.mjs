// Run after `uv run scripts/build_docs.py` from the repository root.
// Uses the app's existing Playwright dependency; no scientific data are mocked.
// Add --layout-only to inspect HTML/CSS/navigation without starting the query engine.
import { chromium } from '@playwright/test';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { createReadStream } from 'node:fs';
import { readFile, readdir, stat, mkdir } from 'node:fs/promises';
import { resolve, relative, join, extname, sep } from 'node:path';
import { fileURLToPath } from 'node:url';

const site = resolve(process.argv[2] ?? 'site');
const layoutOnly = process.argv.includes('--layout-only');
const evidence = fileURLToPath(new URL(layoutOnly ? '../test-results/docsite-layout/' : '../test-results/docsite/', import.meta.url));
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

/** Open a direct sidebar guide, expanding the mobile menu when needed. */
async function openGuide(page, label) {
  // Wait for the stylesheet before deciding whether the mobile toggle is visible.
  await page.waitForLoadState('load');
  const toggle = page.getByRole('button', { name: 'Browse guides', exact: true });
  if (await toggle.isVisible() && await toggle.getAttribute('aria-expanded') !== 'true') await toggle.click();
  await page.getByRole('navigation', { name: 'Documentation guides', exact: true }).getByRole('link', { name: label, exact: true }).click();
  await page.waitForLoadState('load');
}

/** Check the existing project artwork is loaded at the requested header/home sizes. */
async function projectLogos(page, home = false) {
  assert.equal((await page.locator('.wordmark').textContent()).trim(), 'duckdb-zarr');
  const header = page.locator('.wordmark .brand-logo');
  assert.equal(await header.count(), 1);
  assert.ok(await header.evaluate((image) => image.complete && image.naturalWidth > 0));
  const headerSize = await header.boundingBox();
  assert.equal(headerSize.width, 32);
  assert.equal(headerSize.height, 32);
  if (home) {
    const logo = page.locator('main .homepage-logo');
    assert.equal(await logo.count(), 1);
    assert.ok(await logo.evaluate((image) => image.complete && image.naturalWidth > 0 && Boolean(image.compareDocumentPosition(document.querySelector('main h1')) & Node.DOCUMENT_POSITION_FOLLOWING)));
    assert.equal(await logo.getAttribute('alt'), 'duckdb-zarr logo');
    assert.equal(await logo.evaluate((image) => image.currentSrc), await header.evaluate((image) => image.currentSrc));
    const size = await logo.boundingBox();
    assert.equal(size.width, 240);
    assert.equal(size.height, 240);
  }
}

/** Keep the public concepts/SQL guide discoverable through the shared docsite. */
async function publicReference(page, expectedBase) {
  const navigation = page.getByRole('navigation', { name: 'Documentation guides', exact: true, includeHidden: true });
  const guide = navigation.getByRole('link', { name: 'How it works', exact: true, includeHidden: true });
  assert.equal(await guide.count(), 1, 'Publish the concepts/SQL guide under its public-facing name.');
  assert.equal(await guide.evaluate((link) => link.closest('ul').previousElementSibling.textContent.trim()), 'Reference');
  await openGuide(page, 'How it works');
  assert.equal(page.url(), expectedBase + 'design/');
  assert.equal((await page.locator('main h1').textContent()).replace(/¶$/, ''), 'How duckdb-zarr works');
  assert.equal(await guide.getAttribute('aria-current'), 'page');
  for (const id of ['background', 'from-arrays-to-tables', 'the-sql-interface', 'reading-values-correctly', 'known-limits', 'roadmap']) {
    assert.equal(await page.locator(`main h2#${id}`).count(), 1);
    assert.equal(await page.locator(`.page-toc a[href="#${id}"]`).count(), 1);
  }
  assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth), 'The public reference must fit desktop and mobile layouts.');
  await openGuide(page, 'Start here');
  const introduction = page.locator('main');
  const overview = introduction.locator('h1 + p + p').getByRole('link', { name: 'How it works', exact: true });
  assert.equal(await overview.count(), 1, 'Link to How it works directly below the Start here introduction.');
  await overview.click();
  await page.waitForLoadState('load');
  assert.equal(page.url(), expectedBase + 'design/');
  await openGuide(page, 'Start here');
  const entry = introduction.getByRole('link', { name: 'How duckdb-zarr works', exact: true });
  assert.equal(await entry.count(), 1);
  assert.equal(new URL(await entry.getAttribute('href'), page.url()).href, expectedBase + 'design/');
  for (const id of ['from-arrays-to-tables', 'the-sql-interface', 'known-limits', 'roadmap']) {
    assert.equal(await introduction.locator(`a[href="design/#${id}"]`).count(), 1);
  }
}

/** Compare the shared navigation's typography and spacing across guide and app pages. */
async function navigationStyles(page) {
  await projectLogos(page);
  assert.equal(await page.locator('.project-header .docs-search').count(), 1, 'Search belongs inside the top header.');
  assert.equal(await page.getByRole('searchbox', { name: 'Search documentation' }).count(), 1);
  const searchFits = await page.locator('.docs-search input').evaluate((input) => {
    const header = document.querySelector('.project-header').getBoundingClientRect();
    const box = input.getBoundingClientRect();
    return box.left >= header.left && box.right <= header.right && box.top >= header.top && box.bottom <= header.bottom;
  });
  assert.ok(searchFits, 'Keep search within the header on desktop and mobile.');
  return page.evaluate(() => ['.project-header', '.docs-tools', '.docs-search input', '#guide-nav', '#guide-nav .nav-heading', '.guides-toggle'].map((selector) => {
    const style = getComputedStyle(document.querySelector(selector));
    return [selector, ...['fontSize', 'fontWeight', 'lineHeight', 'margin', 'padding', 'width'].map((property) => style[property])];
  }));
}

/** Snapshot actual typography so example pages can be compared to the biology reference. */
async function titleStyles(page) {
  const layout = await page.locator('main h1').evaluate((heading) => {
    const box = heading.getBoundingClientRect();
    return {
      height: box.height,
      expectedHeight: 2 * parseFloat(getComputedStyle(heading).lineHeight),
      gap: heading.nextElementSibling.getBoundingClientRect().top - box.bottom,
      permalinkBoxes: [...heading.querySelectorAll('.headerlink')].reduce((total, link) => total + link.getClientRects().length, 0),
    };
  });
  assert.equal(layout.permalinkBoxes, 0, 'Title permalinks must not add a glyph or an extra line.');
  assert.ok(Math.abs(layout.height - layout.expectedHeight) < 2, 'Field-guide titles should occupy exactly two lines.');
  assert.ok(layout.gap >= 0 && layout.gap <= 24, 'Introductory content should follow the title without a large gap.');
  return page.locator('main h1').evaluate((element) => {
    const style = getComputedStyle(element);
    return Object.fromEntries(['fontFamily', 'fontSize', 'fontWeight', 'lineHeight', 'letterSpacing', 'margin', 'color'].map((property) => [property, style[property]]));
  });
}

/** Compare normal introductory copy across Markdown and interactive example pages. */
async function subtitleStyles(page, selector = '.example-content h1 + p') {
  return page.locator(selector).evaluate((element) => {
    const style = getComputedStyle(element);
    return Object.fromEntries(['fontFamily', 'fontSize', 'fontWeight', 'lineHeight', 'letterSpacing', 'margin', 'color', 'maxWidth'].map((property) => [property, style[property]]));
  });
}

/** Keep the same closing message at the end of every field guide. */
async function guideClosing(page) {
  const heading = page.locator('main h2').filter({ hasText: 'Bring your own questions.' });
  assert.equal(await heading.count(), 1);
  assert.equal(await heading.innerText().then((text) => text.replace(/¶$/, '').trim()), 'Bring your own questions.\nduckdb-zarr for the answers.');
  assert.equal(await heading.locator('br').count(), 1);
  const answer = heading.locator('.closing-answer');
  assert.equal(await answer.count(), 1);
  const typography = await heading.evaluate((element) => {
    const answer = element.querySelector('.closing-answer');
    const properties = ['fontFamily', 'fontSize', 'fontWeight', 'lineHeight', 'letterSpacing'];
    const styles = (node) => properties.map((property) => getComputedStyle(node)[property]);
    const firstLine = document.createRange();
    firstLine.selectNodeContents(element.firstChild);
    return { heading: styles(element), answer: styles(answer), firstBottom: firstLine.getBoundingClientRect().bottom, answerTop: answer.getBoundingClientRect().top };
  });
  assert.deepEqual(typography.answer, typography.heading, 'Both closing lines must have identical typography.');
  assert.ok(typography.answerTop >= typography.firstBottom - 1, 'Keep the answer on its own line.');
}

/** Verify the dark project name and green second line without changing concise navigation labels. */
async function exampleTitle(page, domain) {
  await guideClosing(page);
  const heading = page.locator('main h1');
  assert.equal((await heading.textContent()).replace(/¶$/, ''), `duckdb-zarr for ${domain}.`);
  assert.equal(await heading.locator('.title-accent').textContent(), `for ${domain}.`);
  const colors = await heading.evaluate((element) => ({
    project: getComputedStyle(element).color,
    body: getComputedStyle(document.documentElement).color,
    accent: getComputedStyle(element.querySelector('.title-accent')).color,
    display: getComputedStyle(element.querySelector('.title-accent')).display,
  }));
  assert.equal(colors.project, colors.body);
  assert.equal(colors.accent, 'rgb(23, 106, 76)');
  assert.equal(colors.display, 'block');
  return titleStyles(page);
}

try {
  const files = await htmlFiles(site);
  assert.ok(!files.some((file) => relative(site, file).split(sep).join('/') === 'examples/index.html'), 'The separate field-guides landing page must not be published.');
  assert.ok(!files.some((file) => relative(site, file).includes('filter-pushdown-learnings')), 'Internal handoff notes must not be published.');
  let checked = 0;
  for (const file of files) {
    const html = await readFile(file, 'utf8');
    const wordmark = html.match(/class="wordmark"[^>]*>(.*?)<\/a>/s)?.[1];
    assert.equal(wordmark?.replace(/<[^>]*>/g, '').trim(), 'duckdb-zarr', 'Keep only the project name alongside its logo.');
    assert.match(wordmark, /<img[^>]*class="brand-logo"/);
    const current = new URL(relative(site, file).split(sep).join('/'), base);
    for (const [, raw] of html.matchAll(/(?:href|src)="([^\"]+)"/g)) {
      const url = new URL(raw.replaceAll('&amp;', '&'), current);
      if (url.origin !== origin) continue;
      assert.notEqual(url.pathname, prefix + 'examples/', 'No links may target the removed guide index.');
      assert.notEqual(url.pathname, prefix + 'examples/index.html');
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
  assert.equal((await fetch(base + 'examples/')).status, 404);
  assert.equal((await fetch(origin + '/examples/')).status, 404);
  assert.equal((await fetch(base + 'biology/data/manifest.json')).status, 404);
  assert.equal((await fetch(base + 'filter-pushdown-learnings/')).status, 404);
  assert.equal((await fetch(base + 'filter-pushdown-learnings.md')).status, 404);
  const search = await (await fetch(base + 'search/search_index.json')).json();
  assert.ok(search.docs.every((doc) => !doc.location.includes('filter-pushdown-learnings') && !doc.text.includes('Handoff notes for a future agent.')), 'Internal notes must not leak into search.');
  assert.ok(search.docs.every((doc) => !['examples/', 'examples/index.html'].includes(doc.location.split('#')[0])), 'The removed guide index must not appear in search.');

  browser = await chromium.launch({ headless: true, args: ['--enable-webgl', '--use-gl=angle', '--use-angle=swiftshader', '--enable-unsafe-swiftshader'] });
  const page = await browser.newPage({ viewport: { width: 1440, height: 1100 } });
  if (layoutOnly) await page.route('**/biology/assets/*.js', (route) => route.abort('blockedbyclient'));
  const errors = [], externalRequests = [];
  if (layoutOnly) page.on('request', (request) => { if (new URL(request.url()).origin !== origin) externalRequests.push(request.url()); });
  page.on('pageerror', (error) => errors.push(error.message));
  await page.goto(base);
  assert.equal((await page.locator('main h1').textContent()).replace(/¶$/, ''), 'duckdb-zarr');
  assert.deepEqual(await page.locator('main #field-guides + ul a').allTextContents(), ['Biology', 'Climate', 'Geoscience']);
  assert.equal(await page.locator('main #thinking-in-arrays-and-sql').count(), 1);
  assert.equal(await page.locator('main a[href="https://github.com/xqlsystems/xarray-sql"]').count(), 1);
  assert.equal(await page.locator('main pre code').filter({ hasText: 'AVG(air) AS mean_air' }).count(), 1);
  assert.ok(search.docs.some((doc) => doc.location.startsWith('#thinking-in-arrays-and-sql')), 'The main-page introduction must be searchable.');
  for (const id of ['the-sql-interface', 'known-limits', 'roadmap']) {
    assert.ok(search.docs.some((doc) => doc.location === `design/#${id}`), 'The public reference sections must be searchable.');
  }
  await publicReference(page, base);
  const docsPalette = await page.evaluate(() => ({ color: getComputedStyle(document.documentElement).color, background: getComputedStyle(document.documentElement).backgroundColor, heading: getComputedStyle(document.querySelector('h1')).fontFamily }));
  await projectLogos(page, true);
  const docsNavigation = await navigationStyles(page);
  await page.setViewportSize({ width: 390, height: 844 });
  await publicReference(page, base);
  await projectLogos(page, true);
  const mobileNavigation = await navigationStyles(page);
  await page.getByRole('searchbox', { name: 'Search documentation' }).fill('AnnData');
  await page.locator('#search-results a').first().waitFor();
  const mobileResults = await page.locator('#search-results').boundingBox();
  assert.ok(mobileResults.x >= 0 && mobileResults.x + mobileResults.width <= 390, 'Mobile search results must stay within the viewport.');
  await page.keyboard.press('Escape');
  assert.equal(await page.locator('#search-results').isHidden(), true);
  await page.getByRole('searchbox', { name: 'Search documentation' }).fill('');
  await page.setViewportSize({ width: 1440, height: 1100 });
  await mkdir(evidence, { recursive: true });
  await page.screenshot({ path: join(evidence, 'docs-desktop.png'), fullPage: false });
  await page.getByRole('searchbox', { name: 'Search documentation' }).fill('AnnData');
  await page.locator('#search-results a').first().waitFor();
  await page.locator('#search-results a').first().click();
  assert.ok(page.url().startsWith(base + 'anndata/'), `Search lost the prefix: ${page.url()}`);
  assert.ok((await page.locator('main h1').textContent()).includes('AnnData'));
  assert.equal(await page.getByRole('link', { name: 'Field guides', exact: true }).count(), 0, 'Field guides is a section label, not a landing-page link.');
  assert.equal(await page.locator('#guide-nav .guide-group').filter({ hasText: /^Field guides$/ }).count(), 1);
  assert.equal(await page.locator('#guide-nav').getByRole('link', { name: 'Overview', exact: true }).count(), 0);
  const biologyExample = page.getByRole('navigation', { name: 'Documentation guides', exact: true }).getByRole('link', { name: 'Biology', exact: true });
  assert.equal(await biologyExample.count(), 1, 'Biology must be a sidebar example, not only an index link.');
  assert.equal(await biologyExample.evaluate((link) => link.closest('ul').previousElementSibling.textContent.trim()), 'Field guides');
  assert.deepEqual(await biologyExample.evaluate((link) => [...link.closest('ul').querySelectorAll(':scope > li > a')].map((item) => item.textContent.trim())), ['Biology', 'Climate', 'Geoscience']);
  for (const [label, section] of [['AnnData', 'Formats'], ['OME-Zarr', 'Formats'], ['Scientific domains', 'Reference']]) {
    const link = page.getByRole('navigation', { name: 'Documentation guides', exact: true }).getByRole('link', { name: label, exact: true });
    assert.equal(await link.evaluate((element) => element.closest('ul').previousElementSibling.textContent.trim()), section);
  }
  const guideLinks = await page.locator('#guide-nav a').evaluateAll((links) => links.map((link) => [link.textContent.trim(), link.href]));
  await openGuide(page, 'Climate');
  assert.equal(page.url(), base + 'examples/climate/');
  const climateTitle = await exampleTitle(page, 'climate');
  const climateSubtitle = await subtitleStyles(page);
  const climateHeadingWidth = await page.locator('main h1').evaluate((heading) => heading.getBoundingClientRect().width);
  assert.equal(await page.locator('main pre code').count(), 2);
  assert.equal(await page.locator('main canvas').count(), 0, 'The climate walkthrough is Markdown, not a second interactive app.');
  assert.equal(await page.getByRole('navigation', { name: 'Documentation guides', exact: true }).getByRole('link', { name: 'Climate', exact: true }).getAttribute('aria-current'), 'page');
  await openGuide(page, 'Geoscience');
  assert.equal(page.url(), base + 'examples/geoscience/');
  const geoscienceTitle = await exampleTitle(page, 'geoscience');
  assert.deepEqual(await subtitleStyles(page), climateSubtitle);
  assert.equal(await page.locator('main pre code').count(), 3);
  assert.equal(await page.locator('main #vegetation-from-landsat-bands').count(), 1);
  assert.equal(await page.locator('main a[href="https://data.source.coop/kentstephen/landsat-mosaics-new-england/README.md"]').count(), 1);
  assert.doesNotMatch(await page.locator('main').textContent(), /air_temperature|warmest locations/);
  await openGuide(page, 'Biology');
  assert.equal(page.url(), base + 'biology/');
  assert.equal(await page.getByRole('navigation', { name: 'Documentation guides', exact: true }).count(), 1, 'Biology must retain the docsite sidebar.');
  assert.deepEqual(await page.locator('#guide-nav a').evaluateAll((links) => links.map((link) => [link.textContent.trim(), link.href])), guideLinks);
  assert.equal(await biologyExample.getAttribute('aria-current'), 'page');
  assert.equal(await page.getByRole('searchbox', { name: 'Search documentation' }).count(), 1);
  assert.equal(await page.locator('.wordmark').count(), 1, 'Do not retain the standalone header alongside the shared header.');
  assert.equal(await page.locator('main').count(), 1, 'Do not nest the standalone document inside the docsite.');
  await guideClosing(page);
  const biologyTitle = await titleStyles(page);
  assert.deepEqual(climateTitle, biologyTitle, 'Climate must match biology typography and spacing, not just colors.');
  assert.deepEqual(geoscienceTitle, biologyTitle, 'Geoscience must match biology typography and spacing, not just colors.');
  assert.deepEqual(await subtitleStyles(page, '.intro-copy > p:last-child'), climateSubtitle, 'Biology introductory copy must match the other example pages.');
  assert.equal(await page.locator('.intro-copy > .eyebrow').count(), 0, 'Use the shared page label instead of an extra marketing label.');
  const biologyHeadingWidth = await page.locator('main h1').evaluate((heading) => heading.getBoundingClientRect().width);
  assert.ok(Math.abs(biologyHeadingWidth - climateHeadingWidth) < 1, 'The biology heading must not be squeezed alongside the index.');
  const index = page.locator('.contents');
  assert.deepEqual(await index.getByRole('link').evaluateAll((links) => links.map((link) => link.getAttribute('href'))), ['#anndata', '#ome']);
  assert.ok(await index.evaluate((element) => element.getBoundingClientRect().top >= document.querySelector('.intro-copy').getBoundingClientRect().bottom), 'Keep the index below the introduction.');
  assert.deepEqual(await navigationStyles(page), docsNavigation, 'The app stylesheet must not change shared navigation styles.');
  assert.equal(await page.getByRole('link', { name: 'Field guides', exact: true }).count(), 0);
  const appPalette = await page.evaluate(() => ({ color: getComputedStyle(document.documentElement).color, background: getComputedStyle(document.documentElement).backgroundColor, heading: getComputedStyle(document.querySelector('h1')).fontFamily }));
  assert.deepEqual(appPalette, docsPalette, 'The docs and app should retain the same palette and heading font.');
  assert.equal(await page.getByRole('link', { name: 'AnnData guide', exact: true }).getAttribute('href'), '../anndata/');
  assert.equal(await page.getByRole('link', { name: 'OME-Zarr guide', exact: true }).getAttribute('href'), '../ome-zarr/');
  for (const [section, credit, sources] of [
    ['anndata', 'Zheng et al. (2017)', [
      'https://scanpy.readthedocs.io/en/latest/api/generated/scanpy.datasets.pbmc68k_reduced.html',
      'https://doi.org/10.1038/ncomms14049',
    ]],
    ['ome', 'Blin et al. (2019)', [
      'https://idr.openmicroscopy.org/webclient/img_detail/6001240/',
      'https://doi.org/10.17867/10000125',
      'https://doi.org/10.1371/journal.pbio.3000388',
      'https://creativecommons.org/licenses/by/4.0/',
    ]],
  ]) {
    const source = page.locator(`#${section} .dataset-source`);
    assert.equal(await source.count(), 1, 'Dataset credits must be present without running SQL or loading remote data.');
    assert.ok((await source.textContent()).includes(credit));
    for (const href of sources) assert.equal(await source.locator(`a[href="${href}"]`).count(), 1);
  }
  if (!layoutOnly) {
    await page.waitForFunction(() => document.getElementById('ome-status')?.textContent.includes('plot updated') || document.getElementById('ome-status')?.classList.contains('error'), null, { timeout: 180_000 });
    const status = await page.locator('#ome-status').textContent();
    assert.ok(status.includes('35,175,800 rows') && status.includes('plot updated'), status);
    assert.ok((await page.locator('#anndata-status').textContent()).includes('700 rows'));
    assert.equal(await page.locator('#ome-plot .biology-visualization').getAttribute('data-channel-ids'), '0,1');
    assert.deepEqual(await page.getByRole('list', { name: 'Channel colors' }).getByRole('listitem').allTextContents(), ['LaminB1', 'Dapi']);
  }
  await page.getByRole('searchbox', { name: 'Search documentation' }).fill('Vegetation from Landsat bands');
  await page.locator('#search-results a').first().waitFor();
  assert.ok((await page.locator('#search-results a').first().getAttribute('href')).startsWith(base + 'examples/geoscience/'));
  await page.keyboard.press('Escape');
  assert.equal(await page.locator('#search-results').isHidden(), true);
  await page.screenshot({ path: join(evidence, 'biology-desktop.png'), fullPage: false });
  await page.setViewportSize({ width: 390, height: 844 });
  await guideClosing(page);
  const mobileBiologyTitle = await titleStyles(page);
  const mobileBiologySubtitle = await subtitleStyles(page, '.intro-copy > p:last-child');
  assert.deepEqual(await navigationStyles(page), mobileNavigation, 'Biology must use the same mobile navigation styles as the guides.');
  assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth), 'Shared navigation must not overflow the biology page on mobile.');
  const biologyGuides = page.getByRole('button', { name: 'Browse guides', exact: true });
  assert.equal(await biologyGuides.getAttribute('aria-expanded'), 'false');
  assert.equal(await page.locator('#guide-nav').isHidden(), true);
  await biologyGuides.click();
  assert.equal(await biologyGuides.getAttribute('aria-expanded'), 'true');
  assert.equal(await biologyExample.isVisible(), true);
  await page.screenshot({ path: join(evidence, 'biology-mobile-navigation.png'), fullPage: false });

  await openGuide(page, 'Climate');
  assert.equal(page.url(), base + 'examples/climate/');
  await page.setViewportSize({ width: 1440, height: 1100 });
  await openGuide(page, 'OME-Zarr');
  assert.equal(page.url(), base + 'ome-zarr/');
  await page.getByRole('navigation', { name: 'Project navigation', exact: true }).getByRole('link', { name: 'Documentation', exact: true }).click();
  assert.equal(page.url(), base);

  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto(base + 'anndata/');
  assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth));
  const guides = page.getByRole('button', { name: 'Browse guides', exact: true });
  await guides.click();
  assert.equal(await guides.getAttribute('aria-expanded'), 'true');
  await openGuide(page, 'OME-Zarr');
  assert.equal(page.url(), base + 'ome-zarr/');
  assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth));
  await page.screenshot({ path: join(evidence, 'docs-mobile.png'), fullPage: false });

  await openGuide(page, 'Climate');
  assert.equal(page.url(), base + 'examples/climate/');
  assert.deepEqual(await exampleTitle(page, 'climate'), mobileBiologyTitle);
  assert.deepEqual(await subtitleStyles(page), mobileBiologySubtitle);
  assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth));
  await openGuide(page, 'Geoscience');
  assert.equal(page.url(), base + 'examples/geoscience/');
  assert.deepEqual(await exampleTitle(page, 'geoscience'), mobileBiologyTitle);
  assert.deepEqual(await subtitleStyles(page), mobileBiologySubtitle);
  assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth));
  await page.goto(origin + '/');
  await projectLogos(page, true);
  assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth));
  await page.screenshot({ path: join(evidence, 'docs-home-mobile.png'), fullPage: false });
  assert.equal((await page.locator('main h1').textContent()).replace(/¶$/, ''), 'duckdb-zarr');
  for (const [label, path] of [['Climate', 'examples/climate/'], ['Geoscience', 'examples/geoscience/'], ['Biology', 'biology/']]) {
    const href = await page.locator('main').getByRole('link', { name: label, exact: true }).getAttribute('href');
    assert.equal(new URL(href, page.url()).href, origin + '/' + path);
  }
  await publicReference(page, origin + '/');
  await page.goto(base + 'domains/');
  assert.deepEqual((await page.locator('main h2').allTextContents()).slice(0, 3).map((title) => title.replace(/¶$/, '')), ['Biology', 'Climate', 'Geoscience']);
  assert.equal(await page.locator('main #climate-and-geoscience').count(), 1, 'Keep existing combined-section links working.');
  for (const domain of ['Biology', 'Climate', 'Geoscience']) {
    const link = page.locator('main').getByRole('link', { name: `${domain} field guide`, exact: true });
    assert.equal(await link.count(), 1);
  }
  assert.deepEqual(errors, []);
  if (layoutOnly) assert.deepEqual(externalRequests, [], 'Layout checks must not download scientific data.');
  console.log(layoutOnly
    ? 'Passed: layout-only sidebar parity, exact example title styles, search, mobile menus, root/project-prefix hosting. Scientific runtime was not started.'
    : 'Passed: docs search, direct field guides → live remote biology → format guide → docs, matched theme, mobile navigation, root and project-prefix hosting.');
  console.log(`Screenshot evidence: ${evidence}`);
} finally {
  await browser?.close();
  await new Promise((done) => server.close(done));
}
