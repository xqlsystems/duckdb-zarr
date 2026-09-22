# Virtual Zarr: kerchunk manifests

A [kerchunk](https://fsspec.github.io/kerchunk/spec.html) JSON manifest describes a
Zarr store whose chunks are byte ranges inside other files: NetCDF4/HDF5, GRIB,
GeoTIFF, or another Zarr. Tools such as [VirtualiZarr](https://virtualizarr.readthedocs.io/)
and [virtual-tiff](https://github.com/virtual-zarr/virtual-tiff) produce them without
copying any data. `duckdb-zarr` reads a manifest with `format='kerchunk'` on any of
the three table functions:

```sql
SELECT * FROM read_zarr('refs.json', format='kerchunk');
SELECT * FROM read_zarr('s3://bucket/index/refs.json', format='kerchunk', dims=['time','lat','lon']);
SELECT * FROM read_zarr_metadata('refs.json', format='kerchunk');
SELECT * FROM read_zarr_groups('refs.json', format='kerchunk');
```

## How a manifest is written

A manifest is a JSON document mapping Zarr store keys to either a metadata document
or a `[path, offset, length]` byte range in some other file. Any tool can write one;
these are the two producers the test suite uses. VirtualiZarr indexes a NetCDF4 file:

```python
from virtualizarr import open_virtual_dataset
from virtualizarr.parsers import HDFParser
from obspec_utils.registry import ObjectStoreRegistry
from obstore.store import LocalStore

registry = ObjectStoreRegistry({"file://": LocalStore()})
vds = open_virtual_dataset("file:///data/sst.nc", parser=HDFParser(), registry=registry)
vds.vz.to_kerchunk("sst.json", format="json")
```

and virtual-tiff indexes a TIFF (`VirtualTIFF(ifd=0)` in place of `HDFParser()`).
The result for a two-variable NetCDF4 looks like this, trimmed:

```json
{
  "version": 1,
  "refs": {
    ".zgroup": "{\"zarr_format\": 2}",
    "sst/.zarray": "{\"shape\": [365, 720, 1440], \"chunks\": [1, 720, 1440], \"dtype\": \"<f4\", \"filters\": [{\"id\": \"shuffle\", \"elementsize\": 4}], \"compressor\": {\"id\": \"zlib\", \"level\": 4}, ...}",
    "sst/.zattrs": "{\"_ARRAY_DIMENSIONS\": [\"time\", \"lat\", \"lon\"], \"units\": \"degC\"}",
    "sst/0.0.0": ["/data/sst.nc", 8192, 1048576],
    "sst/1.0.0": ["/data/sst.nc", 1056768, 1048576],
    "time/.zarray": "{\"shape\": [365], \"chunks\": [365], \"dtype\": \"<i4\", ...}",
    "time/0": "base64:AAAAAAEAAAACAAAA..."
  }
}
```

Every chunk key is a byte range of the source file (`sst/0.0.0` is the first 1 MiB
of `sst.nc` after an 8 KiB header), small arrays are inlined as base64, and the
`.zarray` documents carry the source file's own filter pipeline (here HDF5 shuffle
and deflate) as Zarr v2 codec ids. The reader needs a codec for each id it meets;
see [Codecs](#codecs).

Paths may be local, `file://`, `s3://`, `gs://`, `az://` or `https://`. A manifest
written on one machine works on another as long as the paths still resolve, which
is why manifests over object storage usually carry absolute URLs.

## How a manifest is read

The manifest and every file it references are opened through DuckDB's filesystem,
so a manifest can live anywhere DuckDB can read and can point anywhere DuckDB can
read: local paths, HTTP(S), S3, GCS and Azure, with the secrets manager applying to
each file as it is opened. Nothing in the reader is local-only; a local manifest may
reference `https://` files and vice versa. Relative paths in the manifest resolve
against DuckDB's working directory.

Metadata documents (`.zgroup`, `.zarray`, `.zattrs`) and inline chunks (plain text or
`base64:`-prefixed) are served from memory. A `.zmetadata` document is synthesised
when the manifest does not carry one, so array discovery works the same way it does
for consolidated remote stores. Each chunk key that is a `[path, offset, length]`
reference becomes one range read of the referenced file; whole-file `[path]`
references are served from offset zero to the end of the file. Open handles are
pooled and reused across chunks, since one source file typically backs thousands of
chunks.

A key the manifest does not list does not exist. For a chunk key zarrs then uses the
array's fill value, which is how kerchunk represents never-written HDF5 chunks.

## Accepted manifest forms

- Version 0 (a bare reference map) and version 1 (`{"version": 1, "refs": {...}}`).
- `templates` in version 1 are substituted into reference paths.
- `gen` (generated references) is rejected with an error naming the feature. Expand
  the manifest with kerchunk first.
- `file://` URLs are stripped to plain paths.
- Bare `NaN` and `Infinity` tokens in metadata documents (Python's `json.dumps`
  default, common in `_FillValue`) are rewritten to the quoted strings the Zarr v2
  spec uses.

Kerchunk Parquet manifests and Icechunk virtual references are not supported yet.

## Codecs

Whatever the manifest's `.zarray` names must be a codec zarrs can decode. The cases
exercised by the test suite:

| Producer | Source | Filters / compressor | Status |
| --- | --- | --- | --- |
| VirtualiZarr `HDFParser` | NetCDF4 / HDF5 | `shuffle`, `zlib`, `fletcher32` | Reads. `fletcher32` checksums are not validated on manifest reads (see below). |
| VirtualiZarr `HDFParser` | NetCDF4 / HDF5, contiguous dataset | none | Reads as one whole-range reference. |
| virtual-tiff | Tiled GeoTIFF, Deflate | `imagecodecs_deflate` | Reads. The id is aliased to `zlib`. |
| virtual-tiff | Tiled GeoTIFF, ZSTD | `imagecodecs_zstd` | Reads. The id is aliased to `zstd`. |
| virtual-tiff | Stripped TIFF (one chunk per strip), LZW | `imagecodecs_lzw` | Reads. LZW is decoded by the extension's own codec (`lzw_codec.rs`, on the `weezl` crate): TIFF 6.0 LZW, MSB-first with the early code-width change. |
| virtual-tiff | TIFF with a horizontal or floating-point predictor, JPEG, WebP, PackBits | not available in zarrs | Not supported. |
| VirtualiZarr `HDFParser` | HDF5 scale-offset filter | rejected by the producer | Cannot be indexed today. |

Stripped TIFFs need no special handling: virtual-tiff makes each strip a chunk of
`[RowsPerStrip, width]`, so a whole-image strip (common in microscopy exports) is
one chunk and a plate with 8 strips is 8 chunks. Partial trailing strips are still
open on the producer side ([virtual-tiff#24](https://github.com/virtual-zarr/virtual-tiff/issues/24)).
virtual-tiff also fails on TIFFs that omit the `SamplesPerPixel` tag, which some
microscope software does ([async-tiff#318](https://github.com/developmentseed/async-tiff/issues/318));
a manifest built by hand from `tifffile`'s strip offsets reads fine.

Checksums are skipped for manifest reads because zarrs' `fletcher32` implementation
drops the trailing byte of odd-length payloads
([zarrs/zarrs#460](https://github.com/zarrs/zarrs/issues/460)), which would fail
correct HDF5 chunks. The switch is `meta::codec_options`; validation will be
re-enabled once the fix lands.

## Testing

- `test/sql/read_zarr_kerchunk.test` reads the fixtures below and asserts that each
  manifest returns exactly the rows of the same data written as a real Zarr store.
- `test/test_kerchunk_property.py` is a [Hypothesis](https://hypothesis.readthedocs.io/)
  suite: it generates random datasets and encodings, writes them as NetCDF4 and as
  Zarr, indexes the NetCDF4 with VirtualiZarr, and checks the extension reads the
  manifest and the store identically. Run it with `make test_kerchunk` (or
  `make test_kerchunk_deep` for a longer search).

Fixtures under `test/fixtures/xarray_tutorial/`, all built by
`scripts/generate_fixtures.py`:

| Fixture | What it covers |
| --- | --- |
| `kerchunk_netcdf4.json` (+ `.nc`, `_v2.zarr`) | HDF5 pipeline with shuffle, deflate and fletcher32; deflate only; a contiguous dataset; `_FillValue` masking; inline base64 coordinates. |
| `kerchunk_zarr_v2.json` (+ `_source.zarr`) | Hand-built manifest with whole-file `[path]` references over Zarr v2 chunk files, one chunk deliberately missing. |
| `kerchunk_cog_deflate.json`, `kerchunk_cog_zstd.json` (+ `.tif`, `_v2.zarr`) | virtual-tiff over tiled GeoTIFFs: Deflate float32 with a nodata tag, and 3-band int8 ZSTD with the `imagecodecs_zstd` id. |
| `kerchunk_tiff_lzw_strips.json` (+ `.tif`, `_v2.zarr`) | virtual-tiff over a stripped uint16 LZW image in the shape of a microscopy plate TIFF (Cell Painting style): one chunk per strip, `imagecodecs_lzw`. |
| `kerchunk_errors/` | Manifests that reference a missing file, a range past the end of a file, and a truncated chunk, for the error-path tests. |
