// Fetch original compressed Zarr objects in parallel, then register their exact
// HTTPS URLs with DuckDB-Wasm. read_zarr still decodes the original source.
// This is ephemeral browser RAM, not a repacked or site-hosted dataset.
const MAX_OBJECTS = 20_000;
const MAX_BYTES = 256 * 1024 * 1024;

/** Enumerate v2 metadata and coordinate chunk keys using source shape, chunking and separator;
 * reject oversized grids.
 */
export function omeObjectKeys({ level, shape, chunks, dimension_separator: separator = '.' }) {
  if (!/^\d+$/.test(level) || ![3, 4].includes(shape?.length) || shape.length !== chunks?.length || !['.', '/'].includes(separator)) throw new Error('Unsupported OME-Zarr chunk layout.');
  const grid = shape.map((size, i) => {
    if (!Number.isSafeInteger(size) || size <= 0 || !Number.isSafeInteger(chunks[i]) || chunks[i] <= 0) throw new Error('Invalid OME-Zarr shape/chunks.');
    return Math.ceil(size / chunks[i]);
  });
  if (grid.reduce((n, size) => n * size, 1) > MAX_OBJECTS) throw new Error('This source has too many chunks to cache safely.');
  const keys = ['.zattrs', '.zgroup', `${level}/.zarray`];
  /** Walk the declared Cartesian chunk grid without inferring object presence. */
  const walk = (indices) => {
    if (indices.length === grid.length) { keys.push(`${level}/${indices.join(separator)}`); return; }
    for (let index = 0; index < grid[indices.length]; index++) walk([...indices, index]);
  };
  walk([]);
  return keys;
}

/** Recognize only in-grid-style v2 coordinate keys, never metadata names. */
function isChunkKey(key, metadata) {
  const prefix = `${metadata.level}/`;
  if (!key.startsWith(prefix)) return false;
  const coordinates = key.slice(prefix.length).split(metadata.dimension_separator ?? '.');
  return coordinates.length === metadata.shape.length && coordinates.every((value) => /^\d+$/.test(value));
}

/** Cache original objects in RAM; omitted chunks stay absent for read_zarr's fill handling. */
export async function prefetchOME(db, metadata, { onProgress = () => {}, signal, concurrency = 12 } = {}) {
  const keys = omeObjectKeys(metadata);
  const controller = new AbortController();
  const combined = AbortSignal.any([controller.signal, AbortSignal.timeout(120_000), ...(signal ? [signal] : [])]);
  const registered = new Set();
  let next = 0, completed = 0, bytes = 0, missingChunks = 0;
  /** Drop registered buffers, retaining any failed name so cleanup can be retried. */
  const dispose = async () => {
    for (const url of [...registered]) {
      await db.dropFile(url);
      registered.delete(url);
    }
  };
  /** Fetch original objects with bounded retries; omit only chunk-key 404s and propagate all
   * other failures.
   */
  const worker = async () => {
    while (next < keys.length) {
      combined.throwIfAborted();
      const key = keys[next++], url = `${metadata.url}/${key}`;
      let data;
      for (let attempt = 0; attempt < 3; attempt++) {
        try {
          const response = await fetch(url, { cache: 'no-store', credentials: 'omit', signal: AbortSignal.any([combined, AbortSignal.timeout(20_000)]) });
          if (response.status === 404 && isChunkKey(key, metadata)) {
            data = null;
            missingChunks++;
            break;
          }
          if (!response.ok) throw new Error(`HTTP ${response.status} fetching ${url}`);
          data = new Uint8Array(await response.arrayBuffer());
          break;
        } catch (error) {
          combined.throwIfAborted();
          if (attempt === 2) throw error;
        }
      }
      combined.throwIfAborted();
      if (data !== null) {
        bytes += data.byteLength;
        if (bytes > MAX_BYTES) throw new Error('Remote volume exceeds the browser memory download budget.');
        await db.registerFileBuffer(url, data);
        registered.add(url);
      }
      completed++;
      if (completed % 16 === 0 || completed === keys.length) onProgress(`Loading original remote image: ${completed}/${keys.length} objects checked · ${(bytes / 1024 / 1024).toFixed(1)} MB in browser memory…`);
    }
  };
  const tasks = Array.from({ length: Math.max(1, Math.min(12, concurrency, keys.length)) }, worker);
  try {
    await Promise.all(tasks);
  } catch (error) {
    controller.abort(error);
    await Promise.allSettled(tasks);
    await dispose();
    throw error;
  }
  return { sourceURL: metadata.url, objectCount: registered.size, missingChunkCount: missingChunks, bytes, dispose };
}
