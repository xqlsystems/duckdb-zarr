"""Read-time axis names for explicitly selected arrays without dimension metadata."""

from functools import partial
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from threading import Thread

import duckdb
import numpy as np
import pytest
import zarr


@pytest.fixture(params=[2, 3], ids=["zarr-v2", "zarr-v3"])
def unnamed_store(tmp_path, request):
    """Write a real, small matrix without dimension names in either Zarr format."""
    store = tmp_path / "unnamed.zarr"
    root = zarr.open_group(str(store), mode="w", zarr_format=request.param)
    values = np.arange(1, 13, dtype="float32").reshape(3, 4)
    root.create_array("plain", data=values, chunks=(2, 3))
    root.require_group("nested").create_array("plain", data=values, chunks=(2, 3))
    return store, root, values


@pytest.fixture
def connection(extension_path):
    """Load the actual compiled extension, not a substitute reader."""
    with duckdb.connect(config={"allow_unsigned_extensions": True}) as con:
        con.load_extension(str(extension_path))
        yield con


@pytest.fixture(params=["local", "http"])
def store_path(unnamed_store, request):
    """Also exercise direct remote reads without consolidated metadata."""
    store, _, _ = unnamed_store
    if request.param == "local":
        yield str(store)
        return
    handler = partial(SimpleHTTPRequestHandler, directory=str(store.parent))
    with ThreadingHTTPServer(("127.0.0.1", 0), handler) as server:
        thread = Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            yield f"http://127.0.0.1:{server.server_port}/{store.name}"
        finally:
            server.shutdown()
            thread.join(timeout=5)
            assert not thread.is_alive()


@pytest.mark.parametrize("array_path", ["plain", "nested/plain"])
def test_dims_label_positions_without_changing_values(connection, unnamed_store, store_path, array_path):
    """Explicit names change only axis labels, including across ragged chunks."""
    store, _, values = unnamed_store
    before = {str(p): p.read_bytes() for p in store.rglob("*") if p.is_file()}
    cursor = connection.execute(
        "SELECT * FROM read_zarr(?, array_path := ?, dims := ['cells', 'genes']) ORDER BY 1, 2",
        [store_path, array_path],
    )
    assert [column[0] for column in cursor.description] == [
        "cells", "genes", "plain" if array_path == "plain" else "value",
    ]
    assert cursor.fetchall() == [
        (i, j, float(values[i, j])) for i in range(3) for j in range(4)
    ]
    assert connection.execute(
        "SELECT genes FROM read_zarr(?, array_path := ?, dims := ['cells', 'genes']) "
        "WHERE cells = 2 ORDER BY genes", [store_path, array_path],
    ).fetchall() == [(0,), (1,), (2,), (3,)]
    assert before == {str(p): p.read_bytes() for p in store.rglob("*") if p.is_file()}


def test_defaults_and_group_reads_are_unchanged(connection, unnamed_store):
    """Keep default labels, catalog metadata, and group selection unchanged."""
    store, _, _ = unnamed_store
    cursor = connection.execute("SELECT * FROM read_zarr(?, array_path := 'plain')", [str(store)])
    assert [column[0] for column in cursor.description] == ["dim_0", "dim_1", "plain"]
    assert len(cursor.fetchall()) == 12
    with pytest.raises(duckdb.Error, match="declare no dimension names"):
        connection.execute("SELECT * FROM read_zarr(?, dims := ['cells', 'genes'])", [str(store)])
    assert connection.execute(
        "SELECT dims, array_path_dims FROM read_zarr_metadata(?) WHERE name = 'plain'", [str(store)],
    ).fetchone() == ([], ["dim_0", "dim_1"])


@pytest.mark.parametrize("names", [[], ["cells"], ["cells", "genes", "extra"]])
def test_name_count_must_match_rank(connection, unnamed_store, names):
    """Reject a supplied name list that does not match the array rank."""
    store, _, _ = unnamed_store
    with pytest.raises(duckdb.Error, match="2 dimensions"):
        connection.execute(
            "SELECT * FROM read_zarr(?, array_path := 'plain', dims := ?)", [str(store), names],
        )


@pytest.mark.parametrize("names", [
    ["cells", "CELLS"], ["", "genes"], ["  ", "genes"], [None, "genes"],
    pytest.param(
        ["bad\x00name", "genes"],
        marks=pytest.mark.xfail(
            strict=True,
            reason="DuckDB's existing duckdb_get_varchar conversion truncates embedded NULs; core validation is tested separately",
        ),
    ),
])
def test_dimension_names_must_be_valid_and_distinct(connection, unnamed_store, names):
    """Reject invalid or case-insensitively repeated axis names."""
    store, _, _ = unnamed_store
    with pytest.raises(duckdb.Error, match="dimension names"):
        connection.execute(
            "SELECT * FROM read_zarr(?, array_path := 'plain', dims := ?)", [str(store), names],
        )


@pytest.mark.parametrize("names", [["row", "column"], ["dim_0", "dim_1"], ["row", None]])
def test_declared_names_cannot_be_overridden(connection, unnamed_store, names):
    """Preserve declared axes, including explicit dim_N and partial names."""
    store, root, values = unnamed_store
    if root.metadata.zarr_format == 2:
        root["plain"].attrs["_ARRAY_DIMENSIONS"] = names
    else:
        del root["plain"]
        root.create_array("plain", data=values, dimension_names=names)
    with pytest.raises(duckdb.Error, match="has dimensions"):
        connection.execute(
            "SELECT * FROM read_zarr(?, array_path := 'plain', dims := ['cells', 'genes'])", [str(store)],
        )
    expected = [name if name is not None else f"dim_{i}" for i, name in enumerate(names)]
    cursor = connection.execute(
        "SELECT * FROM read_zarr(?, array_path := 'plain', dims := ?)", [str(store), expected],
    )
    assert [column[0] for column in cursor.description] == expected + ["plain"]
    assert len(cursor.fetchall()) == 12


def test_supplied_names_do_not_bind_sibling_coordinates(connection, unnamed_store):
    """Keep integer positions even when a matching sibling array exists."""
    store, root, _ = unnamed_store
    root.create_array("cells", data=np.array([100, 200, 300]))
    assert connection.execute(
        "SELECT DISTINCT cells FROM read_zarr(?, array_path := 'plain', "
        "dims := ['cells', 'genes']) ORDER BY cells", [str(store)],
    ).fetchall() == [(0,), (1,), (2,)]


def test_ome_axis_names_cannot_be_overridden(connection, unnamed_store):
    """Protect axis names supplied by OME multiscales metadata."""
    store, root, _ = unnamed_store
    root.attrs["multiscales"] = [{
        "axes": [{"name": "y"}, {"name": "x"}], "datasets": [{"path": "plain"}],
    }]
    with pytest.raises(duckdb.Error, match="has dimensions"):
        connection.execute(
            "SELECT * FROM read_zarr(?, array_path := 'plain', dims := ['cells', 'genes'])", [str(store)],
        )


def test_anndata_axis_names_cannot_be_overridden(connection, unnamed_store):
    """Protect the observation and variable axes recognized from AnnData."""
    store, root, values = unnamed_store
    root.attrs.update({"encoding-type": "anndata", "encoding-version": "0.1.0"})
    root.create_array("X", data=values)
    with pytest.raises(duckdb.Error, match="has dimensions"):
        connection.execute(
            "SELECT * FROM read_zarr(?, array_path := 'X', dims := ['cells', 'genes'])", [str(store)],
        )


def test_scalar_accepts_an_empty_dimension_list(connection, unnamed_store):
    """Read a rank-zero array with its matching empty axis-name list."""
    store, root, _ = unnamed_store
    root.create_array("scalar", data=np.array(42, dtype="int32"))
    assert connection.execute(
        "SELECT scalar FROM read_zarr(?, array_path := 'scalar', dims := [])", [str(store)],
    ).fetchall() == [(42,)]


def test_unusual_dimension_names_and_array_alias(connection, unnamed_store):
    """Preserve spaces and quotes in names when using the array alias."""
    store, _, _ = unnamed_store
    cursor = connection.execute(
        'SELECT * FROM read_zarr(?, "array" := \'plain\', dims := ?) ORDER BY 1, 2',
        [str(store), ["cell ids", 'gene "names"']],
    )
    assert [column[0] for column in cursor.description] == ["cell ids", 'gene "names"', "plain"]
    assert len(cursor.fetchall()) == 12


@pytest.mark.parametrize("array_path,names,value_name", [
    ("plain", ["plain", "genes"], "plain_1"),
    ("plain", ["PLAIN", "genes"], "plain_1"),
    ("plain", ["plain", "PLAIN_1"], "plain_2"),
    ("nested/plain", ["value", "genes"], "value_1"),
    ("nested/plain", ["value", "VALUE_1"], "value_2"),
    ("0", ["value", "genes"], "value_1"),
    ("0", ["value", "VALUE_1"], "value_2"),
    ("nested/plain", ["nested/plain", "genes"], "value"),
])
def test_value_column_is_distinct_from_a_supplied_axis(
    connection, unnamed_store, store_path, array_path, names, value_name,
):
    """Deduplicate final aliases without hiding values or changing axis labels."""
    _, root, values = unnamed_store
    if array_path == "0":
        root.create_array("0", data=values, chunks=(2, 3))
    cursor = connection.execute(
        "SELECT * FROM read_zarr(?, array_path := ?, dims := ?) ORDER BY 1, 2",
        [store_path, array_path, names],
    )
    assert [column[0] for column in cursor.description] == names + [value_name]
    assert cursor.fetchall() == [
        (i, j, float(values[i, j])) for i in range(3) for j in range(4)
    ]
    # Only the fixed test-case identifier is interpolated, never input or data.
    assert connection.execute(
        f'SELECT "{value_name}" FROM read_zarr(?, array_path := ?, dims := ?) ORDER BY 1',
        [store_path, array_path, names],
    ).fetchall() == [(float(value),) for value in values.flat]


def test_group_value_alias_is_distinct_from_a_dimension(connection, unnamed_store):
    """Apply the same final-alias collision rule to ordinary group reads."""
    store, root, values = unnamed_store
    names = ["value", "genes"]
    if root.metadata.zarr_format == 2:
        array = root.create_array("0", data=values)
        array.attrs["_ARRAY_DIMENSIONS"] = names
    else:
        root.create_array("0", data=values, dimension_names=names)
    cursor = connection.execute(
        "SELECT value, genes, value_1 FROM read_zarr(?, dims := ?) ORDER BY value, genes",
        [str(store), names],
    )
    assert [column[0] for column in cursor.description] == names + ["value_1"]
    assert cursor.fetchall() == [
        (i, j, float(values[i, j])) for i in range(3) for j in range(4)
    ]
