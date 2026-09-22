# Tested scientific domains

`duckdb-zarr` reads Zarr and xarray conventions rather than implementing
domain-specific query engines. Domain coverage therefore means that the test
suite includes representative data and queries from that community.

| Community | Current test coverage |
| --- | --- |
| Atmospheric and climate science | Xarray air-temperature data, an ERA5-style multidimensional dataset, named coordinates, packed values, and dimension-group selection. |
| Ocean and climate science | ERSST sea-surface temperature and basin-mask data, including CF bounds and missing-value handling. |
| Bioimaging | An OME-Zarr v3 multichannel image with a nested label image, including recursive discovery, resolution-level selection, named image axes, and label queries. |
| Remote sensing | Tiled GeoTIFFs (Deflate float32 with nodata, 3-band int8 ZSTD) indexed by virtual-tiff into kerchunk manifests, read through `format='kerchunk'`. |
| Bioimaging (TIFF) | Stripped uint16 LZW plate images (Cell Painting style) indexed by virtual-tiff, read through `format='kerchunk'` with the extension's LZW codec. |
| General xarray/Zarr users | Synthetic Zarr v2 and v3 fixtures covering numeric types, sparse chunks, scalar coordinates, endianness, and compression. |

The format may work for other communities, including astronomy and genomics, but those domains do not yet have representative fixtures in
the test suite. Support claims should follow the addition of domain-specific
test data and queries.

GeoZarr-style pyramids are a likely next area to evaluate because they share
some structural patterns with OME-Zarr multiscales, but they are not currently
covered by fixtures.

See [Querying OME-Zarr](ome-zarr.md) for the current bioimage example and
[Virtual Zarr](virtual-zarr.md) for kerchunk manifests over NetCDF4 and GeoTIFF.
