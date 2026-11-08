import { test } from 'node:test';
import assert from 'node:assert/strict';
import { Int64, Uint64, tableFromArrays, vectorFromArray } from 'apache-arrow';
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

test('64-bit coordinates retain signed high words and sliced buffer offsets', () => {
  const offset = 2 ** 32;
  const table = tableFromArrays({
    c: new BigInt64Array([9n, 0n, 0n]),
    x: new BigInt64Array([9n, BigInt(offset), BigInt(offset + 1)]),
    y: new BigInt64Array([9n, -2n, -1n]),
    z: new BigInt64Array([9n, 0n, 0n]),
    value: new Uint16Array([99, 10, 20]),
  }).slice(1);
  const grid = buildVolumeGrid(table, { spacing: [1, 1, 1] });
  assert.deepEqual(grid.minimum, [offset, -2, 0]);
  assert.deepEqual(grid.dimensions, [2, 2, 1]);
  assert.deepEqual([...grid.data], [0.5, 0, 0, 1]);
  assert.equal(grid.count, 2);
});

test('unsigned 64-bit coordinates reject unsafe integers without truncation', () => {
  const table = tableFromArrays({
    c: new BigUint64Array([1n, 1n, 1n]),
    x: new BigUint64Array([0xffffffffn, 0x100000000n, 0xffffffffffffffffn]),
    y: new BigUint64Array([0n, 0n, 0n]),
    z: new BigUint64Array([0n, 0n, 0n]),
    value: new Uint16Array([10, 20, 30]),
  });
  const grid = buildVolumeGrid(table, { spacing: [1, 1, 1] });
  assert.deepEqual(grid.minimum, [0xffffffff, 0, 0]);
  assert.deepEqual([...grid.data], [0.5, 1]);
  assert.equal(grid.count, 2);
  assert.equal(grid.skipped, 1);
});

test('nullable coordinates and intensities stay absent rather than becoming zero', () => {
  const table = tableFromArrays({
    c: vectorFromArray([0n, 0n, 0n, null], new Int64()),
    x: vectorFromArray([0n, 1n, null, 3n], new Int64()),
    y: new BigInt64Array(4), z: new BigInt64Array(4), value: [10, null, 20, 30],
  });
  const grid = buildVolumeGrid(table, { spacing: [1, 1, 1] });
  assert.deepEqual([...grid.data], [1]);
  assert.equal(grid.count, 1);
  assert.equal(grid.skipped, 3);
});

for (const [name, ArrayType, first, unsafe] of [
  ['positive signed', BigInt64Array, BigInt(Number.MAX_SAFE_INTEGER) - 1n, BigInt(Number.MAX_SAFE_INTEGER) + 1n],
  ['negative signed', BigInt64Array, -BigInt(Number.MAX_SAFE_INTEGER), -BigInt(Number.MAX_SAFE_INTEGER) - 1n],
  ['unsigned', BigUint64Array, BigInt(Number.MAX_SAFE_INTEGER) - 1n, BigInt(Number.MAX_SAFE_INTEGER) + 1n],
]) {
  test(`${name} coordinates preserve the safe-integer boundary`, () => {
    const table = tableFromArrays({
      c: new BigInt64Array(3), x: new ArrayType([first, first + 1n, unsafe]),
      y: new BigInt64Array(3), z: new BigInt64Array(3), value: new Uint16Array([10, 20, 30]),
    });
    const grid = buildVolumeGrid(table, { spacing: [1, 1, 1] });
    assert.deepEqual(grid.minimum, [Number(first), 0, 0]);
    assert.deepEqual([...grid.data], [0.5, 1]);
    assert.equal(grid.count, 2);
    assert.equal(grid.skipped, 1);
  });
}

test('sliced nullable unsigned 64-bit columns keep the validity bitmap', () => {
  const table = tableFromArrays({
    c: new BigUint64Array([9n, 1n, 1n, 1n]),
    x: vectorFromArray([99n, 2n, null, 4n], new Uint64()),
    y: new BigUint64Array(4), z: new BigUint64Array(4), value: [99, 10, 20, null],
  }).slice(1);
  const grid = buildVolumeGrid(table, { spacing: [1, 1, 1] });
  assert.deepEqual(grid.minimum, [2, 0, 0]);
  assert.deepEqual(grid.channelIds, [1]);
  assert.deepEqual([...grid.data], [1]);
  assert.equal(grid.count, 1);
  assert.equal(grid.skipped, 2);
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
