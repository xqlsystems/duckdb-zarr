// Reconstruct an interleaved multi-channel texture from actual Arrow results.
// Iterate one record batch at a time; never materialize millions of row objects.
const MAX_ROWS = 40_000_000;
const MAX_DIMENSION = 512;
const MAX_VOXELS = 20_000_000;
const MAX_TEXTURE_BYTES = 256 * 1024 * 1024;
const HISTOGRAM_BINS = 4096;
const LITTLE_ENDIAN = new Uint8Array(new Uint16Array([1]).buffer)[0] === 1;

/** Read Arrow numbers without allocating a temporary BigInt for each 64-bit value.
 * Keep both words: truncating to 32 bits would silently move large coordinates.
 */
function numberAt(column) {
  if (column.nullCount) return (row) => column.get(row);
  const values = column.toArray();
  if (LITTLE_ENDIAN && (values instanceof BigInt64Array || values instanceof BigUint64Array)) {
    const view = new DataView(values.buffer, values.byteOffset, values.byteLength);
    const high = values instanceof BigInt64Array
      ? (offset) => view.getInt32(offset, true)
      : (offset) => view.getUint32(offset, true);
    return (row) => view.getUint32(row * 8, true) + high(row * 8 + 4) * 0x100000000;
  }
  return (row) => values[row];
}

/** Visit finite Arrow coordinates batchwise, preserving SQL null gaps and clamping negative
 * signal only for display.
 */
function visit(table, callback) {
  for (const batch of table.batches) {
    const [channelAt, xAt, yAt, zAt, valueAt] = ['c', 'x', 'y', 'z', 'value'].map((name) => numberAt(batch.getChild(name)));
    for (let row = 0; row < batch.numRows; row++) {
      const rawC = channelAt(row), rawX = xAt(row), rawY = yAt(row), rawZ = zAt(row), rawValue = valueAt(row);
      if (rawC == null || rawX == null || rawY == null || rawZ == null || rawValue == null) continue;
      const c = Number(rawC), x = Number(rawX), y = Number(rawY), z = Number(rawZ), value = Number(rawValue);
      if (!Number.isSafeInteger(c) || c < 0 || !Number.isSafeInteger(x) || !Number.isSafeInteger(y) || !Number.isSafeInteger(z) || !Number.isFinite(value)) continue;
      callback(c, x, y, z, Math.max(0, value));
    }
  }
}

/** Estimate a normalized nonzero-signal quantile from a fixed histogram without sorting millions
 * of values.
 */
function quantile(histogram, samples, fraction) {
  if (!samples) return 0;
  const target = Math.max(1, Math.ceil(samples * fraction));
  let count = 0;
  for (let bin = 0; bin < histogram.length; bin++) {
    count += histogram[bin];
    if (count >= target) return bin / (histogram.length - 1);
  }
  return 1;
}

/** Reconstruct bounded interleaved channel textures from Arrow batches; preserve coordinate gaps
 * and physical spacing, and return display windows plus geometry statistics.
 */
export function buildVolumeGrid(table, { spacing }) {
  if (!table.getChild('c')) throw new Error('Return the channel column c alongside x, y, z, value.');
  for (const name of ['x', 'y', 'z', 'value']) if (!table.getChild(name)) throw new Error(`Missing volume column ${name}.`);
  if (!Array.isArray(spacing) || spacing.length !== 3 || spacing.some((value) => !Number.isFinite(value) || value <= 0)) throw new Error('Voxel spacing must contain three positive finite numbers.');
  if (table.numRows > MAX_ROWS) throw new Error(`Volume safety limit: ${MAX_ROWS.toLocaleString()} rows. Narrow the SQL selection.`);
  const minimum = [Infinity, Infinity, Infinity];
  const maximum = [-Infinity, -Infinity, -Infinity];
  const byChannel = new Map();
  let count = 0;
  visit(table, (c, x, y, z, value) => {
    if (!byChannel.has(c)) {
      if (byChannel.size === 4) throw new Error('The viewer supports at most four channels at once.');
      byChannel.set(c, { id: c, maximum: 0, sum: 0, count: 0, nonzero: 0, histogram: new Uint32Array(HISTOGRAM_BINS) });
    }
    const stats = byChannel.get(c);
    stats.maximum = Math.max(stats.maximum, value);
    stats.sum += value;
    stats.count++;
    count++;
    minimum[0] = Math.min(minimum[0], x); maximum[0] = Math.max(maximum[0], x);
    minimum[1] = Math.min(minimum[1], y); maximum[1] = Math.max(maximum[1], y);
    minimum[2] = Math.min(minimum[2], z); maximum[2] = Math.max(maximum[2], z);
  });
  if (!count) throw new Error('No finite SQL voxels to display.');
  const dimensions = minimum.map((value, index) => maximum[index] - value + 1);
  const voxels = dimensions.reduce((total, value) => total * value, 1);
  const channelIds = [...byChannel.keys()].sort((a, b) => a - b);
  const components = channelIds.length;
  if (dimensions.some((value) => !Number.isSafeInteger(value) || value < 1 || value > MAX_DIMENSION) || voxels > MAX_VOXELS || voxels * components * 4 > MAX_TEXTURE_BYTES) throw new Error(`Selected bounds ${dimensions.join(' × ')} exceed the volume safety limit. Narrow SQL coordinate bounds; gaps are not collapsed.`);
  const componentFor = new Map(channelIds.map((id, index) => [id, index]));
  const data = new Float32Array(voxels * components);
  visit(table, (c, x, y, z, value) => {
    const stats = byChannel.get(c);
    const normalized = stats.maximum ? value / stats.maximum : 0;
    const index = (x - minimum[0] + dimensions[0] * (y - minimum[1] + dimensions[1] * (z - minimum[2]))) * components + componentFor.get(c);
    data[index] = Math.max(data[index], normalized);
    if (value > 0) {
      stats.histogram[Math.min(HISTOGRAM_BINS - 1, Math.floor(normalized * (HISTOGRAM_BINS - 1)))]++;
      stats.nonzero++;
    }
  });
  const statistics = channelIds.map((id) => {
    const { histogram, nonzero, ...stats } = byChannel.get(id);
    // Display-only background suppression. The original intensity values, SQL
    // results and geometry remain unchanged. Per-channel windows preserve hue.
    const low = quantile(histogram, nonzero, 0.7);
    const high = Math.max(low + 1 / (HISTOGRAM_BINS - 1), quantile(histogram, nonzero, 0.995));
    return { ...stats, low, high };
  });
  const physicalSize = dimensions.map((value, index) => value * spacing[index]);
  const longest = Math.max(...physicalSize);
  const scale = physicalSize.map((value) => value / longest);
  if (scale.some((value) => !Number.isFinite(value) || value < 1e-6)) throw new Error('Voxel spacing aspect ratio is too extreme to render safely.');
  return { data, dimensions, minimum, spacing, physicalSize, scale, channelIds, statistics, count, skipped: table.numRows - count };
}
