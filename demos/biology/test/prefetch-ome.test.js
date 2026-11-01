import { test } from 'node:test';
import assert from 'node:assert/strict';
import { omeObjectKeys } from '../src/prefetch_ome.js';

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
