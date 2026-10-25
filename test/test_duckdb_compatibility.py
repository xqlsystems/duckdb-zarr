"""Check the real extension's C API contract and DuckDB release mapping."""

import importlib.util
import json
from pathlib import Path
import struct

import duckdb
import pytest

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location(
    "check_release_ready", ROOT / "scripts/check_release_ready.py",
)
assert SPEC is not None and SPEC.loader is not None
release_ready = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(release_ready)


@pytest.mark.parametrize("engine,crate", [
    ("v1.5.5", "1.10505.0"),
    ("v1.5.6", "1.10506.0"),
    ("v2.0.0", "1.20000.0"),
    ("v2.0.1", "1.20001.0"),
])
def test_duckdb_crate_version(engine, crate):
    assert release_ready.duckdb_crate_version(engine) == crate


def test_extension_uses_v1_compatibility_metadata(extension_path, tmp_path):
    # The last 512 bytes hold eight 32-byte metadata fields and a 256-byte
    # signature. C_STRUCT_UNSTABLE selects the v2 entrypoint on DuckDB 2.0;
    # this adapter deliberately retains the frozen v1 C API instead.
    with extension_path.open("rb") as binary:
        binary.seek(-512, 2)
        footer = binary.read(256)
    fields = [footer[i:i + 32].rstrip(b"\x00").decode() for i in range(0, 256, 32)]
    assert fields[3] == "C_STRUCT", fields
    assert fields[5] == "v1.5.6", fields
    assert fields[7] == "4", fields

    # A real, uncompressed Zarr v2 store, written using only the stdlib so this
    # isolated CI job needs neither downloaded fixtures nor zarr/xarray.
    store = tmp_path / "sample.zarr"
    store.mkdir()
    (store / ".zgroup").write_text(json.dumps({"zarr_format": 2}))
    for name, values in [("x", [10, 20, 30]), ("signal", [7, 11, 19])]:
        array = store / name
        array.mkdir()
        (array / ".zarray").write_text(json.dumps({
            "zarr_format": 2, "shape": [3], "chunks": [3], "dtype": "<i8",
            "compressor": None, "fill_value": 0, "order": "C", "filters": None,
        }))
        (array / ".zattrs").write_text(json.dumps({"_ARRAY_DIMENSIONS": ["x"]}))
        (array / "0").write_bytes(struct.pack("<3q", *values))

    with duckdb.connect(config={"allow_unsigned_extensions": True}) as con:
        con.load_extension(str(extension_path))
        assert con.execute(
            "SELECT loaded FROM duckdb_extensions() WHERE extension_name = 'zarr'",
        ).fetchone() == (True,)
        # Exercise metadata discovery, chunk decoding, and SQL filtering too;
        # successful LOAD alone does not establish query-time compatibility.
        assert con.execute(
            "SELECT x, signal FROM read_zarr(?) WHERE signal >= 11 ORDER BY x",
            [str(store)],
        ).fetchall() == [(20, 11), (30, 19)]
