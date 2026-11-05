// Metadata are fetched from the upstream OME-NGFF store, never a bundled copy.
export const OME_URL = 'https://uk1s3.embassy.ebi.ac.uk/idr/zarr/v0.4/idr0062A/6001240.zarr';

/** Fetch original JSON metadata with a deadline and explicit HTTP status failures. */
async function json(url) {
  const response = await fetch(url, { cache: 'no-store', signal: AbortSignal.timeout(30_000) });
  if (!response.ok) throw new Error(`Remote metadata returned HTTP ${response.status}: ${url}`);
  return response.json();
}

/** Validate a public CZYX OME-NGFF level and return original chunking, channel colors and XYZ
 * micrometer spacing within browser budgets.
 */
export async function loadOME(url, { level = '0' } = {}) {
  const source = new URL(url);
  if (!['https:', 'http:'].includes(source.protocol) || source.username || source.password) throw new Error('Use a public HTTP(S) Zarr URL without credentials.');
  source.pathname = source.pathname.replace(/\/+$/, '');
  if (source.search || source.hash) throw new Error('Use a Zarr directory URL without a query string or fragment.');
  const base = source.href;
  const attrs = await json(`${base}/.zattrs`);
  const multiscale = attrs.multiscales?.[0];
  if (!multiscale) throw new Error('This example needs OME-NGFF multiscale metadata.');
  const selected = multiscale.datasets.find((item) => item.path === level);
  if (!selected) throw new Error(`The source has no resolution level ${level}.`);
  const array = await json(`${base}/${level}/.zarray`);
  const axes = multiscale.axes.map((axis) => typeof axis === 'string' ? axis : axis.name);
  if (axes.join(',') !== 'c,z,y,x' || array.shape.length !== 4) throw new Error('This example expects c,z,y,x image axes.');
  if (array.shape.slice(1).some((value) => !Number.isSafeInteger(value) || value < 1 || value > 512) || array.shape.slice(1).reduce((total, value) => total * value, 1) > 20_000_000 || array.shape.reduce((total, value) => total * value, 1) * 4 > 256 * 1024 * 1024) throw new Error('Source level 0 exceeds this browser volume budget; choose a smaller image.');
  const spatialAxes = multiscale.axes.slice(1);
  if (spatialAxes.some((axis) => typeof axis !== 'object' || axis.unit !== 'micrometer')) throw new Error('This viewer requires explicit micrometer spatial units.');
  const transformations = [...(multiscale.coordinateTransformations ?? []), ...(selected.coordinateTransformations ?? [])];
  const scale = transformations.filter((item) => item.type === 'scale').reduce((combined, item) => combined.map((value, index) => value * item.scale[index]), [1, 1, 1, 1]);
  if (scale.some((value) => !Number.isFinite(value) || value <= 0)) throw new Error('Invalid OME-NGFF physical scale.');
  const metadataChannels = attrs.omero?.channels;
  if (!metadataChannels || metadataChannels.length !== array.shape[0] || metadataChannels.length > 4) throw new Error('Provide source channel names/colors for at most four channels.');
  for (const channel of metadataChannels) if (!/^[0-9a-f]{6}$/i.test(channel.color)) throw new Error('A source channel is missing its six-digit display color.');
  return {
    url: base, level, shape: array.shape, chunks: array.chunks,
    dimension_separator: array.dimension_separator ?? '.',
    spacing: [scale[3], scale[2], scale[1]],
    channels: metadataChannels.map((channel) => channel.label),
    channel_colors: metadataChannels.map((channel) => `#${channel.color.toLowerCase()}`),
    compressor: array.compressor,
    description: 'Original remote OME-Zarr chunks; no dataset is packaged with this site.',
  };
}
