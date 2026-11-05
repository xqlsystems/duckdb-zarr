import { test } from 'node:test';
import assert from 'node:assert/strict';
import { omeObjectKeys, prefetchOME } from '../src/prefetch_ome.js';
import { fixtureStore, readerFixture } from './fixture-store.js';

// Actual IDR source metadata, also checked by the live browser test.
const source = { level: '0', shape: [2, 236, 275, 271], chunks: [1, 1, 275, 271], dimension_separator: '/' };
test('prefetch enumerates exactly both original full-resolution channel chunk grids', () => {
  const keys = omeObjectKeys(source);
  assert.equal(keys.length, 475);
  assert.equal(new Set(keys).size, keys.length);
  assert.deepEqual(keys.slice(0, 3), ['.zattrs', '.zgroup', '0/.zarray']);
  assert.ok(keys.includes('0/0/0/0/0'));
  assert.ok(keys.includes('0/1/235/0/0'));
  assert.ok(!keys.includes('0/2/0/0/0'));
});
test('chunk keys honor declared dot separators and partial boundary chunks', () => {
  // Algorithm fixture only; not a dataset substituted for the real demo.
  const keys = omeObjectKeys({ level: '1', shape: [2, 3, 4, 5], chunks: [1, 2, 3, 4], dimension_separator: '.' });
  assert.equal(keys.length, 19);
  assert.ok(keys.includes('1/1.1.1.1'));
});
test('invalid or excessive grids are refused before fetching anything', () => {
  for (const fields of [{ level: '../0' }, { chunks: [1, 0, 1, 1] }, { dimension_separator: ':' }, { chunks: [1, 1, 1, 1] }]) {
    assert.throws(() => omeObjectKeys({ ...source, ...fields }));
  }
});

test('an omitted fill-value chunk is not retried or registered as fabricated bytes', async (t) => {
  const store = await fixtureStore();
  t.after(() => store.close());
  const buffers = new Map();
  const db = {
    registerFileBuffer: async (name, data) => { buffers.set(name, Buffer.from(data)); },
    dropFile: async (name) => { buffers.delete(name); },
  };
  const progress = [];
  const handle = await prefetchOME(db, { ...readerFixture.metadata, url: store.url }, { concurrency: 1, onProgress: (message) => progress.push(message) });
  const missing = readerFixture.expected.missing_chunk;
  assert.equal(store.requests.filter((key) => key === missing).length, 1);
  assert.equal(buffers.has(`${store.url}/${missing}`), false);
  assert.equal(handle.objectCount, buffers.size);
  assert.equal(handle.missingChunkCount, 1);
  assert.equal(handle.bytes, [...buffers.values()].reduce((bytes, data) => bytes + data.length, 0));
  assert.match(progress.at(-1), /5\/5 objects/);
  await handle.dispose();
  assert.equal(buffers.size, 0);
});

/** Observe registered bytes without replacing the prefetcher's real HTTP requests. */
function bufferRegistry() {
  const buffers = new Map();
  return {
    buffers,
    registerFileBuffer: async (name, data) => { buffers.set(name, Buffer.from(data)); },
    dropFile: async (name) => { buffers.delete(name); },
  };
}

for (const key of ['.zattrs', '.zgroup', '0/.zarray']) {
  test(`a metadata 404 for ${key} remains fatal and cleans up prior registrations`, async (t) => {
    const store = await fixtureStore({ [key]: 404 });
    t.after(() => store.close());
    const db = bufferRegistry();
    await assert.rejects(prefetchOME(db, { ...readerFixture.metadata, url: store.url }, { concurrency: 1 }), /HTTP 404/);
    assert.equal(store.requests.filter((request) => request === key).length, 3);
    assert.equal(db.buffers.size, 0);
  });
}

for (const failure of [503, 'network-error']) {
  test(`a chunk ${failure} still fails rather than becoming an omitted fill chunk`, async (t) => {
    const key = '0/0/1/0/0';
    const store = await fixtureStore({ [key]: failure });
    t.after(() => store.close());
    const db = bufferRegistry();
    await assert.rejects(prefetchOME(db, { ...readerFixture.metadata, url: store.url }, { concurrency: 1 }));
    assert.ok(store.requests.filter((request) => request === key).length >= 3);
    assert.equal(db.buffers.size, 0);
  });
}

test('progress completes even when the final chunk is absent', async (t) => {
  const store = await fixtureStore({ '0/0/1/0/0': 404 });
  t.after(() => store.close());
  const db = bufferRegistry();
  const progress = [];
  const handle = await prefetchOME(db, { ...readerFixture.metadata, url: store.url }, { concurrency: 1, onProgress: (message) => progress.push(message) });
  assert.equal(handle.missingChunkCount, 2);
  assert.equal(handle.objectCount, 3);
  assert.match(progress.at(-1), /5\/5 objects checked/);
  assert.deepEqual([...db.buffers.keys()].map((url) => url.slice(store.url.length + 1)), ['.zattrs', '.zgroup', '0/.zarray']);
  await handle.dispose();
});
