# Filter pushdown for read_zarr: learnings (issue #59)

Handoff notes for a future agent. Not committed. Written 2026-10-11.

## Goal

`WHERE` on time, lat or lon should skip chunks. Today `read_zarr` scans every chunk, so a one-day slice of ARCO ERA5 (69,033 chunks) is unusable, especially in duckdb-wasm. The user wants real pushdown. Any explicit-parameter workaround is a stopgap only.

## Hard facts

- **The DuckDB 1.5.x C extension API has no filter pushdown hook.** Projection pushdown works. DuckDB re-applies filters after the scan, so results are correct but nothing is skipped. See `docs/design.md` item (e).
- **No Rust or C-API extension does planner-level pushdown today.**
  - `alitrack/duckdb_orc` documents pushdown as "Blocked".
  - duckdb-rs #401 ("VTab: filter_pushdown support") is open and waiting on C-API V2.
  - Older C API attempts, duckdb/duckdb #14591 and #19093, were abandoned.
  - C++ extensions (Parquet, Postgres scanner, Query Farm Airport) use `filter_pushdown` / `pushdown_complex_filter`. A Rust C-API extension can't reach those. A C++ shim would be the same route.
  - xarray-sql gets pushdown from core `arrow_scan` over a pyarrow dataset. That is not an extension-API solution.
  - Unverified: `ST_Read` in the spatial extension may use `spatial_filter_box` parameters as a precedent for explicit-parameter filtering. I couldn't confirm it, and I couldn't find how Vortex does pushdown.
- **DuckDB 2.0** (expected fall 2026; alpha `v2.0.0-alpha43678`) adds async I/O only for built-in readers (Parquet, CSV). Nothing for extensions or wasm. It does not fix browser concurrency.
- **C-API-V2** (duckdb/duckdb #25340, merged 2026-09-04) adds expression claim-based filter pushdown.
  - A function declares support and receives candidate filters.
  - It calls `accept_pushdown(i)` for the ones it handles.
  - DuckDB stops re-applying accepted filters. Unaccepted ones stay enforced.
- **`duckdb-neo` crate** (duckdb-rs main, PR #855, merged 2026-09-14) wraps this.
  - Types: `TableFilterPushdownCallbacks`, `with_filter_pushdown()`, `PushdownData` (`filter_count`, `filter(i)`, `get_column_index`, `accept_pushdown`) and `Expression` (`expression_type`, `children`, `get_constant_value`, `reference_index`).
  - It is pinned to the alpha and not on crates.io. Its extension docs say "TODO".

## Spike result (v2 pushdown works, but isn't shippable)

A throwaway extension `spike_scan(n)` against the alpha loaded and received filters.

- **Entrypoint:** `<name>_init_c_api_v2(struct duckdb_v2_extension_input*)`.
  - The loader requires `get_api(extension, "v2.0.0")` to be called first. Otherwise: "did not initialize the C API struct".
  - The footer ABI type is `C_STRUCT_UNSTABLE`, and the version string is the alpha label.
- **Exec order:** call `set_size` before writing vector elements. Otherwise: "Vector index 0 is out of bounds for length 0".
- **Gotcha, duckdb/duckdb #25391: `BETWEEN` loses inclusivity.**
  - Inclusive and exclusive ranges are pushed identically. Any two-sided range arrives as one `CompareBetween` with 3 children and no inclusivity flag.
  - Claiming it gave 11 rows where 9 or 10 were correct.
  - **Rule:** prune chunks from `CompareBetween`, but never claim it. Claim only single comparisons (`=`, `<`, `>`, `<=`, `>=`).
- **Blockers to shipping on v2:**
  1. `libduckdb-sys` v2 bindings are plain extern functions. There is no loadable function-table binding, whereas v1 has `bindgen_bundled_version_loadable.rs`. This is needed for a `.duckdb_extension`.
  2. `duckdb-neo` is unpublished.
  3. The extension must be ported to the v2 entrypoint and ABI.
  4. duckdb-wasm needs a 2.0 build.
  5. #25391 still applies.
- Spike code lived in the session scratchpad (`spike/`, `neo/`, `duckdb-rs/`) and is not in the repo. Memory note: `duckdb2-filter-pushdown-spike.md`.

## Design that works under any entry point

Keep this core independent of whether bounds come from a parameter or from pushdown.

1. Resolve each bound against the cached 1-D coordinate array into a per-index keep mask (`index_mask`).
2. Select chunks per dimension: a chunk is read iff some kept index lies in its span (`chunks_with_kept`).
3. Build work units as the cartesian product of the selected chunks per dimension, in row-major order (`build_work_units_in`).
4. Filter emitted rows by the masks in every bounded dimension.

Notes:

- Going through the real coordinate values handles descending (ERA5 lat 90→-90), non-uniform and non-monotonic axes with no special cases.
- Compare as f64. TIMESTAMPs are microseconds since the epoch and exact within ±2^53 µs.
- Fill sentinels, NaN and UInt64 overflow in time give `None` (never kept).
- Bounds are inclusive.

## Existing work

- **PR #49** (closed, remote branch deleted, 5 commits behind `main`, no review comments). Local ref: `git fetch origin refs/pull/49/head:pr49`, commit 2cac034.
  - API: `ranges=['dim:lo:hi']`, raw coordinate values, inclusive.
  - Functions: `build_work_units_pruned`, `build_dim_keep_masks`, `kept_rows_in_chunk`, with `design_*` unit tests in `meta.rs`.
  - It measured 3 of 69,033 chunks touched on ARCO ERA5.
  - Problems: `:` splitting can't take timestamps (`'2020-07-01 12:00:00'`). #43 (cftime, TIMESTAMP time columns) had not landed then, but it has now.
  - Its body lists routes to real pushdown: `ranges=`, a C++ shim, reviving the C API upstream, table-macro sugar.
- **Uncommitted `filter-pushdown` branch (my `bounds=` stopgap).** It is the same idea as #49 with typed parsing.
  - API: `bounds=[['time','2020-07-01','2020-07-02'], ['lat','40','50']]`, a `LIST(LIST(VARCHAR))`. Endpoints must be strings because DuckDB can't type mixed lists (`['lat', -30, 30]` fails: "Could not convert string 'lat' to INT32").
  - Files: `src/zarr_reader/bounds.rs` (new, 27 `cargo test --lib` tests passed), `scan.rs` (`element_key`), `types.rs` (`ColumnValues::key_at`), `meta.rs` (`build_work_units_in`), `read_zarr.rs` (the param, `finish_bind`, `fill_chunk_slice` returning `(consumed, written)`), `scripts/generate_fixtures.py` (`bounds_grid`, `bounds_grid_poisoned`), `test/sql/read_zarr_bounds.test`.
  - **The full SQL suite was never confirmed green.** The run was interrupted; individual failures had been fixed one at a time. Run `make test_debug` before trusting it.
  - `demos/` is untracked leftover and must not be committed.

## Testing lessons

- `COUNT(*)` alone doesn't decode the data variable, so it can't prove chunks were skipped. Project a data variable, e.g. `COUNT(value)`.
- **Poisoned-chunk fixture:** a copy of the grid with one chunk overwritten by garbage bytes. A query that must prune it succeeds only if the chunk is never read. This proves "not read", not just "filtered after reading".
- Test seams explicitly: chunk boundaries, ragged tails, descending lat, empty and inverted ranges, one-sided bounds, and `decode_times=false`.

## Environment (NixOS)

- Build: `nix shell nixpkgs#cargo nixpkgs#rustc nixpkgs#gcc nixpkgs#clippy nixpkgs#rustfmt nixpkgs#gnumake -c ...`
- Python or numpy tests: `export LD_LIBRARY_PATH=$(nix build --no-link --print-out-paths nixpkgs#stdenv.cc.cc.lib)/lib`
- Single sqllogictest: `./configure/venv/bin/python3 -m duckdb_sqllogictest --test-dir test/sql --file-path test/sql/<f>.test --external-extension build/debug/zarr.duckdb_extension`

## Proposed plan (not yet agreed)

1. **Now:** revive #49 as a new PR from commit 2cac034, preserving authorship. Rebase onto `main`. Fold in typed time bounds, the poisoned-chunk fixture and the `COUNT(value)` lesson. Keep one spelling, not both. Open choice: `ranges=[[dim,lo,hi],...]` (my version) vs a flat `'dim|lo|hi'` list.
2. **Migration plan** in `docs/design.md`:
   - Keep the pruning core entry-point-agnostic.
   - On v2, add a `pushdown_filter` that converts single comparisons to bounds and claims them. Use `CompareBetween` for pruning only.
   - Keep the parameter working for one release as a compatible alias, then deprecate.
   - Gates: loadable v2 Rust bindings, a published `duckdb-neo`, the extension port, a duckdb-wasm 2.0 build.
3. **Optional upstream contribution:** loadable function-table bindings for v2 in duckdb-rs.

## Direction under consideration: VGI (Query Farm)

User wants to try [VGI](https://query.farm/vgi/architecture/). Notes from one page fetch, so details are unverified:

- A single generic DuckDB extension bridges to out-of-process workers over Apache Arrow IPC. Transports: pipes or Unix sockets, shared memory, HTTP.
- Workers mirror DuckDB's extension semantics (bind/process/combine) and expose catalogs, schemas, tables and views.
- The page says DuckDB pushes projection, filter, `ORDER BY + LIMIT` and join-key filters to the worker. This would give real filter pushdown with no DuckDB 2.0, no C++ shim and no `duckdb-neo`. It is the main reason to try it.
- SDKs exist for Python, TypeScript, Go, Rust, Java and C#.
- The page says nothing about wasm or in-browser use. HTTP workers could serve a browser client, but that is untested. It also does not help the single-threaded blocking-XHR problem unless the worker is remote.
- Open questions for the spike:
  - What shape do pushed filters take (expression tree vs simple comparisons)?
  - Do `BETWEEN` and inclusivity survive (see #25391)?
  - Can a Rust worker reuse the pruning core above and the existing zarr reader?
  - What is the cost of the Arrow IPC hop for large scans?
  - Does the VGI extension load in duckdb-wasm?

## Deprioritised

- The `demos/zarr-map` demo (PR #58). The user has a working xarray-sql wasm demo.
- Browser concurrency. Single-threaded duckdb-wasm does blocking XHR, so reads are serialized. Haybarn and `AsyncDuckDB` don't fix this; JSPI would. A prefetch-cache idea was proposed but not started.
- Updating the README for the ARCO default.
- A Safari check.
