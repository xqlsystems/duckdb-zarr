# Tested scientific domains

`duckdb-zarr` reads Zarr and xarray conventions rather than implementing
domain-specific query engines. Domain coverage therefore means that the test
suite includes representative data and queries from that community.

| Community | Current test coverage |
| --- | --- |
| Atmospheric and climate science | Xarray air-temperature data, an ERA5-style multidimensional dataset, named coordinates, packed values, and dimension-group selection. |
| Ocean and climate science | ERSST sea-surface temperature and basin-mask data, including CF bounds and missing-value handling. |
| Bioimaging | An OME-Zarr v3 multichannel image with a nested label image, including recursive discovery, resolution-level selection, named image axes, and label queries. |
| General xarray/Zarr users | Synthetic Zarr v2 and v3 fixtures covering numeric types, sparse chunks, scalar coordinates, endianness, and compression. |

The format may work for other communities, including astronomy, genomics, and
remote sensing, but those domains do not yet have representative fixtures in
the test suite. Support claims should follow the addition of domain-specific
test data and queries.

GeoZarr-style pyramids are a likely next area to evaluate because they share
some structural patterns with OME-Zarr multiscales, but they are not currently
covered by fixtures.

See [Querying OME-Zarr](ome-zarr.md) for the current bioimage example.
