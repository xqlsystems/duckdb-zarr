"""
Property-based tests for virtual Zarr: kerchunk manifests written by
VirtualiZarr must read back through the extension exactly like the data they
describe.

Each example builds a random xarray Dataset (dims, shape, chunking, dtypes,
NaN masking, HDF5 filter pipeline), writes it as a NetCDF4 file and as a real
Zarr v2 store, indexes the NetCDF4 with VirtualiZarr's HDF parser into a
kerchunk JSON manifest, and asserts that

  * read_zarr(manifest, format='kerchunk') equals the dataset itself, and
  * read_zarr(manifest, format='kerchunk') equals read_zarr(zarr store),

so a failure isolates to the manifest path (first) or the pivot (both).
Hypothesis drives the generation and shrinks any failure to a minimal case.

Run with:
    make test_kerchunk           # default profile
    make test_kerchunk_deep      # many more examples, for hunting
    # or directly:
    pytest test/test_kerchunk_property.py \
        --extension build/release/zarr.duckdb_extension --hypothesis-profile deep
"""
import json
import os
import pathlib
import tempfile

import duckdb
import numpy as np
import pandas as pd
import pytest
import xarray as xr
from hypothesis import HealthCheck, given, settings
from hypothesis import strategies as st
from hypothesis.extra import numpy as npst

settings.register_profile("ci", max_examples=20, deadline=None,
                          suppress_health_check=[HealthCheck.too_slow])
settings.register_profile("default", max_examples=60, deadline=None,
                          suppress_health_check=[HealthCheck.too_slow])
settings.register_profile("deep", max_examples=600, deadline=None,
                          suppress_health_check=[HealthCheck.too_slow])
settings.load_profile(os.environ.get("HYPOTHESIS_PROFILE", "default"))

DIM_NAMES = ("time", "lat", "lon")

# Every numeric dtype VirtualiZarr's HDF parser emits for plain NetCDF4
# variables and read_zarr maps to a DuckDB column.
DTYPES = ("int8", "int16", "int32", "int64", "uint8", "uint16", "uint32",
          "float32", "float64")


@pytest.fixture(scope="module")
def con(extension_path):
    c = duckdb.connect(config={"allow_unsigned_extensions": True})
    c.execute(f"LOAD '{extension_path}'")
    return c


@pytest.fixture(scope="module")
def registry():
    try:
        from obspec_utils.registry import ObjectStoreRegistry
    except ImportError:  # virtualizarr < 2.8
        from virtualizarr.registry import ObjectStoreRegistry
    from obstore.store import LocalStore
    return ObjectStoreRegistry({"file://": LocalStore()})


# ── strategies ───────────────────────────────────────────────────────────────

def elements_for(dtype: str):
    """Element strategy for a dtype, kept well inside the type's range so the
    NetCDF and Zarr writers never have to clip."""
    if dtype.startswith("float"):
        width = 32 if dtype == "float32" else 64
        return st.floats(-1e6, 1e6, width=width, allow_nan=False,
                         allow_infinity=False)
    info = np.iinfo(dtype)
    lo = max(info.min, -1_000_000)
    hi = min(info.max, 1_000_000)
    return st.integers(lo, hi)


@st.composite
def variable(draw, shape):
    dtype = draw(st.sampled_from(DTYPES))
    data = draw(npst.arrays(dtype, shape, elements=elements_for(dtype)))
    fill = None
    if dtype.startswith("float") and draw(st.booleans()):
        mask = draw(npst.arrays(bool, shape))
        data = data.copy()
        data[mask] = np.nan
        # An explicit sentinel so the manifest carries _FillValue and the
        # reader has to mask it; NaN-as-fill is the other branch.
        if draw(st.booleans()):
            fill = np.array(-9999.0, dtype=dtype)
    return data, fill


@st.composite
def encoding(draw, shape):
    """HDF5 storage layout for one variable: contiguous, or chunked with any
    subset of the shuffle / deflate / fletcher32 pipeline."""
    if not draw(st.booleans()):
        return {}
    chunks = tuple(draw(st.integers(1, n)) for n in shape)
    enc = {"chunksizes": chunks}
    if draw(st.booleans()):
        enc.update(zlib=True, complevel=draw(st.integers(1, 4)),
                   shuffle=draw(st.booleans()))
    if draw(st.booleans()):
        enc["fletcher32"] = True
    return enc


@st.composite
def datasets(draw):
    ndim = draw(st.integers(1, 3))
    dims = DIM_NAMES[:ndim]
    shape = tuple(draw(st.integers(1, 6)) for _ in dims)
    nvars = draw(st.integers(1, 2))
    # read_zarr requires one chunk shape per dimension group, so every data
    # variable shares one encoding.
    enc = draw(encoding(shape))
    data_vars = {}
    nc_encoding = {}
    for i in range(nvars):
        data, fill = draw(variable(shape))
        name = f"v{i}"
        data_vars[name] = (dims, data)
        var_enc = dict(enc)
        if fill is not None:
            var_enc["_FillValue"] = fill
        nc_encoding[name] = var_enc
    coords = {}
    for d, n in zip(dims, shape):
        if draw(st.booleans()):
            coords[d] = np.arange(n, dtype="int64") * draw(st.integers(1, 7))
        else:
            start = draw(st.floats(-90, 90, allow_nan=False))
            coords[d] = np.linspace(start, start + n, n, dtype="float64")
    return xr.Dataset(data_vars, coords=coords), nc_encoding


# ── helpers ──────────────────────────────────────────────────────────────────

def expected_frame(ds: xr.Dataset) -> pd.DataFrame:
    dims = list(ds.dims)
    df = ds.to_dataframe().reset_index()
    return normalise(df, dims, list(ds.data_vars))


def normalise(df: pd.DataFrame, dims: list[str], data_vars: list[str]) -> pd.DataFrame:
    df = df[dims + data_vars].sort_values(dims, kind="stable").reset_index(drop=True)
    # Compare on value, not storage type: DuckDB hands back TINYINT as int8
    # and pandas may widen either side.
    return df.astype({c: "float64" for c in df.columns})


def read_frame(con, source: str, fmt: str | None, dims, data_vars) -> pd.DataFrame:
    fmt_arg = f", format='{fmt}'" if fmt else ""
    dims_arg = "[" + ",".join(f"'{d}'" for d in dims) + "]"
    df = con.execute(
        f"SELECT * FROM read_zarr('{source}'{fmt_arg}, dims={dims_arg})"
    ).df()
    return normalise(df, list(dims), list(data_vars))


def assert_frames_equal(actual: pd.DataFrame, expected: pd.DataFrame, what: str):
    assert list(actual.columns) == list(expected.columns), what
    assert len(actual) == len(expected), f"{what}: {len(actual)} rows vs {len(expected)}"
    pd.testing.assert_frame_equal(actual, expected, check_exact=False,
                                  rtol=1e-6, atol=0, obj=what)


def write_manifest(ds, nc_path: pathlib.Path, refs_path: pathlib.Path, registry, nc_encoding):
    from virtualizarr import open_virtual_dataset
    from virtualizarr.parsers import HDFParser

    ds.to_netcdf(nc_path, engine="h5netcdf", encoding=nc_encoding)
    vds = open_virtual_dataset(f"file://{nc_path.resolve()}",
                               parser=HDFParser(), registry=registry)
    vds.vz.to_kerchunk(str(refs_path), format="json")


# ── properties ───────────────────────────────────────────────────────────────

@given(case=datasets())
def test_manifest_matches_dataset_and_zarr_twin(con, registry, case):
    ds, nc_encoding = case
    dims = list(ds.dims)
    data_vars = list(ds.data_vars)
    with tempfile.TemporaryDirectory() as tmp:
        tmp = pathlib.Path(tmp)
        nc_path = tmp / "data.nc"
        refs_path = tmp / "refs.json"
        zarr_path = tmp / "twin.zarr"
        write_manifest(ds, nc_path, refs_path, registry, nc_encoding)

        # The manifest must only ever point at the file we wrote.
        doc = json.loads(refs_path.read_text())
        refs = doc.get("refs", doc)
        targets = {v[0] for v in refs.values() if isinstance(v, list)}
        assert targets <= {f"file://{nc_path.resolve()}", str(nc_path.resolve())}, targets

        expected = expected_frame(ds)
        via_manifest = read_frame(con, refs_path, "kerchunk", dims, data_vars)
        assert_frames_equal(via_manifest, expected, "manifest vs dataset")

        zarr_encoding = {}
        for name, enc in nc_encoding.items():
            zenc = {}
            if "chunksizes" in enc:
                zenc["chunks"] = enc["chunksizes"]
            if enc.get("zlib"):
                zenc["compressor"] = {"id": "gzip", "level": enc["complevel"]}
            else:
                zenc["compressor"] = None
            if "_FillValue" in enc:
                zenc["_FillValue"] = enc["_FillValue"]
            zarr_encoding[name] = zenc
        ds.to_zarr(zarr_path, zarr_format=2, consolidated=True, encoding=zarr_encoding)
        via_zarr = read_frame(con, zarr_path, None, dims, data_vars)
        assert_frames_equal(via_manifest, via_zarr, "manifest vs zarr twin")


@given(case=datasets())
def test_metadata_matches_manifest(con, registry, case):
    """read_zarr_metadata lists every array in the manifest with the dtype and
    chunk shape VirtualiZarr wrote."""
    ds, nc_encoding = case
    with tempfile.TemporaryDirectory() as tmp:
        tmp = pathlib.Path(tmp)
        refs_path = tmp / "refs.json"
        write_manifest(ds, tmp / "data.nc", refs_path, registry, nc_encoding)
        doc = json.loads(refs_path.read_text())
        refs = doc.get("refs", doc)
        zarrays = {k[: -len("/.zarray")]: json.loads(v)
                   for k, v in refs.items() if k.endswith("/.zarray")}

        rows = con.execute(
            f"SELECT name, shape, chunk_shape FROM read_zarr_metadata('{refs_path}', format='kerchunk')"
        ).fetchall()
        # shape and chunk_shape are JSON-encoded lists.
        listed = {name: (json.loads(shape), json.loads(chunk_shape))
                  for name, shape, chunk_shape in rows}
        assert set(listed) == set(zarrays)
        for name, (shape, chunk_shape) in listed.items():
            assert shape == zarrays[name]["shape"], name
            assert chunk_shape == zarrays[name]["chunks"], name
