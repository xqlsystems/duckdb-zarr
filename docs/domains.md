# Tested scientific domains

`duckdb-zarr` reads Zarr and xarray conventions rather than implementing
domain-specific query engines. Domain coverage therefore means that the test
suite includes representative data and queries from that community.

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

See [Querying AnnData stores](anndata.md), including its
[SpatialData boundaries](anndata.md#spatialdata-stores), and
[Querying OME-Zarr](ome-zarr.md).

## Climate and geoscience

| Community | Current test coverage |
| --- | --- |
| Atmospheric and climate science | Xarray air-temperature data, an ERA5-style multidimensional dataset, named coordinates, packed values, and dimension-group selection. |
| Ocean and climate science | ERSST sea-surface temperature and basin-mask data, including CF bounds and missing-value handling. |

GeoZarr-style pyramids are a likely next area to evaluate because they share
some structural patterns with OME-Zarr multiscales, but they are not currently
covered by fixtures.

## General xarray/Zarr

Synthetic Zarr v2 and v3 fixtures cover numeric types, sparse chunks, scalar
coordinates, endianness, and compression. Nested-group tests cover
[xarray DataTree-style stores](../test/sql/datatree.test).

## Not yet represented

The format may work for other communities, including astronomy and
remote sensing, but those domains do not yet have representative fixtures in
the test suite. Support claims should follow the addition of domain-specific
test data and queries.
