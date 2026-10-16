"""End-to-end AnnData regressions using real writers and temporary stores.

Run with::

    python -m pytest test/test_anndata_edge_cases.py \
        --extension build/debug/zarr.duckdb_extension

The nested layout mirrors SpatialData's ``tables/<name>`` AnnData placement;
no SpatialData dependency or pre-generated fixture stores are required.
"""

import anndata as ad
import duckdb
import numpy as np
import pandas as pd
import pytest
import scipy.sparse as sp
import zarr


@pytest.fixture(scope="module")
def con(extension_path):
    connection = duckdb.connect(config={"allow_unsigned_extensions": True})
    connection.load_extension(str(extension_path))
    yield connection
    connection.close()


@pytest.fixture(params=[2, 3], ids=["zarr-v2", "zarr-v3"])
def zarr_format(request):
    return request.param


@pytest.fixture(params=["", "tables/cells"], ids=["anndata", "nested-table"])
def table_path(request):
    return request.param


def write_table(tmp_path, adata, zarr_format, table_path):
    """Use AnnData's element writer at either the root or a nested table."""
    path = tmp_path / "edge_cases.zarr"
    root = zarr.open_group(str(path), mode="w", zarr_format=zarr_format)
    with ad.settings.override(allow_write_nullable_strings=True):
        if table_path:
            parent, name = table_path.rsplit("/", 1)
            ad.io.write_elem(root.require_group(parent), name, adata)
        else:
            ad.io.write_elem(root, "/", adata)
    # Writing '/' clears and recreates the root; reopen rather than retain stale attrs.
    root = zarr.open_group(str(path), mode="a")
    assert root.metadata.zarr_format == zarr_format
    group = root[table_path] if table_path else root
    assert isinstance(group, zarr.Group)
    assert group.attrs["encoding-type"] == "anndata"
    return path, group


def scan(con, path, group, dims, columns):
    """Fetch every row, retaining duplicates; never validate only an aggregate."""
    quoted_columns = ", ".join(
        '"' + name.replace('"', '""') + '"' for name in columns
    )
    escaped_path = str(path).replace("'", "''")
    escaped_group = group.replace("'", "''")
    dims_sql = "[" + ", ".join(f"'{dim}'" for dim in dims) + "]"
    return con.execute(
        f"SELECT {quoted_columns} FROM read_zarr('{escaped_path}', "
        f"group_path := '{escaped_group}', dims := {dims_sql}) "
        "ORDER BY 1, 2"
    )


@pytest.mark.parametrize("storage", ["dense", "csr", "csc"])
def test_raw_uses_own_var_names(con, tmp_path, zarr_format, table_path, storage):
    """raw/X coordinates and raw/var annotations must not use live var names."""
    obs = pd.DataFrame(index=["cell_a", "cell_b", "cell_c"])
    raw_var = pd.DataFrame(
        {"symbol": ["RAW_A", "RAW_B", "RAW_C", "RAW_D"]},
        index=["raw_a", "raw_b", "raw_c", "raw_d"],
    )
    values = np.array(
        [[0, 2, 0, 4], [5, 0, 7, 0], [0, 10, 11, 12]], dtype=np.float32
    )
    make_matrix = {"dense": np.asarray, "csr": sp.csr_matrix, "csc": sp.csc_matrix}
    matrix = make_matrix[storage](values)
    adata = ad.AnnData(
        X=np.array([[101, 102], [103, 104], [105, 106]], dtype=np.float32),
        obs=obs.copy(),
        var=pd.DataFrame(
            {"symbol": ["LIVE_A", "LIVE_B"]}, index=["live_a", "live_b"]
        ),
    )
    adata.raw = ad.AnnData(X=matrix, obs=obs.copy(), var=raw_var)
    path, _ = write_table(tmp_path, adata, zarr_format, table_path)
    raw_group = f"{table_path}/raw" if table_path else "raw"

    raw_var_rows = scan(
        con, path, raw_group, ["raw_var"], ["raw_var", "symbol"]
    ).fetchall()
    assert raw_var_rows == [
        (name, symbol) for name, symbol in zip(raw_var.index, raw_var["symbol"])
    ]
    expected = [
        (cell, gene, float(values[i, j]))
        for i, cell in enumerate(obs.index)
        for j, gene in enumerate(raw_var.index)
        if storage == "dense" or values[i, j] != 0
    ]
    assert scan(
        con, path, raw_group, ["obs", "raw_var"], ["obs", "raw_var", "X"]
    ).fetchall() == expected
    # Both tables coexist: choosing raw must not alter the live coordinate source.
    assert scan(con, path, table_path, ["var"], ["var", "symbol"]).fetchall() == [
        ("live_a", "LIVE_A"), ("live_b", "LIVE_B")
    ]


@pytest.mark.parametrize("dtype", ["Int64", "boolean"])
@pytest.mark.parametrize("value_chunk,mask_chunk", [(2, 3), (3, 2)])
def test_nullable_values_and_mask_have_independent_chunks(
    con, tmp_path, zarr_format, table_path, dtype, value_chunk, mask_chunk
):
    """Mask lookup must use the mask's own grid, including chunk-edge NULLs."""
    values = (
        [None, 0, 12, None, -7, None, 99, None]
        if dtype == "Int64"
        else [None, False, True, None, False, None, True, None]
    )
    names = [f"cell_{i}" for i in range(len(values))]
    obs = pd.DataFrame({"nullable": pd.array(values, dtype=dtype)}, index=names)
    adata = ad.AnnData(X=np.ones((len(values), 1), dtype=np.float32), obs=obs)
    path, group = write_table(tmp_path, adata, zarr_format, table_path)
    nullable = group["obs/nullable"]
    assert isinstance(nullable, zarr.Group)
    # Keep AnnData's encoding and payload, changing only each physical chunk grid.
    for name, chunk in (("values", value_chunk), ("mask", mask_chunk)):
        array = nullable[name]
        assert isinstance(array, zarr.Array)
        payload, attrs = array[:], dict(array.attrs)
        nullable.create_array(
            name, data=payload, chunks=(chunk,), attributes=attrs, overwrite=True
        )
    assert nullable["values"].chunks == (value_chunk,)
    assert nullable["mask"].chunks == (mask_chunk,)
    # The mutated store is still a valid AnnData encoding, not a synthetic mock.
    pd.testing.assert_series_equal(
        ad.io.read_elem(group).obs["nullable"], obs["nullable"]
    )

    assert scan(
        con, path, table_path, ["obs"], ["obs", "nullable"]
    ).fetchall() == list(zip(names, values))


@pytest.mark.parametrize("category_count", [255, 256, 65535, 65536])
def test_categorical_enum_width_boundaries(
    con, tmp_path, zarr_format, table_path, category_count
):
    """ENUM vector writes must use the right width and preserve missing codes."""
    categories = [f"category_{i:05d}" for i in range(category_count)]
    codes = [
        0, category_count - 1, -1, category_count - 2, 1, -1, 0, category_count - 1
    ]
    categorical = pd.Categorical.from_codes(codes, categories=categories)
    names = [f"cell_{i}" for i in range(len(codes))]
    obs = pd.DataFrame({"label": categorical}, index=names)
    adata = ad.AnnData(X=np.ones((len(codes), 1), dtype=np.float32), obs=obs)
    path, group = write_table(tmp_path, adata, zarr_format, table_path)
    assert group["obs/label/categories"].shape == (category_count,)
    assert group["obs/label/codes"][:].tolist() == codes

    result = scan(con, path, table_path, ["obs"], ["obs", "label"])
    assert str(result.description[1][1]).startswith("ENUM("), (
        "categorical must exercise the ENUM path"
    )
    assert result.fetchall() == [
        (name, None if code == -1 else categories[code]) for name, code in zip(names, codes)
    ]
