"""Keep the generic DataTree contract intact alongside AnnData support."""

import anndata as ad
import duckdb
import numpy as np
import pytest
import xarray as xr
import zarr


@pytest.mark.parametrize("zarr_format", [2, 3], ids=["zarr-v2", "zarr-v3"])
def test_same_shaped_groups_remain_separate(tmp_path, extension_path, zarr_format):
    # Identical variable names, dimensions and shapes must not merge across
    # groups. The nested group inherits its coordinate through an ancestor.
    signals = {
        "/": [1.0, 2.0, 3.0],
        "/left": [4.0, 5.0, 6.0],
        "/left/deep": [7.0, 8.0, 9.0],
        "/right": [10.0, 11.0, 12.0],
    }
    datasets = {
        group: xr.Dataset({"signal": ("sample", values)})
        for group, values in signals.items()
    }
    datasets["/"] = datasets["/"].assign_coords(sample=[10, 20, 30])
    store = tmp_path / "tree.zarr"
    xr.DataTree.from_dict(datasets).to_zarr(
        store, zarr_format=zarr_format, consolidated=True,
    )

    with duckdb.connect(config={"allow_unsigned_extensions": True}) as con:
        con.load_extension(str(extension_path))
        path = str(store)
        root_rows = [(10, 1.0), (20, 2.0), (30, 3.0)]
        assert con.execute(
            "SELECT * FROM read_zarr(?) ORDER BY sample", [path],
        ).fetchall() == root_rows
        assert con.execute(
            f"SELECT * FROM '{path}' ORDER BY sample",
        ).fetchall() == root_rows

        for group, values in signals.items():
            expected = list(zip([10, 20, 30], values))
            assert con.execute(
                "SELECT sample, signal FROM read_zarr(?, group_path := ?, "
                "table_name := 'sample') ORDER BY sample", [path, group],
            ).fetchall() == expected
            assert con.execute(
                'SELECT sample, signal FROM read_zarr(?, "group" := ?, '
                '"table" := \'SAMPLE\') ORDER BY sample', [path, group],
            ).fetchall() == expected

        catalog = con.execute(
            "SELECT group_path, schema_name, table_name, dims, shape, "
            "data_vars, coord_vars FROM read_zarr_tables(?) ORDER BY group_path",
            [path],
        ).fetchall()
        assert catalog == [
            (group, "main" if group == "/" else group[1:], "sample",
             ["sample"], [3], ["signal" if group == "/" else f"{group[1:]}/signal"],
             ["sample"])
            for group in signals
        ]
        assert con.execute(
            "SELECT * FROM read_zarr_groups(?)", [path],
        ).fetchall() == con.execute(
            "SELECT * FROM read_zarr_tables(?)", [path],
        ).fetchall()
        assert con.execute(
            "SELECT name, dims, array_path_dims FROM read_zarr_metadata(?, "
            "group_path := 'left/deep')", [path],
        ).fetchall() == [("left/deep/signal", ["sample"], ["sample"])]

        # Execute the generated SQL, not a hand-written copy of the recipe.
        statements = con.execute(
            "SELECT sql FROM read_zarr_views(?, 'tree')", [path],
        ).fetchall()
        for (statement,) in statements:
            con.execute(statement)
        for group, values in signals.items():
            schema = "main" if group == "/" else group[1:]
            assert con.execute(
                f'SELECT sample, signal FROM tree."{schema}".sample ORDER BY sample',
            ).fetchall() == list(zip([10, 20, 30], values))


@pytest.mark.parametrize("existing", ["memory", "file", "default", "case-folded"])
def test_mount_rejects_existing_database_alias(tmp_path, extension_path, existing):
    """A generated mount must not replace views in an unrelated database."""
    store = tmp_path / "mount.zarr"
    xr.Dataset({"signal": ("sample", [1, 2])}).to_zarr(
        store, zarr_format=3, consolidated=False,
    )
    with duckdb.connect(config={"allow_unsigned_extensions": True}) as con:
        con.load_extension(str(extension_path))
        if existing == "default":
            row = con.execute("SELECT current_database()").fetchone()
            assert row is not None
            database = row[0]
        else:
            database = "mount"
            source = str(tmp_path / "existing.duckdb") if existing == "file" else ":memory:"
            source = source.replace("'", "''")
            con.execute(f"ATTACH '{source}' AS mount")
        requested = database.upper() if existing == "case-folded" else database
        con.execute(f'CREATE VIEW "{database}".main.sample AS SELECT 999 AS sentinel')
        statements = con.execute(
            "SELECT sql FROM read_zarr_views(?, ?)", [str(store), requested],
        ).fetchall()
        with pytest.raises(duckdb.Error, match="already exists"):
            for (statement,) in statements:
                con.execute(statement)
        assert con.execute(
            f'SELECT sentinel FROM "{database}".main.sample',
        ).fetchall() == [(999,)]


@pytest.mark.parametrize("zarr_format", [2, 3], ids=["zarr-v2", "zarr-v3"])
@pytest.mark.parametrize("group", ["", "measurements"], ids=["root", "sibling"])
@pytest.mark.parametrize("axis", ["obs_i", "obs_j", "var_i", "var_j"])
def test_anndata_aliases_do_not_replace_plain_coordinates(
    tmp_path, extension_path, zarr_format, group, axis,
):
    """An unrelated AnnData object must not change existing plain-Zarr tables."""
    store = tmp_path / "mixed.zarr"
    base_axis = axis.split("_")[0]
    xr.Dataset(
        {"signal": (axis, [1, 2])},
        coords={base_axis: [10, 20], axis: [100, 200]},
    ).to_zarr(store, group=group, zarr_format=zarr_format, consolidated=False)

    with duckdb.connect(config={"allow_unsigned_extensions": True}) as con:
        con.load_extension(str(extension_path))
        query = (
            f'SELECT "{axis}", signal FROM read_zarr(?, '
            f'group_path := ?, table_name := ?) ORDER BY "{axis}"'
        )
        expected = [(100, 1), (200, 2)]
        assert con.execute(query, [str(store), group, axis]).fetchall() == expected

        adata = ad.AnnData(np.eye(2))
        adata.obs_names = ["cell_a", "cell_b"]
        adata.var_names = ["gene_a", "gene_b"]
        adata.obsp["neighbors"] = np.eye(2)
        adata.varp["similarity"] = np.eye(2)
        root = zarr.open_group(str(store), mode="a")
        with ad.settings.override(allow_write_nullable_strings=True):
            ad.io.write_elem(root.require_group("tables"), "cells", adata)

        assert con.execute(query, [str(store), group, axis]).fetchall() == expected
        prefix = f"{group}/" if group else ""
        assert con.execute(
            "SELECT coord_vars FROM read_zarr_tables(?, group_path := ?) "
            "WHERE table_name = ?", [str(store), group, axis],
        ).fetchone() == ([f"{prefix}{axis}"],)
        assert con.execute(
            "SELECT name, role FROM read_zarr_metadata(?, group_path := ?) "
            "WHERE name = ?", [str(store), group, f"{prefix}{axis}"],
        ).fetchone() == (f"{prefix}{axis}", "coord")
        assert con.execute(
            'SELECT * FROM read_zarr(?, array_path := ?) '
            f'ORDER BY "{axis}"', [str(store), f"{prefix}signal"],
        ).fetchall() == expected

        # The AnnData object's own pairwise axes must still use its index names.
        for mapping, dims, names in [
            ("obsp", ["obs_i", "obs_j"], ["cell_a", "cell_b"]),
            ("varp", ["var_i", "var_j"], ["gene_a", "gene_b"]),
        ]:
            assert con.execute(
                f'SELECT "{dims[0]}", "{dims[1]}" FROM read_zarr(?, '
                'group_path := ?, table_name := ?) ORDER BY 1, 2',
                [str(store), f"tables/cells/{mapping}", "_".join(dims)],
            ).fetchall() == [(a, b) for a in names for b in names]
