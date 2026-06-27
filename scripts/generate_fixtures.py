#!/usr/bin/env python3
"""
Generate Zarr v3 test fixtures from xarray tutorial datasets.

Each fixture exercises one or more code paths in the Rust reader.
See docs/design.md §Bind phase and §Type mapping for the requirements.

Usage:
    uv run scripts/generate_fixtures.py

Output:
    test/fixtures/xarray_tutorial/<name>.zarr
    test/fixtures/bioimage/ome_zarr/<name>.ome.zarr

Note on base64-encoded _FillValue: xarray encodes ALL float _FillValue attrs
as base64 when writing zarr v3 — including non-NaN values like -9.97e36.
The encoding is always 8 bytes of IEEE 754 float64, little-endian. Example:
  NaN       -> "AAAAAAAA+H8="
  -9.97e36  -> "AAAAAAAAnsc="
This is an xarray convention; zarr-python itself uses plain "NaN" string in
zarr.json fill_value and plain JSON numbers in attrs. missing_value attr stays
as a plain JSON number (not base64). The Rust reader must handle _FillValue as
base64 when it encounters a string value; for NaN sentinels use is_nan().
"""
import contextlib
import os
import pathlib
import shutil
import subprocess
import time

# zarr 2.x gates v3 writes behind an env var; zarr 3.x ignores it.
# Must be set before zarr is imported.
os.environ.setdefault('ZARR_V3_EXPERIMENTAL_API', '1')


def _rmtree(path: pathlib.Path) -> None:
    """Remove a directory tree, handling root-owned files from Docker CI steps.

    Strategy:
    1. chmod parent dir (unlink needs parent write perm) + file, both best-effort.
    2. Call the original func (unlink/rmdir).
    3. If shutil.rmtree still raises PermissionError, fall back to
       `sudo rm -rf` — passwordless on GitHub Actions Ubuntu runners.
    """
    if not path.exists():
        return

    def _fix_readonly(func, fpath, _):
        for p in (os.path.dirname(fpath), fpath):
            with contextlib.suppress(OSError):
                os.chmod(p, 0o755)
        func(fpath)

    try:
        shutil.rmtree(path, onerror=_fix_readonly)
    except PermissionError:
        subprocess.run(['sudo', 'rm', '-rf', str(path)], check=False)

import numpy as np
import xarray as xr
import zarr

ROOT = pathlib.Path(__file__).parent.parent
FIXTURES = ROOT / "test" / "fixtures" / "xarray_tutorial"
BIOIMAGE_FIXTURES = ROOT / "test" / "fixtures" / "bioimage" / "ome_zarr"


def write_zarr(ds: xr.Dataset, name: str, encoding: dict | None = None) -> None:
    dest = FIXTURES / f"{name}.zarr"
    if (dest / "zarr.json").exists():
        print(f"  (cached) {dest}")
        return
    if dest.exists():
        _rmtree(dest)
    ds.to_zarr(dest, zarr_format=3, consolidated=False, encoding=encoding or {})
    print(f"  wrote {dest}")


def ensure_attr(ds: xr.Dataset, var: str, key: str, value) -> xr.Dataset:
    """Add key=value to da.attrs; restores attrs xarray moved to encoding."""
    da = ds[var].copy()
    da.attrs[key] = value
    if var in ds.coords:
        return ds.assign_coords({var: da})
    return ds.assign({var: da})


def fixture_exists(name: str) -> bool:
    """True if the fixture is already materialised (cached from a prior run)."""
    return (FIXTURES / f"{name}.zarr" / "zarr.json").exists()


def open_tutorial(name: str, **kwargs) -> xr.Dataset:
    """Download an xarray tutorial dataset, retrying transient network failures.

    xarray.tutorial fetches from github pydata/xarray-data via pooch; GitHub raw
    intermittently returns 5xx, which would otherwise fail the whole CI build on
    a cold fixture cache. Retry with exponential backoff before giving up.
    """
    last_exc: Exception | None = None
    for attempt in range(5):
        try:
            return xr.tutorial.open_dataset(name, **kwargs)
        except Exception as exc:  # pooch surfaces requests.HTTPError / URLError
            last_exc = exc
            wait = 2 ** attempt
            print(f"  download of {name!r} failed ({exc}); "
                  f"retry {attempt + 1}/5 in {wait}s")
            time.sleep(wait)
    raise last_exc


def main() -> None:
    FIXTURES.mkdir(parents=True, exist_ok=True)
    BIOIMAGE_FIXTURES.mkdir(parents=True, exist_ok=True)

    # ── synthetic_multichannel (OME-Zarr bioimage) ──────────────────────────
    # A minimal two-channel microscopy image with OME multiscales metadata.
    # Writing it through xarray records named dimensions in the Zarr v3 array,
    # which read_zarr exposes directly as ergonomic c/y/x SQL columns.
    print("synthetic_multichannel (OME-Zarr bioimage)...")
    dest = BIOIMAGE_FIXTURES / "synthetic_multichannel.ome.zarr"
    if not (dest / "zarr.json").exists():
        if dest.exists():
            _rmtree(dest)
        pixels = np.concatenate([
            np.arange(1, 13, dtype="uint16").reshape(1, 3, 4),
            np.arange(101, 113, dtype="uint16").reshape(1, 3, 4),
        ])
        image = xr.DataArray(
            pixels,
            dims=["c", "y", "x"],
            attrs={"long_name": "synthetic two-channel microscopy image"},
        )
        ds_bioimage = xr.Dataset(
            {"0": image},
            attrs={
                "multiscales": [{
                    "version": "0.5",
                    "name": "synthetic_multichannel",
                    "axes": [
                        {"name": "c", "type": "channel"},
                        {"name": "y", "type": "space", "unit": "micrometer"},
                        {"name": "x", "type": "space", "unit": "micrometer"},
                    ],
                    "datasets": [{
                        "path": "0",
                        "coordinateTransformations": [{
                            "type": "scale",
                            "scale": [1.0, 0.65, 0.65],
                        }],
                    }],
                }],
                "omero": {
                    "name": "synthetic_multichannel",
                    "channels": [
                        {"label": "DNA", "color": "0000FF"},
                        {"label": "RNA", "color": "FFFF00"},
                    ],
                },
            },
        )
        ds_bioimage.to_zarr(
            dest,
            zarr_format=3,
            consolidated=False,
            encoding={"0": {"chunks": [1, 2, 2]}},
        )
        print(f"  wrote {dest}")
    else:
        print(f"  (cached) {dest}")

    # Add a nested OME-Zarr labels hierarchy. This is a dense integer label
    # image, not a ragged array: each pixel stores 0 for background or an object
    # id. Keeping this as a separate xarray write exercises recursive discovery
    # and store-relative array selection.
    label_path = dest / "labels" / "nuclei" / "0" / "zarr.json"
    if not label_path.exists():
        xr.Dataset(attrs={"labels": ["nuclei"]}).to_zarr(
            dest,
            group="labels",
            mode="a",
            zarr_format=3,
            consolidated=False,
        )
        labels = xr.DataArray(
            np.array([
                [0, 1, 1, 0],
                [0, 1, 2, 2],
                [0, 0, 2, 0],
            ], dtype="uint8"),
            dims=["y", "x"],
            attrs={"image-label": {"source": {"image": "../../"}}},
        )
        xr.Dataset(
            {"0": labels},
            attrs={
                "multiscales": [{
                    "version": "0.5",
                    "name": "nuclei",
                    "axes": [
                        {"name": "y", "type": "space", "unit": "micrometer"},
                        {"name": "x", "type": "space", "unit": "micrometer"},
                    ],
                    "datasets": [{"path": "0"}],
                }],
            },
        ).to_zarr(
            dest,
            group="labels/nuclei",
            mode="a",
            zarr_format=3,
            consolidated=False,
            encoding={"0": {"chunks": [2, 2]}},
        )
        print(f"  wrote nested labels to {dest}")

    # ── float_baseline (synthetic) ───────────────────────────────────────────
    # True float32 baseline with no packing, no sentinels.
    # Tests: basic read_zarr, coord classification, plain numeric copy path.
    # (air_temperature is actually int16+scale_factor on disk — see below.)
    print("float_baseline (synthetic)...")
    rng = np.random.default_rng(0)
    data = rng.standard_normal((8, 6, 12)).astype("float32")
    lat = np.linspace(-90.0, 90.0, 6)
    lon = np.linspace(0.0, 360.0, 12, endpoint=False)
    time = np.arange(8, dtype="int64")
    da = xr.DataArray(data, dims=["time", "lat", "lon"],
                      coords={"time": time, "lat": lat, "lon": lon},
                      attrs={"units": "K", "long_name": "temperature"})
    write_zarr(xr.Dataset({"temperature": da}), "float_baseline")

    # ── air_temperature ──────────────────────────────────────────────────────
    # On-disk dtype is int16 with scale_factor=0.01 (xarray re-encodes using the
    # original NetCDF encoding dict). Tests: packed int16 decoding, coord
    # classification, CF-encoded time passthrough (float32 units/calendar).
    if fixture_exists("air_temperature"):
        print("air_temperature... (cached)")
    else:
        print("air_temperature...")
        ds = open_tutorial("air_temperature")
        write_zarr(ds, "air_temperature")

    # ── air_temperature_gradient (synthetic, small) ──────────────────────────
    # Tests: scale_factor/add_offset packed decoding (Tair as int16), PLUS
    # mismatched-chunk-shape bind error (Tair chunk [1,4,6] != dTdx chunk [2,4,6]).
    # Uses a small synthetic array so the fixture stays tiny; the bind error fires
    # before any chunk is read, so shape matters but actual data does not.
    print("air_temperature_gradient (synthetic, small)...")
    rng = np.random.default_rng(1)
    shape = (4, 4, 6)
    lat = np.linspace(-90.0, 90.0, shape[1])
    lon = np.linspace(0.0, 360.0, shape[2], endpoint=False)
    time = np.arange(shape[0], dtype="int64")
    coords = {"time": time, "lat": lat, "lon": lon}
    dims = ["time", "lat", "lon"]
    # Tair: decode from int16 with scale_factor; chunked [1, 4, 6]
    Tair_raw = (rng.standard_normal(shape) * 100).astype("float64")
    # dTdx / dTdy: native float32; chunked [2, 4, 6] — different from Tair
    dTdx = rng.standard_normal(shape).astype("float32")
    dTdy = rng.standard_normal(shape).astype("float32")
    ds_grad = xr.Dataset({
        "Tair": xr.DataArray(Tair_raw, dims=dims, coords=coords,
                             attrs={"units": "degK", "long_name": "Air temperature"}),
        "dTdx": xr.DataArray(dTdx, dims=dims, coords=coords,
                             attrs={"units": "degK/m"}),
        "dTdy": xr.DataArray(dTdy, dims=dims, coords=coords,
                             attrs={"units": "degK/m"}),
    })
    encoding_grad = {
        "Tair": {"dtype": "int16", "scale_factor": np.float64(0.01),
                 "_FillValue": np.int16(-32767), "chunks": [1, 4, 6]},
        "dTdx": {"dtype": "float32", "chunks": [2, 4, 6]},
        "dTdy": {"dtype": "float32", "chunks": [2, 4, 6]},
    }
    write_zarr(ds_grad, "air_temperature_gradient", encoding=encoding_grad)

    # ── ersstv5 ──────────────────────────────────────────────────────────────
    # Tests: CF bounds variable suppression (time.bounds='time_bnds', shape (624,2));
    # also missing_value sentinel masking on sst.
    if fixture_exists("ersstv5"):
        print("ersstv5... (cached)")
    else:
        print("ersstv5...")
        ds = open_tutorial("ersstv5", mask_and_scale=False)
        if "missing_value" not in ds["sst"].attrs and "missing_value" in ds["sst"].encoding:
            ds = ensure_attr(ds, "sst", "missing_value", ds["sst"].encoding["missing_value"])
        if "_FillValue" not in ds["sst"].attrs and "_FillValue" in ds["sst"].encoding:
            ds = ensure_attr(ds, "sst", "_FillValue", ds["sst"].encoding["_FillValue"])
        # Add explicit bounds attr — the raw NetCDF omits it but time_bnds is present.
        # Exercises the primary CF bounds detection path (attr-based); the name-pattern
        # fallback is exercised by any store that has a *_bnds variable without the attr.
        ds = ensure_attr(ds, "time", "bounds", "time_bnds")
        write_zarr(ds, "ersstv5")

    # ── basin_mask ───────────────────────────────────────────────────────────
    # Tests: missing_value=-100 (int8) with no _FillValue — the "missing_value
    # only" branch in NULL masking precedence.
    if fixture_exists("basin_mask"):
        print("basin_mask... (cached)")
    else:
        print("basin_mask...")
        ds = open_tutorial("basin_mask", mask_and_scale=False)
        if "missing_value" not in ds["basin"].attrs and "missing_value" in ds["basin"].encoding:
            ds = ensure_attr(ds, "basin", "missing_value", ds["basin"].encoding["missing_value"])
        write_zarr(ds, "basin_mask")

    # ── rasm ─────────────────────────────────────────────────────────────────
    # Tests: noleap calendar CF-encoded time (raw on-disk), 2D non-dim coords
    # (xc, yc) which should trigger the v1 bind error for non-dimension coords.
    # Skipped: requires cftime for the noleap calendar; support deferred.
    # print("rasm...")
    # ds = xr.tutorial.open_dataset("rasm")
    # write_zarr(ds, "rasm")

    # ── unindexed_dim (synthetic) ────────────────────────────────────────────
    # Tests: dimension with no backing coordinate array. dim_0 has no coord;
    # the reader synthesizes 0..5 (arange). lat and lon are explicit coords.
    print("unindexed_dim (synthetic)...")
    rng = np.random.default_rng(42)
    data = rng.standard_normal((5, 4, 6)).astype("float32")
    lat = np.linspace(-90.0, 90.0, 4)
    lon = np.linspace(0.0, 360.0, 6, endpoint=False)
    da = xr.DataArray(data, dims=["dim_0", "lat", "lon"],
                      coords={"lat": lat, "lon": lon})
    write_zarr(xr.Dataset({"values": da}), "unindexed_dim")

    # ── scalar_coord (synthetic) ─────────────────────────────────────────────
    # Tests: scalar (0-dim) coordinate variables (e.g. ROMS hc, Vtransform).
    # These should be excluded from the row schema and surfaced in metadata.
    print("scalar_coord (synthetic)...")
    rng = np.random.default_rng(7)
    data = rng.standard_normal((3, 4)).astype("float32")
    lat = np.array([10.0, 20.0, 30.0, 40.0])
    time = np.arange(3, dtype="int64")
    da = xr.DataArray(data, dims=["time", "lat"],
                      coords={"time": time, "lat": lat})
    ds = xr.Dataset(
        {"temperature": da},
        coords={
            "hc": xr.DataArray(np.float64(250.0), attrs={"long_name": "critical depth"}),
            "Vtransform": xr.DataArray(np.int32(2), attrs={"long_name": "vertical transform"}),
        },
    )
    write_zarr(ds, "scalar_coord")

    # ── multi_dim_group (synthetic) ──────────────────────────────────────────
    # Tests: one table per distinct dimension set — the design's headline feature.
    # Surface group (time, lat, lon): t2m, sp.
    # Atmosphere group (time, level, lat, lon): temp, u, v.
    # read_zarr without dims= should error (multiple groups); with dims= resolves.
    print("multi_dim_group (synthetic, ERA5-mini)...")
    rng = np.random.default_rng(3)
    nt, nlat, nlon, nlev = 4, 5, 8, 3
    time = np.arange(nt, dtype="int64")
    lat = np.linspace(90.0, -90.0, nlat)   # descending, ERA5-style
    lon = np.linspace(0.0, 360.0, nlon, endpoint=False)
    level = np.array([1000, 850, 500], dtype="float32")

    surf_dims = ["time", "lat", "lon"]
    surf_coords = {"time": time, "lat": lat, "lon": lon}
    atm_dims = ["time", "level", "lat", "lon"]
    atm_coords = {"time": time, "level": level, "lat": lat, "lon": lon}

    ds_multi = xr.Dataset({
        "t2m": xr.DataArray(
            rng.standard_normal((nt, nlat, nlon)).astype("float32"),
            dims=surf_dims, coords=surf_coords,
            attrs={"units": "K", "long_name": "2m temperature"}),
        "sp": xr.DataArray(
            (rng.standard_normal((nt, nlat, nlon)) * 1000 + 101325).astype("float32"),
            dims=surf_dims, coords=surf_coords,
            attrs={"units": "Pa", "long_name": "surface pressure"}),
        "temp": xr.DataArray(
            rng.standard_normal((nt, nlev, nlat, nlon)).astype("float32"),
            dims=atm_dims, coords=atm_coords,
            attrs={"units": "K", "long_name": "temperature"}),
        "u": xr.DataArray(
            rng.standard_normal((nt, nlev, nlat, nlon)).astype("float32"),
            dims=atm_dims, coords=atm_coords,
            attrs={"units": "m/s", "long_name": "u-wind"}),
        "v": xr.DataArray(
            rng.standard_normal((nt, nlev, nlat, nlon)).astype("float32"),
            dims=atm_dims, coords=atm_coords,
            attrs={"units": "m/s", "long_name": "v-wind"}),
    })
    write_zarr(ds_multi, "multi_dim_group")

    # ── sparse (synthetic) ───────────────────────────────────────────────────
    # Tests: implicit (missing) chunks — zarrs returns "chunk not found"; the
    # reader must synthesize a fill-value chunk from zarr.json fill_value.
    # Only the first quadrant (chunk [0,0]) is written; the other 3 are absent.
    print("sparse (synthetic)...")
    dest = FIXTURES / "sparse.zarr"
    if (dest / "zarr.json").exists():
        print(f"  (cached) {dest}")
    else:
        if dest.exists():
            _rmtree(dest)
        store = zarr.open_group(str(dest), mode="w", zarr_format=3)
        lat_vals = np.linspace(-90.0, 90.0, 4)
        lon_vals = np.linspace(0.0, 360.0, 4, endpoint=False)
        lat_arr = store.create_array("lat", shape=(4,), chunks=(4,), dtype="float64")
        lat_arr[:] = lat_vals
        lon_arr = store.create_array("lon", shape=(4,), chunks=(4,), dtype="float64")
        lon_arr[:] = lon_vals
        data_arr = store.create_array(
            "data", shape=(4, 4), chunks=(2, 2), dtype="float32", fill_value=-9999.0,
        )
        # Write only chunk [0,0]; leave [0,1], [1,0], [1,1] implicit.
        data_arr[0:2, 0:2] = np.ones((2, 2), dtype="float32")
        data_arr.attrs["_ARRAY_DIMENSIONS"] = ["lat", "lon"]
        lat_arr.attrs["_ARRAY_DIMENSIONS"] = ["lat"]
        lon_arr.attrs["_ARRAY_DIMENSIONS"] = ["lon"]
        print(f"  wrote {dest}")

    # ── big_endian (synthetic) ───────────────────────────────────────────────
    # Tests: big-endian arrays — zarrs decodes to native byte order, so the copy
    # into DuckDB DataChunk is always a memcpy; this fixture catches any path that
    # interprets bytes before zarrs normalises endianness.
    print("big_endian (synthetic)...")
    dest = FIXTURES / "big_endian.zarr"
    if (dest / "zarr.json").exists():
        print(f"  (cached) {dest}")
    else:
        if dest.exists():
            _rmtree(dest)
        store = zarr.open_group(str(dest), mode="w", zarr_format=3)
        lat_vals = np.linspace(-90.0, 90.0, 4)
        lon_vals = np.linspace(0.0, 360.0, 6, endpoint=False)
        lat_arr = store.create_array("lat", shape=(4,), chunks=(4,), dtype="float64")
        lat_arr[:] = lat_vals
        lon_arr = store.create_array("lon", shape=(6,), chunks=(6,), dtype="float64")
        lon_arr[:] = lon_vals
        rng = np.random.default_rng(99)
        data = rng.standard_normal((4, 6)).astype("float32")
        temp_arr = store.create_array(
            "temperature",
            shape=(4, 6),
            chunks=(4, 6),
            dtype="float32",
            fill_value=np.float32(0.0),
            # Explicitly request big-endian bytes codec — zarr-python 3.x ignores
            # the ">f4" dtype prefix and defaults to little-endian otherwise.
            serializer=zarr.codecs.BytesCodec(endian="big"),
            compressors=[zarr.codecs.ZstdCodec()],
        )
        temp_arr[:] = data
        temp_arr.attrs["_ARRAY_DIMENSIONS"] = ["lat", "lon"]
        temp_arr.attrs["units"] = "K"
        lat_arr.attrs["_ARRAY_DIMENSIONS"] = ["lat"]
        lon_arr.attrs["_ARRAY_DIMENSIONS"] = ["lon"]
        print(f"  wrote {dest}")

    # ── blosc_compressed (synthetic) ─────────────────────────────────────────
    # Tests: Blosc/LZ4 codec support. float32 data compressed with BloscCodec.
    # Shape (4, 6) → 24 rows; values known so we can check exact sums.
    print("blosc_compressed (synthetic)...")
    dest = FIXTURES / "blosc_compressed.zarr"
    if (dest / "zarr.json").exists():
        print(f"  (cached) {dest}")
    else:
        if dest.exists():
            _rmtree(dest)
        store = zarr.open_group(str(dest), mode="w", zarr_format=3)
        lat_vals = np.linspace(-90.0, 90.0, 4)
        lon_vals = np.linspace(0.0, 360.0, 6, endpoint=False)
        lat_arr = store.create_array("lat", shape=(4,), chunks=(4,), dtype="float64")
        lat_arr[:] = lat_vals
        lon_arr = store.create_array("lon", shape=(6,), chunks=(6,), dtype="float64")
        lon_arr[:] = lon_vals
        rng = np.random.default_rng(42)
        data = rng.standard_normal((4, 6)).astype("float32")
        temp_arr = store.create_array(
            "temperature",
            shape=(4, 6),
            chunks=(4, 6),
            dtype="float32",
            fill_value=np.float32(0.0),
            compressors=[zarr.codecs.BloscCodec(cname="lz4", clevel=5)],
        )
        temp_arr[:] = data
        temp_arr.attrs["_ARRAY_DIMENSIONS"] = ["lat", "lon"]
        temp_arr.attrs["units"] = "K"
        lat_arr.attrs["_ARRAY_DIMENSIONS"] = ["lat"]
        lon_arr.attrs["_ARRAY_DIMENSIONS"] = ["lon"]
        print(f"  wrote {dest}")
        print(f"  temperature sum (for test): {float(data.sum()):.6f}")

    # ── blosc_multichunk (synthetic) ─────────────────────────────────────────
    # Tests: Blosc codec across multiple chunks — exercises chunk-boundary
    # reassembly and correct zarrs_stride math for non-square boundary chunks.
    # Shape (8, 12), chunks (4, 6) → 4 chunks (2×2 grid), 96 rows total.
    print("blosc_multichunk (synthetic)...")
    dest = FIXTURES / "blosc_multichunk.zarr"
    if (dest / "zarr.json").exists():
        print(f"  (cached) {dest}")
    else:
        if dest.exists():
            _rmtree(dest)
        store = zarr.open_group(str(dest), mode="w", zarr_format=3)
        lat_vals = np.linspace(-90.0, 90.0, 8)
        lon_vals = np.linspace(0.0, 360.0, 12, endpoint=False)
        lat_arr = store.create_array("lat", shape=(8,), chunks=(8,), dtype="float64")
        lat_arr[:] = lat_vals
        lon_arr = store.create_array("lon", shape=(12,), chunks=(12,), dtype="float64")
        lon_arr[:] = lon_vals
        rng = np.random.default_rng(77)
        data = rng.standard_normal((8, 12)).astype("float32")
        temp_arr = store.create_array(
            "temperature",
            shape=(8, 12),
            chunks=(4, 6),
            dtype="float32",
            fill_value=np.float32(0.0),
            compressors=[zarr.codecs.BloscCodec(cname="lz4", clevel=5)],
        )
        temp_arr[:] = data
        temp_arr.attrs["_ARRAY_DIMENSIONS"] = ["lat", "lon"]
        temp_arr.attrs["units"] = "K"
        lat_arr.attrs["_ARRAY_DIMENSIONS"] = ["lat"]
        lon_arr.attrs["_ARRAY_DIMENSIONS"] = ["lon"]
        print(f"  wrote {dest}")
        print(f"  temperature sum (for test): {float(data.sum()):.6f}")
        print(f"  temperature[0,0] (for test): {float(data[0,0]):.6f}")
        print(f"  temperature[4,6] (for test): {float(data[4,6]):.6f}")

    # ── float_baseline_v2 ────────────────────────────────────────────────────
    # Same data as float_baseline but written as Zarr v2 (.zarray/.zattrs).
    # Tests: v2 store detection in list_array_names + replacement scan.
    print("float_baseline_v2 (zarr v2)...")
    dest = FIXTURES / "float_baseline_v2.zarr"
    if (dest / ".zattrs").exists():
        print(f"  (cached) {dest}")
    else:
        if dest.exists():
            _rmtree(dest)
        rng = np.random.default_rng(0)
        data = rng.standard_normal((8, 6, 12)).astype("float32")
        lat = np.linspace(-90.0, 90.0, 6)
        lon = np.linspace(0.0, 360.0, 12, endpoint=False)
        time = np.arange(8, dtype="int64")
        da = xr.DataArray(data, dims=["time", "lat", "lon"],
                          coords={"time": time, "lat": lat, "lon": lon},
                          attrs={"units": "K", "long_name": "temperature"})
        # Use gzip instead of the default blosc; blosc build fails on macOS Tahoe.
        v2_enc = {v: {"compressor": {"id": "gzip", "level": 1}}
                  for v in ("temperature", "lat", "lon", "time")}
        xr.Dataset({"temperature": da}).to_zarr(
            dest, zarr_format=2, consolidated=False, encoding=v2_enc)
        print(f"  wrote {dest}")

    print("\nAll fixtures written.")


if __name__ == "__main__":
    main()
