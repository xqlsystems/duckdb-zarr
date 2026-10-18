"""Exercise coordinate bounds through the real DuckDB extension and Zarr writers."""

import anndata as ad
import duckdb
import numpy as np
import pytest
import xarray as xr


@pytest.mark.parametrize("zarr_format", [2, 3], ids=["zarr-v2", "zarr-v3"])
@pytest.mark.parametrize("threads", [1, 4])
def test_bounds_on_nested_table_with_ragged_mismatched_chunks(
    tmp_path, extension_path, zarr_format, threads,
):
    # The first plan chunk and selected result both exceed one DuckDB vector.
    x = np.arange(87, dtype=np.int64)
    y = np.arange(93, dtype=np.int64)[::-1]
    a = x[:, None] * 1000 + y[None, :]
    b = a + 100_000
    store = tmp_path / "nested.zarr"
    xr.Dataset(coords={"x": x, "y": y}).to_zarr(
        store, zarr_format=zarr_format, consolidated=False,
    )
    xr.Dataset(
        {"a": (("x", "y"), a), "b": (("x", "y"), b)},
    ).to_zarr(
        store, group="simulation/fine", mode="a", zarr_format=zarr_format,
        consolidated=False,
        encoding={"a": {"chunks": (55, 59)}, "b": {"chunks": (17, 23)}},
    )
    bounds = [["x", "10", "75"], ["y", "20", "80"]]
    expected = [
        (int(i), int(j), int(a[i, k]), int(b[i, k]))
        for i in range(len(x)) if 10 <= x[i] <= 75
        for k, j in enumerate(y) if 20 <= j <= 80
    ]
    expected.sort()
    with duckdb.connect(config={"allow_unsigned_extensions": True}) as con:
        con.load_extension(str(extension_path))
        con.execute(f"SET threads = {threads}")
        query = (
            "FROM read_zarr(?, group_path := 'simulation/fine', "
            "table_name := 'x_y', bounds := ?)"
        )
        assert con.execute(
            "SELECT x, y, a, b " + query + " ORDER BY x, y", [str(store), bounds],
        ).fetchall() == expected
        assert con.execute(
            "SELECT count(*) " + query, [str(store), bounds],
        ).fetchone() == (len(expected),)
        assert con.execute(
            "SELECT b " + query + " ORDER BY x, y", [str(store), bounds],
        ).fetchall() == [(row[3],) for row in expected]
        assert con.execute(
            "SELECT count(*) " + query, [str(store), [["x", "200", None]]],
        ).fetchone() == (0,)


@pytest.mark.parametrize("zarr_format", [2, 3], ids=["zarr-v2", "zarr-v3"])
def test_numeric_bounds_reject_string_coordinates(tmp_path, extension_path, zarr_format):
    store = tmp_path / "strings.zarr"
    xr.Dataset(
        {"signal": ("sample", [1, 2, 3])},
        coords={"sample": np.array(["a", "b", "c"], dtype=object)},
    ).to_zarr(store, zarr_format=zarr_format, consolidated=False)
    with duckdb.connect(config={"allow_unsigned_extensions": True}) as con:
        con.load_extension(str(extension_path))
        assert con.execute(
            "SELECT sample, signal FROM read_zarr(?) ORDER BY sample", [str(store)],
        ).fetchall() == [("a", 1), ("b", 2), ("c", 3)]
        with pytest.raises(duckdb.Error, match="numeric|not supported"):
            con.execute(
                "SELECT * FROM read_zarr(?, bounds := [['sample', '0', '99']])",
                [str(store)],
            ).fetchall()


@pytest.mark.parametrize("dtype", ["int64", "uint64"])
@pytest.mark.parametrize("large", ["coordinate", "endpoint"])
def test_bounds_reject_lossy_integer_comparisons(tmp_path, extension_path, dtype, large):
    # f64 rounds 2**53 + 1 down to 2**53, which can include the wrong row.
    edge = 2**53
    coordinates = [edge + 1] if large == "coordinate" else [0]
    store = tmp_path / "integers.zarr"
    xr.Dataset(
        {"signal": ("sample", [7])},
        coords={"sample": np.array(coordinates, dtype=dtype)},
    ).to_zarr(store, zarr_format=3, consolidated=False)
    bounds = [["sample", str(edge), str(edge)]] if large == "coordinate" else [
        ["sample", str(edge + 1), str(edge + 1)]
    ]
    with duckdb.connect(config={"allow_unsigned_extensions": True}) as con:
        con.load_extension(str(extension_path))
        assert con.execute(
            "SELECT sample FROM read_zarr(?)", [str(store)],
        ).fetchall() == [(coordinates[0],)]
        with pytest.raises(duckdb.Error, match="exact|precision"):
            con.execute(
                "SELECT * FROM read_zarr(?, bounds := ?)", [str(store), bounds],
            ).fetchall()


@pytest.mark.parametrize("bound_kind", ["endpoint", "coordinate"])
def test_timestamp_bounds_reject_microsecond_rounding(tmp_path, extension_path, bound_kind):
    edge = 2**53
    store = tmp_path / "time.zarr"
    xr.Dataset(
        {"signal": ("time", [6, 7, 8])},
        coords={"time": (
            "time", np.array([edge - 1, edge, edge + 1], dtype="int64"),
            {"units": "microseconds since 1970-01-01 00:00:00",
             "calendar": "proleptic_gregorian"},
        )},
    ).to_zarr(store, zarr_format=3, consolidated=False)
    stamp = "2255-06-05 23:47:34.740992"
    bounds = [["time", stamp, stamp]] if bound_kind == "endpoint" else [
        ["time", "2255-06-05 23:47:34.740991", None]
    ]
    with duckdb.connect(config={"allow_unsigned_extensions": True}) as con:
        con.load_extension(str(extension_path))
        assert con.execute(
            "SELECT signal FROM read_zarr(?) WHERE time = ?::TIMESTAMP",
            [str(store), stamp],
        ).fetchall() == [(7,)]
        with pytest.raises(duckdb.Error, match="precision"):
            con.execute(
                "SELECT signal FROM read_zarr(?, bounds := ?)", [str(store), bounds],
            ).fetchall()


@pytest.mark.parametrize("storage", ["dense", "sparse"])
def test_numeric_bounds_reject_anndata_names(tmp_path, extension_path, storage):
    # AnnData row names are labels, not numeric positions or sortable numeric axes.
    import scipy.sparse as sp

    values = np.array([[0, 2], [3, 0]], dtype=np.float32)
    adata = ad.AnnData(sp.csr_matrix(values) if storage == "sparse" else values)
    adata.obs_names = ["cell_a", "cell_b"]
    adata.var_names = ["gene_a", "gene_b"]
    store = tmp_path / "anndata.zarr"
    adata.write_zarr(store)
    with duckdb.connect(config={"allow_unsigned_extensions": True}) as con:
        con.load_extension(str(extension_path))
        with pytest.raises(duckdb.Error, match="numeric|not supported"):
            con.execute(
                "SELECT * FROM read_zarr(?, table_name := 'obs_var', "
                "bounds := [['obs', '0', '99']])", [str(store)],
            ).fetchall()
