import { test } from 'node:test';
import assert from 'node:assert/strict';
import { anndataViewSQL, labelsSQL, scatterSQL, spatialdataViewSQL, sqlString, volumeSQL } from '../src/queries.js';

test('SQL strings escape annotations without changing them', () => {
  assert.equal(sqlString("T cell's"), "'T cell''s'");
  assert.match(scatterSQL('anndata', { filter: "T cell's" }), /WHERE o.cell_type = 'T cell''s'/);
});
test('UMAP pivots components and joins by cell name', () => {
  const sql = scatterSQL('anndata');
  assert.match(sql, /MAX\(X_umap\) FILTER \(WHERE X_umap_component = 0\)/);
  assert.match(sql, /JOIN positions p USING \(obs\)/);
  assert.doesNotMatch(sql, /WHERE o.cell_type/);
});
test('volume SELECT lists put each column on its own indented line', () => {
  for (const statistics of [false, true]) {
    const sql = volumeSQL('https://example.org/image.zarr', { zmin: 0, zmax: 235, statistics });
    const select = sql.split('\nFROM ')[0].split('\n');
    assert.equal(select[0], 'SELECT');
    assert.equal(select.length, statistics ? 7 : 6);
    for (const [index, column] of select.slice(1).entries()) {
      assert.match(column, /^  \S/);
      assert.equal(column.endsWith(','), index < select.length - 2);
      assert.doesNotMatch(column, /,\s*\S/);
    }
  }
});
test('volume examples use plain coordinate columns without casts', () => {
  const sql = volumeSQL('https://example.org/image.zarr', { zmin: 0, zmax: 235 });
  assert.equal(sql.split('\nFROM ')[0], 'SELECT\n  c,\n  x,\n  y,\n  z,\n  value');
  assert.doesNotMatch(sql, /::|CAST\s*\(/i);
});
test('all channels are returned together and statistics stay separated by channel', () => {
  const sql = volumeSQL('https://example.org/image.zarr', { channel: 'all', zmin: 0, zmax: 235 });
  assert.match(sql, /SELECT\s+c,\s+x,\s+y,\s+z,\s+value/);
  assert.doesNotMatch(sql, /c =/);
  assert.match(sql, /WHERE z BETWEEN 0 AND 235/);
  const statistics = volumeSQL('x', { channel: 'all', zmin: 0, zmax: 235, statistics: true });
  assert.match(statistics, /SELECT\s+c,\s+COUNT/);
  assert.match(statistics, /GROUP BY c ORDER BY c/);
});

test('volume filters implicit channel and depth coordinates', () => {
  const sql = volumeSQL('https://example.org/image.zarr', { channel: 1, zmin: 2, zmax: 4, level: '3' });
  assert.match(sql, /array_path := '3'/);
  assert.match(sql, /WHERE c = 1/);
  assert.match(sql, /z BETWEEN 2 AND 4/);
  assert.doesNotMatch(sql, /bounds/);
  assert.match(volumeSQL('x', { channel: 0, zmin: 0, zmax: 4, statistics: true }), /AVG\(value\) AS mean_intensity/);
});
test('the value column is aliased for any purely-numeric or slashed level basename', () => {
  // Mirrors needs_value_alias() in the Rust extension: a bare numeric level name
  // (array_path '0', '2', 'labels/0/0') can't be a valid unquoted/quoted identifier
  // the same way, so duckdb-zarr exposes it as the generic column `value` instead.
  assert.match(volumeSQL('x', { channel: 0, zmin: 0, zmax: 0, level: '0' }), /,\s+value\n/);
  assert.match(volumeSQL('x', { channel: 0, zmin: 0, zmax: 0, level: 'labels/0/0' }), /,\s+value\n/);
  assert.match(volumeSQL('x', { channel: 0, zmin: 0, zmax: 0, level: 's2' }), /,\s+"s2" AS value\n/);
});
test('nonnumeric nested image and label paths use value, not the basename', () => {
  const image = volumeSQL('x', { channel: 0, zmin: 0, zmax: 0, level: 'pyramid/full' });
  assert.match(image, /,\s+value\n/);
  const labels = labelsSQL('x', { zmin: 0, zmax: 1, arrayPath: 'labels/nuclei' });
  assert.match(labels, /,\s+value\n/);
  assert.match(labelsSQL('x', { zmin: 0, zmax: 1, arrayPath: 'labels/nuclei', statistics: true }), /COUNT\(DISTINCT value\)/);
  assert.match(volumeSQL('x', { channel: 0, zmin: 0, zmax: 0, level: 'stain"blue' }), /"stain""blue" AS value/);
});

test('invalid depth bounds are rejected', () => {
  for (const args of [{ channel: -1, zmin: 0, zmax: 2 }, { channel: 0, zmin: 4, zmax: 2 }, { channel: 0, zmin: 0.1, zmax: 4 }]) assert.throws(() => volumeSQL('x', args));
});
test('AnnData views point at whatever store URL is passed in', () => {
  const statements = anndataViewSQL('https://example.org/biology/data/pbmc.zarr');
  assert.equal(statements.length, 2);
  for (const sql of statements) assert.match(sql, /CREATE OR REPLACE VIEW/);
  assert.match(statements[0], /https:\/\/example.org\/biology\/data\/pbmc.zarr/);
  assert.match(statements[1], /group_path := 'obsm'/);
});
test('SpatialData views remain defined for when that section returns', () => {
  const statements = spatialdataViewSQL('https://example.org/data/tissue.zarr');
  assert.equal(statements.length, 3);
  assert.match(statements[0], /group_path := 'tables\/table'/);
});
test('labels query has no channel filter and reuses the value-aliasing rule', () => {
  const sql = labelsSQL('https://example.org/idr0062A.ome.zarr', { zmin: 10, zmax: 20, arrayPath: 'labels' });
  assert.match(sql, /array_path := 'labels'/);
  assert.match(sql, /z BETWEEN 10 AND 20/);
  assert.doesNotMatch(sql, /WHERE c =/);
  assert.match(sql, /"labels" AS value/);
  assert.match(labelsSQL('x', { zmin: 0, zmax: 0, arrayPath: 'labels/0/0', statistics: true }), /COUNT\(DISTINCT value\) AS distinct_labels/);
});
test('invalid label depth bounds are rejected', () => {
  for (const args of [{ zmin: -1, zmax: 2 }, { zmin: 4, zmax: 2 }]) assert.throws(() => labelsSQL('x', args));
});
