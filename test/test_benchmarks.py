"""One opt-in native benchmark with values checked against NumPy."""

import hashlib
import json
import os
import subprocess
from importlib.metadata import version
from pathlib import Path

import duckdb
import numpy as np
import pytest
import xarray as xr

pytestmark = pytest.mark.benchmark
ROOT = Path(__file__).resolve().parent.parent


def provenance(extension_path, store, values):
    """Record the binary, checkout, fixture and runtime outside the timed query."""
    def git(*args):
        """Read checkout identity without exposing local filenames in the report."""
        return subprocess.check_output(
            ["git", *args], cwd=ROOT, text=True, timeout=5,
        ).strip()

    with extension_path.open("rb") as binary:
        extension_hash = hashlib.file_digest(binary, "sha256").hexdigest()
    metadata = json.loads((store / "temperature" / "zarr.json").read_text())
    return {
        "checkout_revision": git("rev-parse", "HEAD"),
        "checkout_dirty": bool(git("status", "--porcelain", "--untracked-files=normal")),
        "extension_sha256": extension_hash,
        "build_profile": os.environ.get("DUCKDB_ZARR_BENCHMARK_PROFILE", "unspecified"),
        "storage": "local_filesystem",
        "cache": "warm_repeated_query; no OS cache flush",
        "dataset": {
            "kind": "synthetic", "seed": 0, "shape": list(values.shape),
            "generator": "PCG64 signed 52-bit integers scaled by 2^-52",
            "chunks": metadata["chunk_grid"]["configuration"]["chunk_shape"],
            "zarr_format": metadata["zarr_format"], "dtype": str(values.dtype),
            "codecs": metadata["codecs"],
            "values_sha256": hashlib.sha256(values.astype("<f8", copy=False).tobytes()).hexdigest(),
        },
        "versions": {name: version(name) for name in (
            "duckdb", "numpy", "xarray", "zarr", "pytest-benchmark",
        )},
        "rustc": subprocess.check_output(["rustc", "--version"], text=True, timeout=5).strip(),
    }


@pytest.fixture(scope="module")
def grid_store(tmp_path_factory):
    """Create a deterministic local Zarr grid outside the timed query."""
    rng = np.random.Generator(np.random.PCG64(0))
    # Exact binary fractions avoid platform-specific floating distribution math.
    values = rng.integers(
        -(1 << 52), 1 << 52, size=(100, 50, 50), dtype=np.int64,
    ).astype(np.float64) * (2.0 ** -52)
    store = tmp_path_factory.mktemp("benchmark") / "grid.zarr"
    grid = xr.Dataset(
        {"temperature": (("time", "lat", "lon"), values)},
        coords={"time": np.arange(100), "lat": np.arange(50), "lon": np.arange(50)},
    )
    grid.to_zarr(
        store, zarr_format=3, consolidated=False,
        encoding={"temperature": {"chunks": (10, 25, 25)}},
    )
    return store, values


def test_groupby_mean_over_time(benchmark, grid_store, extension_path):
    """Time a complete local read, decode, aggregate and result materialization."""
    store, values = grid_store
    with duckdb.connect(config={"allow_unsigned_extensions": True}) as con:
        con.load_extension(str(extension_path))
        con.execute("SET threads = 1")
        benchmark.extra_info.update(provenance(extension_path, store, values))
        benchmark.extra_info["threads"] = con.execute("SELECT current_setting('threads')").fetchone()[0]

        def run():
            """Bind and execute the same query, returning all grouped values."""
            return con.execute(
                "SELECT lat, lon, AVG(temperature) AS mean_temperature "
                "FROM read_zarr(?) GROUP BY lat, lon ORDER BY lat, lon",
                [str(store)],
            ).fetchall()

        result = benchmark.pedantic(run, rounds=10, iterations=1, warmup_rounds=1)

    rows = np.asarray(result)
    assert rows.shape == (50 * 50, 3)
    np.testing.assert_array_equal(rows[:, 0], np.repeat(np.arange(50), 50))
    np.testing.assert_array_equal(rows[:, 1], np.tile(np.arange(50), 50))
    np.testing.assert_allclose(
        rows[:, 2].reshape(50, 50), values.mean(axis=0), rtol=1e-12, atol=1e-12,
    )
    assert benchmark.extra_info["dataset"]["shape"] == list(values.shape)
    assert benchmark.extra_info["threads"] == 1
    assert len(benchmark.extra_info["extension_sha256"]) == 64
    # Freeze the workload before collecting history, including across machines.
    expected_values_hash = "88df6a125832d307bd56087bb38804f855ffe93da6d2ca4ade2fbac57317a216"
    actual_values_hash = hashlib.sha256(values.astype("<f8", copy=False).tobytes()).hexdigest()
    assert actual_values_hash == expected_values_hash
    assert benchmark.extra_info["dataset"]["values_sha256"] == expected_values_hash
