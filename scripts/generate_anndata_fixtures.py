#!/usr/bin/env python3
# /// script
# requires-python = ">=3.11"
# dependencies = [
#     "anndata[dask]==0.13.3.post0",
#     "zarr==3.3",
#     "pytest==9.1.1",
#     "awkward==2.13.0",
# ]
# ///
"""Generate AnnData Zarr stores (Zarr v3 and v2) from anndata's own test helper.

`anndata.tests.helpers.gen_adata` builds an AnnData object with every encoding
anndata supports: data frames, categoricals, CSR and CSC matrices, awkward
arrays, and nested `uns` entries. These stores are for exploring real anndata
output. No SQL test reads them yet; test/sql/anndata.test uses the smaller,
deterministic pbmc_like.zarr from scripts/generate_fixtures.py.

The script pins its own dependencies (PEP 723 metadata above) so that anndata,
dask and awkward do not constrain the versions in the main fixture environment.
See https://github.com/xqlsystems/duckdb-zarr/issues/40.

Usage:
    uv run scripts/generate_anndata_fixtures.py

Output:
    test/fixtures/anndata/adata_v3.zarr
    test/fixtures/anndata/adata_v2.zarr
"""
import pathlib
import shutil

import anndata as ad
import numpy as np
from anndata.tests.helpers import gen_adata

ROOT = pathlib.Path(__file__).parent.parent
FIXTURES = ROOT / "test" / "fixtures" / "anndata"


def _fixture_exists(dest: pathlib.Path) -> bool:
    return (dest / "zarr.json").exists() or (dest / ".zgroup").exists()


def write(name: str, zarr_write_format: int) -> None:
    dest = FIXTURES / f"{name}.zarr"
    if _fixture_exists(dest):
        print(f"{name}... (cached)")
        return
    if dest.exists():
        shutil.rmtree(dest)

    ad.settings.zarr_write_format = zarr_write_format
    ad.settings.allow_write_nullable_strings = False
    adata = gen_adata((10, 20), random_state=np.random.default_rng(0))
    adata.write_zarr(dest)
    print(f"{name}... wrote {dest}")


def main() -> None:
    FIXTURES.mkdir(parents=True, exist_ok=True)
    write("adata_v3", zarr_write_format=3)
    write("adata_v2", zarr_write_format=2)
    print("Anndata fixtures written.")


if __name__ == "__main__":
    main()
