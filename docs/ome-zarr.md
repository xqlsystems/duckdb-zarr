# Querying OME-Zarr

OME-Zarr images commonly contain several resolution levels and nested label
arrays. Start by listing the available arrays:

```sql
SELECT name, dims, shape, dtype
FROM read_zarr_metadata('image.ome.zarr');
```

Then select a resolution level and aggregate a region by channel:

```sql
SELECT c, AVG("0") AS mean_intensity
FROM read_zarr('image.ome.zarr', array_path='0')
WHERE y BETWEEN 100 AND 199
  AND x BETWEEN 200 AND 299
GROUP BY c;
```

Here, `0` is the conventional path for the highest-resolution level. Xarray
dimension names such as `c`, `y`, and `x` become SQL columns. Numeric and nested
array names must be double-quoted when referenced as columns.

Nested label arrays use the same selector:

```sql
SELECT "labels/nuclei/0" AS label, COUNT(*) AS pixels
FROM read_zarr('image.ome.zarr', array_path='labels/nuclei/0')
WHERE "labels/nuclei/0" > 0
GROUP BY label;
```
