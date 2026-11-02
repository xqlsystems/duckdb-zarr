# Climate and geoscience

Three SQL examples from the repository's existing climate queries. Run them in
DuckDB with the Zarr extension loaded; see the
[loading instructions](../../README.md#loading-once-built).
These are static SQL examples, not browser-executed cells.

## Air temperature for a map

Read one time slice as `lon`, `lat`, and `value` — the same result columns used
by the existing zarr-map demo. A map renderer can color each location by `value`.

This uses the real air-temperature tutorial dataset. For this and the next
query, run `make generate_fixtures` first, then run the SQL from the repository
root. The generated data are not included in this website.

```sql
SELECT lon, lat, air AS value
FROM read_zarr('test/fixtures/xarray_tutorial/air_temperature.zarr',
               dims := ['time','lat','lon'])
WHERE time = TIMESTAMP '2013-01-01';
```

## Monthly mean air temperature

Group the same dataset by month. CF-encoded time coordinates are decoded to
DuckDB timestamps, so ordinary date functions work directly.

```sql
SELECT date_trunc('month', time) AS month, AVG(air) AS mean_air
FROM 'test/fixtures/xarray_tutorial/air_temperature.zarr'
WHERE time >= TIMESTAMP '2014-01-01'
GROUP BY month ORDER BY month;
```

## Public precipitation data

Read the public GPCP store directly over HTTPS. Select the precipitation table
by its dimensions because the store also contains coordinate-bound arrays.
This query needs network access, but no local dataset preparation. If the
external endpoint is unavailable, the local examples above still work.

```sql
SELECT time, latitude, longitude, precip
FROM read_zarr(
  'https://ncsa.osn.xsede.org/Pangeo/pangeo-forge/gpcp-feedstock/gpcp.zarr',
  dims=['time','latitude','longitude']
)
LIMIT 10;
```

The monthly and precipitation queries also appear in the
[repository quick start](../../README.md). See the
[real-data HTTP tests](../../test/test_http_integration_real.py) for the GPCP
checks and [tested scientific domains](../domains.md#climate-and-geoscience)
for the broader climate and ocean coverage.
