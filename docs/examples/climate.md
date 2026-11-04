---
example_title: true
---

# duckdb-zarr <span class="title-accent">for climate.</span>

Explore temperature over time and public precipitation data using the
repository's existing SQL examples. Run these in DuckDB with the Zarr extension
loaded; see the [loading instructions](../../README.md#loading-once-built).
These are static SQL examples, not browser-executed cells.

For latitude/longitude grids and map-ready results, see the
[geoscience example](geoscience.md).

## Monthly mean air temperature

Group the real air-temperature tutorial dataset by month. CF-encoded time
coordinates are decoded to DuckDB timestamps, so ordinary date functions work
directly.

Run `make generate_fixtures` first, then run this SQL from the repository root.
The generated data are not included in this website.

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
external endpoint is unavailable, the local example above still works.

```sql
SELECT time, latitude, longitude, precip
FROM read_zarr(
  'https://ncsa.osn.xsede.org/Pangeo/pangeo-forge/gpcp-feedstock/gpcp.zarr',
  dims=['time','latitude','longitude']
)
LIMIT 10;
```

Both queries also appear in the [repository quick start](../../README.md).
See the [real-data HTTP tests](../../test/test_http_integration_real.py) for
the GPCP checks and [tested scientific domains](../domains.md#climate)
for the broader climate and ocean coverage.

## Bring your own questions.<br><span class="closing-answer">duckdb-zarr for the answers.</span>
