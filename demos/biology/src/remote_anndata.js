// The sample is fetched from Scanpy, never from this site's public/data tree.
// Its ZIP entries remain byte-for-byte Zarr v3 files: DuckDB's extension, not
// JavaScript, decodes AnnData, categorical labels, sharding and Zstd.
export const PBMC_SOURCE_URL = 'https://raw.githubusercontent.com/scverse/scanpy/1a421f222c65987e010ccd395d6065cf5a9682ce/src/scanpy/datasets/10x_pbmc68k_reduced.zarr.zip';
export const PBMC_SOURCE_SHA256 = '1afd91a3e8edca5f7a5ecb9c6caed1a471b4046f51e6ad033677e42ffac11358';
const MAX_BYTES = 64 * 1024 * 1024;
const text = new TextDecoder('utf-8', { fatal: true });
const sqlString = (value) => `'${String(value).replaceAll("'", "''")}'`;

/**
 * Read an integrity-checked, uncompressed ZIP (the pinned Scanpy archive).
 * ZIP central-directory order is significant: Scanpy writes repeated metadata
 * entries; the LAST version of each key is authoritative, as in Python ZipFile.
 * This deliberately rejects compressed/encrypted/ZIP64 archives rather than
 * adding an unpinned decompressor or silently misreading an unsupported ZIP.
 */
export function unpackStoredZip(bytes) {
  if (!(bytes instanceof Uint8Array) || bytes.byteLength > MAX_BYTES) throw new Error('Invalid or oversized AnnData ZIP.');
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const range = (offset, size) => {
    if (offset < 0 || size < 0 || offset + size > bytes.byteLength) throw new Error('Truncated AnnData ZIP.');
  };
  const u16 = (offset) => { range(offset, 2); return view.getUint16(offset, true); };
  const u32 = (offset) => { range(offset, 4); return view.getUint32(offset, true); };
  let end = bytes.byteLength - 22;
  for (; end >= Math.max(0, bytes.byteLength - 65557); end--) {
    if (u32(end) === 0x06054b50 && end + 22 + u16(end + 20) === bytes.byteLength) break;
  }
  if (end < Math.max(0, bytes.byteLength - 65557)) throw new Error('Missing ZIP directory.');
  if (u16(end + 4) || u16(end + 6) || u16(end + 8) !== u16(end + 10)) throw new Error('Split ZIP archives are unsupported.');
  const count = u16(end + 10), directorySize = u32(end + 12), directoryStart = u32(end + 16);
  if (count === 65535 || directoryStart === 0xffffffff || directorySize === 0xffffffff) throw new Error('ZIP64 archives are unsupported.');
  range(directoryStart, directorySize);
  if (directoryStart + directorySize !== end) throw new Error('Invalid ZIP directory size.');
  const files = new Map();
  let offset = directoryStart, total = 0;
  for (let entry = 0; entry < count; entry++) {
    range(offset, 46);
    if (u32(offset) !== 0x02014b50) throw new Error('Invalid ZIP directory entry.');
    const flags = u16(offset + 8), method = u16(offset + 10);
    const size = u32(offset + 24), compressedSize = u32(offset + 20);
    const nameSize = u16(offset + 28), extraSize = u16(offset + 30), commentSize = u16(offset + 32);
    const local = u32(offset + 42);
    range(offset + 46, nameSize + extraSize + commentSize);
    const name = text.decode(bytes.subarray(offset + 46, offset + 46 + nameSize));
    if (!name || name.startsWith('/') || /[\\\x00:]/.test(name) || name.split('/').some((part) => part === '..' || part === '.')) throw new Error('Unsafe AnnData ZIP path.');
    if (flags & 1 || method !== 0 || size !== compressedSize) throw new Error('Only unencrypted, stored ZIP entries are supported.');
    range(local, 30);
    if (u32(local) !== 0x04034b50 || u16(local + 8) !== method) throw new Error('Invalid ZIP local header.');
    const start = local + 30 + u16(local + 26) + u16(local + 28);
    range(start, size);
    if (start + size > directoryStart) throw new Error('ZIP entry overlaps its directory.');
    total += size;
    if (total > MAX_BYTES) throw new Error('AnnData ZIP exceeds the in-memory size limit.');
    if (!name.endsWith('/')) files.set(name, bytes.subarray(start, start + size));
    offset += 46 + nameSize + extraSize + commentSize;
  }
  if (offset !== directoryStart + directorySize) throw new Error('Invalid ZIP entry count.');
  return files;
}

/**
 * Fetch a remote AnnData store. Call after db.instantiate/open; keep the returned
 * handle alive while SQL views use handle.storeURL. await handle.dispose() only
 * after dropping/replacing those views. No IndexedDB, OPFS, Cache API, service
 * worker, server proxy, persistent filesystem, or prepared site payload is used.
 *
 * HTTPS directory URLs go directly to read_zarr (server must support CORS).
 * ZIP URLs require SHA-256 integrity; the pinned sample supplies its known hash.
 * Each load gets a unique virtual directory, so changing sources is isolated.
 */
export async function loadRemoteAnnData(db, {
  url = PBMC_SOURCE_URL,
  sha256 = url === PBMC_SOURCE_URL ? PBMC_SOURCE_SHA256 : undefined,
  signal = AbortSignal.timeout(30_000),
  onProgress = () => {},
} = {}) {
  const source = new URL(url);
  if (!['https:', 'http:'].includes(source.protocol)) throw new Error('AnnData source must be an HTTP(S) URL.');
  if (source.username || source.password || source.search || source.hash) throw new Error('Use a public AnnData URL without credentials, query parameters or fragments.');
  const sourceURL = source.href.replace(/\/$/, '');
  if (!source.pathname.endsWith('.zip')) {
    return Object.freeze({ sourceURL, storeURL: sourceURL, transport: 'remote-directory', dispose: async () => {} });
  }
  if (!/^[a-f0-9]{64}$/.test(sha256 ?? '')) throw new Error('A SHA-256 digest is required for an AnnData ZIP.');
  onProgress('Downloading the original Scanpy AnnData ZIP into browser memory…');
  const response = await fetch(sourceURL, { mode: 'cors', credentials: 'omit', cache: 'no-store', signal });
  if (!response.ok) throw new Error(`AnnData download failed: HTTP ${response.status}.`);
  if (Number(response.headers.get('content-length')) > MAX_BYTES) throw new Error('AnnData ZIP exceeds the in-memory size limit.');
  const bytes = new Uint8Array(await response.arrayBuffer());
  if (bytes.byteLength > MAX_BYTES) throw new Error('AnnData ZIP exceeds the in-memory size limit.');
  const digest = [...new Uint8Array(await crypto.subtle.digest('SHA-256', bytes))].map((v) => v.toString(16).padStart(2, '0')).join('');
  if (digest !== sha256) throw new Error(`AnnData ZIP SHA-256 mismatch: ${digest}.`);
  const files = unpackStoredZip(bytes);
  const json = (path) => {
    if (!files.has(path)) throw new Error(`AnnData ZIP is missing ${path}.`);
    return JSON.parse(text.decode(files.get(path)));
  };
  const root = json('zarr.json');
  if (root.zarr_format !== 3 || root.attributes?.['encoding-type'] !== 'anndata') throw new Error('Expected a Zarr v3 AnnData ZIP.');
  const shape = json('X/zarr.json').shape;
  const embeddingShape = json('obsm/X_umap/zarr.json').shape;
  const annotationColumns = json('obs/zarr.json').attributes?.['column-order'];
  if (!annotationColumns?.includes('bulk_labels') || embeddingShape?.[0] !== shape?.[0] || embeddingShape?.[1] !== 2) throw new Error('Expected AnnData bulk_labels and a two-component X_umap.');
  // The extension's array discovery treats plain paths as host directories
  // (std::fs::read_dir cannot enumerate DuckDB's registered-file namespace).
  // A reserved .invalid HTTPS namespace selects consolidated-metadata discovery;
  // every real entry is registered below, so data reads resolve in memory.
  const storeURL = `https://duckdb-wasm-memory.invalid/remote-pbmc-${crypto.randomUUID()}.zarr`;
  const registered = new Set();
  const dispose = async () => {
    for (const name of [...registered]) {
      try { await db.dropFile(name); }
      catch (error) { throw new Error(`Could not release AnnData memory file ${name}: ${error.message || error}`, {cause:error}); }
      registered.delete(name);
    }
  };
  onProgress('Registering original Zarr files in DuckDB-Wasm memory…');
  try {
    for (const [key, data] of files) {
      signal.throwIfAborted();
      const name = `${storeURL}/${key}`;
      // AsyncDuckDB transfers/detaches buffers. Copy each entry so registering
      // one never detaches the complete archive or its other entries.
      await db.registerFileBuffer(name, data.slice());
      registered.add(name);
    }
  } catch (error) {
    try { await dispose(); } catch (cleanupError) { throw new AggregateError([error, cleanupError], 'AnnData registration and cleanup failed.'); }
    throw error;
  }
  return Object.freeze({
    sourceURL, storeURL, transport: 'remote-zip-memory', sha256: digest,
    archiveBytes: bytes.byteLength, fileCount: files.size, zarrFormat: root.zarr_format,
    shape: Object.freeze([...shape]), embeddingShape: Object.freeze([...embeddingShape]),
    annotation: 'bulk_labels', embedding: 'X_umap', dispose,
  });
}

/** Existing scatterSQL/statistics/filter code can retain the cell_type alias. */
export function remoteAnnDataViewSQL(storeURL) {
  const store = sqlString(storeURL);
  return [
    `CREATE OR REPLACE VIEW pbmc_obs AS SELECT *, bulk_labels AS cell_type FROM read_zarr(${store}, table_name := 'obs')`,
    `CREATE OR REPLACE VIEW pbmc_coordinates AS SELECT * FROM read_zarr(${store}, group_path := 'obsm', table_name := 'obs_X_umap_component')`,
  ];
}
