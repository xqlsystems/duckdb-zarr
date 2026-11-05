import { test } from 'node:test';
import assert from 'node:assert/strict';
import { tableFromArrays } from 'apache-arrow';
import { buildVolumeGrid } from '../src/volume_grid.js';

// Deliberately asymmetric algorithm fixture, not a demo dataset. SQL gaps must
// stay empty and channel IDs must survive shuffled rows and unequal intensities.
const fixture = () => tableFromArrays({
  c: new Int32Array([1, 0, 1, 0]),
  x: new Int32Array([4, 2, 2, 4]),
  y: new Int32Array([3, 1, 1, 3]),
  z: new Int32Array([8, 6, 6, 8]),
  value: new Float32Array([200, 10, 100, 40]),
});

test('Arrow results reconstruct all channels in one physically spaced grid', () => {
  const grid = buildVolumeGrid(fixture(), { spacing: [0.3, 0.4, 0.5] });
  assert.deepEqual(grid.channelIds, [0, 1]);
  assert.deepEqual(grid.dimensions, [3, 3, 3]);
  assert.deepEqual(grid.minimum, [2, 1, 6]);
  assert.deepEqual(grid.physicalSize, [0.8999999999999999, 1.2000000000000002, 1.5]);
  assert.equal(grid.data.length, 3 * 3 * 3 * 2);
  assert.equal(grid.data[0], 0.25);
  assert.equal(grid.data[1], 0.5);
  assert.equal(grid.data.at(-2), 1);
  assert.equal(grid.data.at(-1), 1);
  assert.equal(grid.data[2], 0, 'an absent X coordinate is not compacted away');
  assert.equal(grid.count, 4);
  assert.deepEqual(grid.statistics.map((s) => s.maximum), [40, 200]);
  assert.deepEqual(grid.statistics.map((s) => s.sum), [50, 300]);
});

test('a single selected channel keeps its real ID rather than becoming channel zero', () => {
  const table = tableFromArrays({ c: new Int32Array([1, 1]), x: [2, 3], y: [0, 0], z: [0, 0], value: [100, 200] });
  const grid = buildVolumeGrid(table, { spacing: [1, 1, 1] });
  assert.deepEqual(grid.channelIds, [1]);
  assert.deepEqual([...grid.data], [0.5, 1]);
});

test('mixed channels without c are rejected rather than silently merged', () => {
  assert.throws(() => buildVolumeGrid(tableFromArrays({ x: [0], y: [0], z: [0], value: [1] }), { spacing: [1, 1, 1] }), /channel.*c/i);
});

test('unsafe bounds, non-finite spacing, and unsupported channel counts are rejected before allocation', () => {
  assert.throws(() => buildVolumeGrid(fixture(), { spacing: [1, 0, 1] }), /spacing/i);
  const enormous = tableFromArrays({ c: [0, 0], x: [0, 999999], y: [0, 0], z: [0, 0], value: [1, 1] });
  assert.throws(() => buildVolumeGrid(enormous, { spacing: [1, 1, 1] }), /safety limit/i);
  const tooMany = tableFromArrays({ c: [0, 1, 2, 3, 4], x: [0, 0, 0, 0, 0], y: [0, 0, 0, 0, 0], z: [0, 0, 0, 0, 0], value: [1, 1, 1, 1, 1] });
  assert.throws(() => buildVolumeGrid(tooMany, { spacing: [1, 1, 1] }), /four channels/i);
});
