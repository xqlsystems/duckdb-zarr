#!/usr/bin/env python3
# /// script
# requires-python = ">=3.11"
# dependencies = [
#     "anndata[dask]==0.13.3.post0",
#     "zarr==3.3",
#     "awkward==2.13.0",
#     "duckdb==1.5.5",
#     "scipy",
# ]
# ///
"""Check every table read_zarr gives for an AnnData store against anndata.read_zarr.

For each store, this compares the obs and var data frames, X, every layer, and
every obsm/varm/obsp/varp entry with the extension's tables (design decision 9).
A sparse matrix alone in its table must give exactly its stored entries (plus
zeros at positions another matrix in the table stores); beside a dense array it
must give every cell.

Usage:
    make debug generate_fixtures
    uv run scripts/check_anndata_tables.py build/debug/zarr.duckdb_extension [store ...]

The stores default to the three AnnData fixtures.
"""
import sys

import anndata as ad
import duckdb
import numpy as np
import pandas as pd
import scipy.sparse as sp

con = duckdb.connect(config={"allow_unsigned_extensions": True})
con.load_extension(sys.argv[1])
failures = 0


def check(label, ok, detail=""):
    global failures
    if not ok:
        failures += 1
        print(f"FAIL {label} {detail}")
    else:
        print(f"ok   {label}")


def table(store, group, dims):
    dims_sql = "[" + ",".join(f"'{d}'" for d in dims) + "]"
    return con.sql(
        f"SELECT * FROM read_zarr('{store}', group_path := '{group}', dims := {dims_sql})"
    ).df()


def same_series(got, want):
    got = pd.Series(got).reset_index(drop=True)
    want = pd.Series(want).reset_index(drop=True)
    gna, wna = got.isna().to_numpy(), pd.isna(want).to_numpy()
    if not (gna == wna).all():
        return False, f"null mask differs: got {gna.nonzero()} want {wna.nonzero()}"
    g, w = got[~gna].to_numpy(), want[~wna].to_numpy()
    if g.dtype.kind in "fc" or w.dtype.kind in "fc":
        return bool(np.allclose(g.astype(float), w.astype(float))), ""
    return bool((g.astype(str) == w.astype(str)).all()), f"{g[:5]} vs {w[:5]}"


def check_frame(store, group, axis, df):
    # The index is the coordinate: the `obs`/`var` column holds the names.
    got = table(store, group, [axis]).set_index(axis)
    got = got.loc[list(df.index.astype(str))].rename_axis(axis).reset_index()
    check(f"{store}:{group} rows", len(got) == len(df), f"{len(got)} vs {len(df)}")
    check(f"{store}:{group} index", same_series(got[axis], df.index)[0])
    for col in df.columns:
        if col not in got:
            check(f"{store}:{group}/{col} present", False)
            continue
        ok, detail = same_series(got[col], df[col])
        check(f"{store}:{group}/{col}", ok, detail)


def check_matrix(store, group, name, dims, m, stored_only, row_names, col_names):
    got = table(store, group, dims)
    # Dimension columns hold names when the axis has an index; map them back
    # to positions to compare with the matrix.
    for dim, names in ((dims[0], row_names), (dims[1], col_names)):
        if names is not None and not pd.api.types.is_integer_dtype(got[dim]):
            pos = {str(n): i for i, n in enumerate(names)}
            got[dim] = got[dim].map(pos)
    if stored_only:
        coo = sp.coo_matrix(m)
        want = {(int(i), int(j)): float(v) for i, j, v in zip(coo.row, coo.col, coo.data)}
        have = {(int(r[dims[0]]), int(r[dims[1]])): float(r[name]) for _, r in got.iterrows()}
        # A union table also has rows other matrices store, where this one is 0.
        extra = {k: v for k, v in have.items() if k not in want}
        ok = all(have.get(k) == v for k, v in want.items()) and all(v == 0 for v in extra.values())
        check(f"{store}:{group}/{name} entries", ok)
    else:
        dense = m.toarray() if sp.issparse(m) else np.asarray(m)
        check(f"{store}:{group}/{name} rows", len(got) == dense.size, f"{len(got)} vs {dense.size}")
        arr = np.zeros(dense.shape)
        for _, r in got.iterrows():
            arr[int(r[dims[0]]), int(r[dims[1]])] = r[name]
        check(f"{store}:{group}/{name} values", np.allclose(arr, dense))


STORES = sys.argv[2:] or [
    "test/fixtures/anndata/pbmc_like.zarr",
    "test/fixtures/anndata/adata_v3.zarr",
    "test/fixtures/anndata/adata_v2.zarr",
]

for store in STORES:
    a = ad.read_zarr(store)
    obs_names, var_names = list(a.obs_names), list(a.var_names)
    check_frame(store, "", "obs", a.obs)
    check_frame(store, "", "var", a.var)
    if a.X is not None:
        check_matrix(store, "", "X", ["obs", "var"], a.X, sp.issparse(a.X), obs_names, var_names)
    layers = {k: v for k, v in a.layers.items() if k is not None}  # anndata 0.13: layers[None] is X
    all_sparse = all(sp.issparse(v) for v in layers.values())
    for k, v in layers.items():
        check_matrix(store, "layers", k, ["obs", "var"], v, all_sparse, obs_names, var_names)
    for axis, names, mapping, pair in (
        ("obs", obs_names, a.obsm, a.obsp),
        ("var", var_names, a.varm, a.varp),
    ):
        for k, v in mapping.items():
            if isinstance(v, pd.DataFrame):
                check_frame(store, f"{axis}m/{k}", axis, v)
            elif sp.issparse(v) or isinstance(v, np.ndarray):
                check_matrix(store, f"{axis}m", k, [axis, f"{k}_component"], v,
                             sp.issparse(v), names, None)
        pair_sparse = all(sp.issparse(v) for v in pair.values())
        for k, v in pair.items():
            check_matrix(store, f"{axis}p", k, [f"{axis}_i", f"{axis}_j"], v, pair_sparse,
                         names, names)

print("FAILURES:", failures)
sys.exit(1 if failures else 0)
