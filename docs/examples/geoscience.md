---
example_title: true
---

# duckdb-zarr <span class="title-accent">for geoscience.</span>

Explore Landsat surface reflectance and vegetation using SQL over a public
Zarr store. These are native DuckDB walkthroughs, not browser-executed cells.

Run the SQL blocks in order on the same connection after
[loading the Zarr extension](../../README.md#loading-once-built).
The data stay at their original HTTPS source; no local dataset preparation is
needed, and no imagery is bundled with this website.

## Open a small Landsat composite

The [New England leaf-on Landsat composites](https://data.source.coop/kentstephen/landsat-mosaics-new-england/README.md)
combine cloud-screened Landsat observations into one image per year. Use 2024
and the coarsest pyramid group, `leafon/7`, for a small regional overview.
Its `x` and `y` coordinates are longitude and latitude in EPSG:4326; `time`
contains integer years, not timestamps. This is an annual composite, not an
individual Landsat scene.

```sql
CREATE OR REPLACE VIEW landsat_2024 AS
SELECT
  time AS year,
  x AS lon,
  y AS lat,
  red * 0.0000275 - 0.2 AS red,
  nir * 0.0000275 - 0.2 AS nir
FROM read_zarr(
  'https://data.source.coop/kentstephen/landsat-mosaics-new-england/pyramid_v1',
  group_path := 'leafon/7',
  bounds := [['time', '2024', '2024']]
) AS pixels
WHERE pixels.red BETWEEN 1 AND 65455
  AND pixels.nir BETWEEN 1 AND 65455;
```

The bands contain digital numbers. Their documented conversion is
`reflectance = DN * 0.0000275 - 0.2`; the store uses `scale` and `offset`
attributes rather than CF's `scale_factor` and `add_offset`, so apply it
explicitly. The filter removes nodata zero and values above the published valid
range before scaling. Do not compute NDVI directly from these raw numbers:
the additive offset does not cancel.

## Vegetation from Landsat bands

The normalized difference vegetation index is `(NIR - red) / (NIR + red)`.
Each pixel is a row, so the calculation is ordinary column arithmetic:

```sql
CREATE OR REPLACE VIEW landsat_ndvi AS
SELECT
  lon,
  lat,
  (nir - red) / NULLIF(nir + red, 0) AS ndvi
FROM landsat_2024
WHERE red > 0
  AND nir > 0;

SELECT
  lon,
  lat,
  ndvi
FROM landsat_ndvi
ORDER BY lat DESC, lon
LIMIT 20;
```

This introductory example keeps positive reflectances; negative reflectance
can occur over water or shadow and is not automatically a storage error.
`NULLIF` protects the denominator. Remove `LIMIT 20` to return the whole coarse
grid for plotting. Higher positive NDVI often indicates vegetation, but NDVI
alone is not a land-cover classification.

## Summarize the vegetation index

```sql
SELECT
  COUNT(*) AS pixels,
  MIN(ndvi) AS minimum_ndvi,
  AVG(ndvi) AS mean_ndvi,
  MAX(ndvi) AS maximum_ndvi,
  COUNT(*) FILTER (WHERE ndvi >= 0.5) AS high_ndvi_pixels
FROM landsat_ndvi;
```

The threshold is illustrative, not a universal vegetation mask. These coarse
bands are averaged before NDVI is calculated: this is not the mean of
full-resolution, 30 m pixel NDVI. Use finer levels and appropriate quality masks
for a scientific analysis.

Data: `kentstephen` on Source Cooperative, derived from USGS Landsat Collection
2 Level-2 Tier 1 surface reflectance via Microsoft Planetary Computer;
CC0-1.0. The column-arithmetic example follows the
[xarray-sql NDVI benchmark](https://github.com/xqlsystems/xarray-sql/blob/103285c891a773803ce1415c5f9e16b6b830aac7/benchmarks/geospatial/01_ndvi.py),
which uses Sentinel-2; the dataset here is Landsat.

For temperature and precipitation examples, see the [climate guide](climate.md).
The [scientific domains](../domains.md#geoscience) page distinguishes the
walkthrough from automated fixture coverage.

## Bring your own questions.<br><span class="closing-answer">duckdb-zarr for the answers.</span>
