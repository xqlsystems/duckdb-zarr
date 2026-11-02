export const sqlString = (value) => `'${String(value).replaceAll("'", "''")}'`;

// The duckdb-zarr extension aliases a single data-variable column to the bare
// identifier `value` whenever its basename would otherwise need SQL double-quoting:
// a purely numeric level name (array_path 'labels/0/0/2' -> basename '2') or a path
// containing '/' (see needs_value_alias() in src/read_zarr.rs). Match that rule exactly,
// or querying a level whose basename is a number (every array_path here, e.g. '2')
// fails with "Referenced column ... not found" because the real column is `value`, not `"2"`.
/** Choose the extension value alias for numeric or nested paths; quote ordinary variable names
 * safely.
 */
function valueColumn(path) {
  const basename = path.split('/').at(-1);
  return path.includes('/') || /^\d+$/.test(basename) ? 'value' : `"${basename.replaceAll('"', '""')}"`;
}

/** Join observation IDs to stored embedding components, with optional annotation and expression
 * filtering.
 */
export function scatterSQL(kind, { filter = '', gene = '' } = {}) {
  const spatial = kind === 'spatialdata';
  const prefix = spatial ? 'tissue' : 'pbmc';
  const embedding = spatial ? 'spatial' : 'X_umap';
  return `WITH positions AS (
  SELECT obs,
    MAX(${embedding}) FILTER (WHERE ${embedding}_component = 0) AS x,
    MAX(${embedding}) FILTER (WHERE ${embedding}_component = 1) AS y
  FROM ${prefix}_coordinates
  GROUP BY obs
)
SELECT o.obs AS cell_id, p.x, p.y,
       o.${spatial ? 'polygon_layer' : 'cell_type'} AS label${gene && spatial ? ', COALESCE(e.X, 0) AS value' : ''}
FROM ${prefix}_obs o
JOIN positions p USING (obs)${gene && spatial ? `
LEFT JOIN tissue_expression e
  ON e.obs = o.obs AND e.var = ${sqlString(gene)}` : ''}${filter ? `
WHERE o.${spatial ? 'polygon_layer' : 'cell_type'} = ${sqlString(filter)}` : ''}
ORDER BY cell_id`;
}

/** Select channel/depth voxels or grouped channel statistics after validating integer bounds. */
export function volumeSQL(url, { channel = 'all', zmin, zmax, level = '0', statistics = false }) {
  for (const value of channel === 'all' ? [zmin, zmax] : [channel, zmin, zmax]) {
    if (!Number.isSafeInteger(Number(value)) || Number(value) < 0) {
      throw new Error('Channel and Z planes must be nonnegative whole numbers.');
    }
  }
  if (Number(zmin) > Number(zmax)) throw new Error('First Z plane must not exceed last Z plane.');
  const column = valueColumn(level);
  return `SELECT ${statistics ? `c, COUNT(*) AS voxels, MIN(${column}) AS minimum,\n       MAX(${column}) AS maximum, AVG(${column}) AS mean_intensity,\n       STDDEV_POP(${column}) AS stddev_intensity` : `c::UTINYINT AS c, x::USMALLINT AS x, y::USMALLINT AS y,\n       z::USMALLINT AS z, ${column} AS value`}
FROM read_zarr(${sqlString(url)}, array_path := ${sqlString(level)})
WHERE ${channel === 'all' ? '' : `c = ${Number(channel)}\n  AND `}z BETWEEN ${Number(zmin)} AND ${Number(zmax)}${statistics ? '\nGROUP BY c ORDER BY c' : ''}`;
}

/** Query a channel-free label array over a validated Z interval; preserve the reader's value alias. */
export function labelsSQL(url, { zmin, zmax, arrayPath = 'labels', statistics = false }) {
  for (const value of [zmin, zmax]) {
    if (!Number.isSafeInteger(Number(value)) || Number(value) < 0) {
      throw new Error('Z planes must be nonnegative whole numbers.');
    }
  }
  if (Number(zmin) > Number(zmax)) throw new Error('First Z plane must not exceed last Z plane.');
  const column = valueColumn(arrayPath);
  return `SELECT ${statistics ? `COUNT(*) AS voxels, MIN(${column}) AS minimum,\n       MAX(${column}) AS maximum, COUNT(DISTINCT ${column}) AS distinct_labels` : `x, y, z, ${column} AS value`}
FROM read_zarr(${sqlString(url)}, array_path := ${sqlString(arrayPath)})
WHERE z BETWEEN ${Number(zmin)} AND ${Number(zmax)}`;
}

/** Build view definitions for AnnData observations and stored UMAP components without executing
 * SQL.
 */
export function anndataViewSQL(url) {
  const store = sqlString(url);
  return [
    `CREATE OR REPLACE VIEW pbmc_obs AS SELECT * FROM read_zarr(${store}, table_name := 'obs')`,
    `CREATE OR REPLACE VIEW pbmc_coordinates AS SELECT * FROM read_zarr(${store}, group_path := 'obsm', table_name := 'obs_X_umap_component')`,
  ];
}

/** Build the paused SpatialData table views without executing SQL or loading an extra dataset. */
export function spatialdataViewSQL(url) {
  const store = sqlString(url);
  return [
    `CREATE OR REPLACE VIEW tissue_obs AS SELECT * FROM read_zarr(${store}, group_path := 'tables/table', table_name := 'obs')`,
    `CREATE OR REPLACE VIEW tissue_coordinates AS SELECT * FROM read_zarr(${store}, group_path := 'tables/table/obsm', table_name := 'obs_spatial_component')`,
    `CREATE OR REPLACE VIEW tissue_expression AS SELECT * FROM read_zarr(${store}, group_path := 'tables/table', table_name := 'obs_var')`,
  ];
}
