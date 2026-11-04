---
example_title: true
---

# duckdb-zarr <span class="title-accent">for geoscience.</span>

Work with latitude/longitude grids using the repository's existing
air-temperature queries. Run these in DuckDB with the Zarr extension loaded;
see the [loading instructions](../../README.md#loading-once-built).
These are static SQL examples, not browser-executed cells.

Both queries use the real air-temperature tutorial dataset. Run
`make generate_fixtures` first, then run the SQL from the repository root.
The generated data are not included in this website.

## Air temperature for a map

Read one time slice as `lon`, `lat`, and `value` — the same result columns used
by the existing zarr-map demo. A map renderer can color each location by `value`.

```sql
SELECT lon, lat, air AS value
FROM read_zarr('test/fixtures/xarray_tutorial/air_temperature.zarr',
               dims := ['time','lat','lon'])
WHERE time = TIMESTAMP '2013-01-01';
```

## Find the warmest locations

Average temperature at each grid location and return the ten warmest results.
This is the location-ranking query exercised in the repository's
[realistic SQL tests](../../test/sql/realistic_queries.test).

```sql
SELECT lat, lon, AVG(air) AS mean_air
FROM read_zarr('test/fixtures/xarray_tutorial/air_temperature.zarr')
GROUP BY lat, lon
ORDER BY mean_air DESC
LIMIT 10;
```

For monthly temperature summaries and public precipitation data, see the
[climate example](climate.md). The
[tested scientific domains](../domains.md#geoscience) page describes
the broader geographic-grid and ocean-region coverage.

## Bring your own questions.<br><span class="closing-answer">duckdb-zarr for the answers.</span>
