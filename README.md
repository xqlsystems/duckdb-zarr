# duckdb-zarr

[![Main Extension Distribution Pipeline](https://github.com/alxmrs/duckdb-zarr/actions/workflows/MainDistributionPipeline.yml/badge.svg)](https://github.com/alxmrs/duckdb-zarr/actions/workflows/MainDistributionPipeline.yml)

A Rust DuckDB extension that lets you query [Zarr](https://zarr.dev/) stores with SQL — in the same spirit as [xarray-sql](https://github.com/alxmrs/xarray-sql) and [zarr-datafusion](https://lib.rs/crates/zarr-datafusion), but as a first-class DuckDB extension with no external query engine.

```sql
-- Local store: path ending in .zarr is intercepted automatically
SELECT lat, lon, AVG(temperature)
FROM 'era5.zarr'
GROUP BY lat, lon;

-- HTTP/HTTPS stores also work via replacement scan
SELECT * FROM 'https://example.com/data.zarr';

-- Explicit table function with optional dims= for multi-group stores
SELECT * FROM read_zarr('path/to/store.zarr', dims=['time', 'lat', 'lon']);

-- Select one array by its store-relative path (including nested arrays)
SELECT * FROM read_zarr('image.ome.zarr', array_path='labels/nuclei/0');

-- Inspect arrays in a store
SELECT name, role, dtype, shape FROM read_zarr_metadata('path/to/store.zarr');

-- List dimension groups
SELECT * FROM read_zarr_groups('path/to/store.zarr');
```

See [docs/design.md](docs/design.md) for the full design and
[docs/ome-zarr.md](docs/ome-zarr.md) for a small bioimage example.

## Status

Active development. Phases 1–3 are implemented:

- **Phase 1** — `read_zarr`, `read_zarr_metadata`, `read_zarr_groups` table functions; Zarr v3; CF conventions (fill values, scale/offset, bounds variables, aux coords)
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
make test_debug   # or make test_release
```

Tests are in `test/sql/` (SQLLogicTest format). Fixtures are generated automatically by `make test_*`. To regenerate manually:

```shell
make generate_fixtures
```

Fixture generation uses [uv](https://docs.astral.sh/uv/) with the dependencies declared in `pyproject.toml`. Install uv with `pip install uv` or via your system package manager.

## Loading (once built)

```sh
duckdb -unsigned
```

```sql
LOAD './build/debug/extension/duckdb_zarr/duckdb_zarr.duckdb_extension';
```
