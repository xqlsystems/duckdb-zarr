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
"""Generate realistic AnnData Zarr fixtures (v2 and v3) for issue 
https://github.com/xqlsystems/duckdb-zarr/issues/40.

Unlike hand-built xarray fixtures, these are created directly via
`anndata.tests.helpers.gen_adata`—matching AnnData's native `write_zarr()`
output and CI test archives (written to directories rather than zip files).

Note on current status:
- Lacks xarray dimension metadata (`dimension_names` / `_ARRAY_DIMENSIONS`),
  so direct `read_zarr` bindings currently fail.
- Contains sparse/dataframe encodings in `X`/`obsm`/`varm`/`obsp`/`varp`.
- `read_zarr_metadata` works on `obs`/`var` (see test/sql/anndata.test).

These fixtures serve as real target data for developing full AnnData support.

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
