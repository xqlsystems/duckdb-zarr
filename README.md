<img src="docs/images/logo.png" alt="duckdb-zarr logo" width="280">

# duckdb-zarr

[![Main Extension Distribution Pipeline](https://github.com/alxmrs/duckdb-zarr/actions/workflows/MainDistributionPipeline.yml/badge.svg)](https://github.com/alxmrs/duckdb-zarr/actions/workflows/MainDistributionPipeline.yml)

A Rust DuckDB extension that lets you query [Zarr](https://zarr.dev/) stores with SQL — in the same spirit as [xarray-sql](https://github.com/alxmrs/xarray-sql) and [zarr-datafusion](https://lib.rs/crates/zarr-datafusion), but as a first-class DuckDB extension with no external query engine.

```sql
-- Query a public Zarr store straight from its URL. GPCP is a multi-group store
-- (a precip variable plus CF bounds), so pick the group with dims= — a list of
-- dimension names.
SELECT time, latitude, longitude, precip
FROM read_zarr(
  'https://ncsa.osn.xsede.org/Pangeo/pangeo-forge/gpcp-feedstock/gpcp.zarr',
  dims=['time','latitude','longitude']
)
LIMIT 10;

-- Inspect a store's arrays — a local path or a URL
SELECT name, role, dtype, shape
FROM read_zarr_metadata('https://ncsa.osn.xsede.org/Pangeo/pangeo-forge/gpcp-feedstock/gpcp.zarr');

-- Single-group stores need no dims=; a path/URL ending in .zarr is intercepted
-- automatically. (These use this repo's fixtures — run `make generate_fixtures` first.)
SELECT lat, lon, AVG(temperature)
FROM 'test/fixtures/xarray_tutorial/float_baseline.zarr'
GROUP BY lat, lon;

-- CF time coordinates arrive as TIMESTAMP, so date predicates and date
-- functions need no ceremony (decode_times=false gives the raw offsets).
SELECT date_trunc('month', time) AS month, AVG(air) AS mean_air
FROM 'test/fixtures/xarray_tutorial/air_temperature.zarr'
WHERE time >= TIMESTAMP '2014-01-01'
GROUP BY month ORDER BY month;

-- Select one array by its store-relative path (e.g. an OME-Zarr resolution level or nested label)
SELECT * FROM read_zarr('test/fixtures/bioimage/ome_zarr/synthetic_multichannel.ome.zarr', array_path='0');

-- List a store's dimension groups
SELECT dims, shape, data_vars FROM read_zarr_groups('test/fixtures/xarray_tutorial/multi_dim_group.zarr');
```

See [docs/design.md](docs/design.md) for the full design and
[docs/ome-zarr.md](docs/ome-zarr.md) for a small bioimage example.

## Remote stores

Read directly from a URL — HTTP/HTTPS, S3, GCS, or Azure. Object stores can't list
directories, so a remote store must carry **consolidated metadata**: a Zarr v2
`.zmetadata` object or a Zarr v3 `consolidated_metadata` block. Most published
datasets already do (e.g. anything written by xarray with `consolidated=True`), so
they read straight from their URL as shown above. S3/GCS/Azure credentials come from
DuckDB's secrets manager (`CREATE SECRET ... TYPE S3`).

## Status

Active development. Phases 1–3 are implemented:

- **Phase 1** — `read_zarr`, `read_zarr_metadata`, `read_zarr_groups` table functions; Zarr v3; CF conventions (fill values, scale/offset, time → `TIMESTAMP`, bounds variables, aux coords)
- **Phase 2** — Zarr v2, Blosc/LZ4, replacement scan for local `.zarr` paths, projection pushdown
- **Phase 3** — HTTP/HTTPS stores, `dims=` and `array_path=` selection, recursive array discovery

See the [phased plan](docs/design.md#phased-plan) for what's next.

## Building

```shell
git clone --recurse-submodules <repo>
make configure
make debug
```

Requires: Rust toolchain, Python 3.11+, make, git.

## Testing

```shell
make test_debug   # SQLLogicTest suite over local stores (or make test_release)
make test_http    # HTTP integration tests: synthetic loopback + a real public store
```

SQLLogicTest cases live in `test/sql/`. The HTTP suite (`test/test_http_integration*.py`)
reads stores over HTTP: synthetic v2 `.zmetadata` and v3 `consolidated_metadata` fixtures
via a loopback server, plus the live public GPCP store (network-gated — skipped when
unreachable). Fixtures are generated automatically by `make test_*`. To regenerate manually:

```shell
make generate_fixtures
```

Fixture generation uses [uv](https://docs.astral.sh/uv/) with the dependencies declared in `pyproject.toml`. Install uv with `pip install uv` or via your system package manager.

## Loading (once built)

```sh
duckdb -unsigned
```

```sql
LOAD './build/debug/extension/zarr/zarr.duckdb_extension';
```
