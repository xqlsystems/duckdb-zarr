# /// script
# dependencies = ["xarray", "zarr", "numpy"]
# ///
"""Write a small consolidated Zarr v2 sample store into public/data/.

Source: the air_temperature fixture (`make generate_fixtures` in the repo root).
Remote reads need consolidated metadata, because HTTP stores cannot be listed.
"""
import sys
from pathlib import Path

import xarray as xr

here = Path(__file__).resolve().parents[1]
src = here.parents[1] / "test/fixtures/xarray_tutorial/air_temperature.zarr"
dest = here / "public/data/air.zarr"
if not src.exists():
    sys.exit(f"{src} not found; run `make generate_fixtures` in the repo root first")

ds = xr.open_zarr(src, consolidated=False).isel(time=slice(0, 40)).load()
enc = {v: {"compressors": [{"id": "gzip", "level": 1}]} for v in ds.variables}
dest.parent.mkdir(parents=True, exist_ok=True)
ds.to_zarr(dest, mode="w", zarr_format=2, consolidated=True, encoding=enc)
print(f"wrote {dest}: {dict(ds.sizes)}")
