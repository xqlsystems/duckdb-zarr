# Tested scientific domains

`duckdb-zarr` reads Zarr and xarray conventions rather than implementing
domain-specific query engines. Domain coverage therefore means that the test
suite includes representative data and queries from that community.

The sections follow the field guide order: Biology,
Climate, then Geoscience. Shared storage conventions are covered separately
below.

## Biology

| Community | Current test coverage |
| --- | --- |
| Annotated matrices / AnnData | A synthetic cell-by-gene store with named observations and variables, categorical and nullable annotations, sparse CSR/CSC matrices, layers, embeddings, pairwise matrices, views, and direct-array reads ([SQL tests](../test/sql/anndata.test)). The [comparison script](../scripts/check_anndata_tables.py) checks tables against `anndata.read_zarr` for three AnnData fixtures. |
| Spatial biology / SpatialData tables | A synthetic SpatialData-like store with nested AnnData objects, inherited observation names, a label-image-to-annotation join, and view mounting ([SQL tests](../test/sql/spatialdata.test)). This does not establish comprehensive SpatialData support or test reading points and shapes from Parquet. |
| Bioimaging | An OME-Zarr v3 multichannel image with a nested label image, including recursive discovery, resolution-level selection, named image axes, and label queries. |

The [AnnData edge-case tests](../test/test_anndata_edge_cases.py) also check
`raw/X` and `raw/var`, independently chunked nullable masks, and categorical
ENUM width boundaries in Zarr v2/v3, both at the root and in nested tables.
They run with `make test_debug` and `make test_release`.

See the [Biology field guide](../demos/biology/README.md) for the interactive
walkthrough, [Querying AnnData stores](anndata.md), including its
[SpatialData boundaries](anndata.md#spatialdata-stores), and
[Querying OME-Zarr](ome-zarr.md).

<a id="climate-and-geoscience"></a>

## Climate

| Community | Current test coverage |
| --- | --- |
| Atmospheric and climate science | Xarray air-temperature data, a synthetic ERA5-style multidimensional dataset, named coordinates, packed values, and dimension-group selection ([ERA5-style SQL tests](../test/sql/era5_tables.test)). |
| Ocean and climate science | ERSST sea-surface temperature, including CF bounds and missing-value handling. |
| Public precipitation data | Network-gated GPCP tests cover remote metadata, coordinates, and compressed values. They skip when the public endpoint is unreachable ([HTTP tests](../test/test_http_integration_real.py)). |

See the [Climate field guide](examples/climate.md) for monthly temperature
summaries and the public precipitation query.

## Geoscience

| Community | Current test coverage |
| --- | --- |
| Geographic grids | Latitude/longitude queries, spatial aggregation, and ranking the warmest grid locations in the air-temperature dataset ([realistic SQL tests](../test/sql/realistic_queries.test)). |
| Ocean regions | Basin-mask arrays, including missing-value handling ([reader SQL tests](../test/sql/read_zarr.test)). |

See the [Geoscience field guide](examples/geoscience.md) for a public Landsat
Zarr composite, calibrated red/NIR reflectance, and NDVI queries. This live-data
walkthrough is separate from the automated fixture coverage above.

The guide reads one coarse pyramid level; this is not comprehensive GeoZarr
interoperability coverage. Pyramid conventions are not yet covered by fixtures.

## General xarray/Zarr

Synthetic Zarr v2 and v3 fixtures cover numeric types, sparse chunks, scalar
coordinates, endianness, and compression. Nested-group tests cover
[xarray DataTree-style stores](../test/sql/datatree.test), with separate groups
and inherited coordinates checked for
[both Zarr versions](../test/test_datatree_compatibility.py).

## Not yet represented

Astronomy does not yet have representative fixtures. Remote sensing is
illustrated by the Landsat field guide, but likewise has no dedicated fixtures
in the default test suite. Support claims should distinguish a verified
walkthrough from systematic format and domain coverage.
