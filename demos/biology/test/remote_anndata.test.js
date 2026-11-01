import test from 'node:test';
import assert from 'node:assert/strict';
import { unpackStoredZip, loadRemoteAnnData, remoteAnnDataViewSQL, PBMC_SOURCE_URL, PBMC_SOURCE_SHA256 } from '../src/remote_anndata.js';

// ZIP container fixtures only: no biological values, annotations or SQL results.
function storedZip(entries) {
  const parts = [], directory = [];
  let offset = 0;
  for (const [name, value] of entries) {
    const nameBytes = Buffer.from(name), data = Buffer.from(value);
    const local = Buffer.alloc(30);
    local.writeUInt32LE(0x04034b50);
    local.writeUInt16LE(nameBytes.length, 26);
    local.writeUInt32LE(data.length, 18);
    local.writeUInt32LE(data.length, 22);
    const central = Buffer.alloc(46);
    central.writeUInt32LE(0x02014b50);
    central.writeUInt32LE(data.length, 20);
    central.writeUInt32LE(data.length, 24);
    central.writeUInt16LE(nameBytes.length, 28);
    central.writeUInt32LE(offset, 42);
    parts.push(local, nameBytes, data);
    directory.push(central, nameBytes);
    offset += local.length + nameBytes.length + data.length;
  }
  const end = Buffer.alloc(22);
  end.writeUInt32LE(0x06054b50);
  end.writeUInt16LE(entries.length, 8);
  end.writeUInt16LE(entries.length, 10);
  end.writeUInt32LE(Buffer.concat(directory).length, 12);
  end.writeUInt32LE(offset, 16);
  return Buffer.concat([...parts, ...directory, end]);
}

test('pinned upstream source and integrity digest are unchanged', () => {
  assert.match(PBMC_SOURCE_URL, /scverse\/scanpy\/1a421f222c65987e010ccd395d6065cf5a9682ce\/src\/scanpy\/datasets\/10x_pbmc68k_reduced\.zarr\.zip$/);
  assert.equal(PBMC_SOURCE_SHA256, '1afd91a3e8edca5f7a5ecb9c6caed1a471b4046f51e6ad033677e42ffac11358');
});

test('ZIP duplicate metadata uses the last central-directory entry', () => {
  const files = unpackStoredZip(storedZip([['zarr.json', 'first'], ['zarr.json', 'last'], ['obs/key', 'original']]));
  assert.equal(files.size, 2);
  assert.equal(Buffer.from(files.get('zarr.json')).toString(), 'last');
  assert.equal(Buffer.from(files.get('obs/key')).toString(), 'original');
});

test('rejects unsafe ZIP paths', () => {
  for (const name of ['../outside', '/absolute', 'obs/../outside', 'C:drive', 'obs\\key', 'obs/./key', 'bad\0name']) {
    assert.throws(() => unpackStoredZip(storedZip([[name, 'original']])), /Unsafe/);
  }
});

test('rejects compressed/encrypted ZIP entries explicitly', () => {
  for (const [field, value] of [[8, 1], [10, 8]]) {
    const bytes = storedZip([['key', 'original']]);
    const central = bytes.indexOf(Buffer.from([0x50, 0x4b, 0x01, 0x02]));
    bytes.writeUInt16LE(value, central + field);
    assert.throws(() => unpackStoredZip(bytes), /Only unencrypted, stored ZIP/);
  }
});

test('rejects malformed or truncated ZIP directories', () => {
  assert.throws(() => unpackStoredZip(new Uint8Array()), /Missing ZIP/);
  const bytes = storedZip([['key', 'original']]);
  assert.throws(() => unpackStoredZip(bytes.subarray(0, bytes.length - 1)), /Missing ZIP/);
  bytes.writeUInt32LE(0xfffffff0, bytes.length - 6);
  assert.throws(() => unpackStoredZip(bytes), /Truncated/);
});

test('HTTP(S) directories stay genuine remote paths and register no local data', async () => {
  const handle = await loadRemoteAnnData(null, { url: 'https://example.org/original.zarr/' });
  assert.equal(handle.storeURL, 'https://example.org/original.zarr');
  assert.equal(handle.sourceURL, handle.storeURL);
  assert.equal(handle.transport, 'remote-directory');
  await handle.dispose();
});

test('rejects credentials and non-HTTP protocols before any download', async () => {
  for (const url of ['file:///pbmc.zarr', 'https://user:password@example.org/store', 'https://example.org/store?token=private', 'https://example.org/store#fragment']) {
    await assert.rejects(loadRemoteAnnData(null, { url }), /HTTP|public AnnData/);
  }
});

test('alternate ZIP URLs require an explicit integrity digest', async () => {
  await assert.rejects(loadRemoteAnnData(null, { url: 'https://example.org/store.zip' }), /SHA-256 digest is required/);
});

test('SQL aliases only the original annotation and uses stored UMAP components', () => {
  const sql = remoteAnnDataViewSQL("https://example.org/a'b.zarr");
  assert.match(sql[0], /SELECT \*, bulk_labels AS cell_type FROM read_zarr/);
  assert.match(sql[0], /a''b\.zarr/);
  assert.match(sql[1], /group_path := 'obsm', table_name := 'obs_X_umap_component'/);
});
