# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/).

## [Unreleased]

### Changed
- **BREAKING — CF time coordinates now come back as `TIMESTAMP`, not as the raw numeric offset they are physically stored as.** A coordinate carrying `units = "<step> since <reference>"` is decoded at scan time, so `WHERE time >= TIMESTAMP '2024-01-01'`, `date_trunc`, and the rest of DuckDB's date machinery work on Zarr time axes directly — previously ARCO-ERA5's `time` came back as a `BIGINT` count of hours and every time predicate had to be hand-written against the array's `units` attr. **This changes existing column types**: the `air_temperature` fixture's `time` goes `FLOAT` → `TIMESTAMP` and `ersstv5`'s goes `DOUBLE` → `TIMESTAMP`, so a query comparing a time column against a bare number (`WHERE time >= 1867128.0`) no longer binds. Pass `decode_times := false` (below) to keep the previous behaviour.

  Decoding covers the calendars that match a wall clock — `proleptic_gregorian`, and `standard`/`gregorian` with a reference on or after the 1582-10-15 Gregorian reform (an absent `calendar` means `standard`, per CF §4.4.1) — plus the UDUNITS step names from `weeks` down to `nanoseconds`, held as an exact rational so sub-microsecond units don't round-trip through a float. The artificial calendars (`noleap`, `365_day`, `all_leap`, `366_day`, `360_day`, `julian`) have years with no wall-clock equivalent and are deliberately left raw rather than given a plausible-looking wrong date; on a `standard`/`gregorian` axis, individual values that fall before the reform decode to `NULL` for the same reason. This needs no new dependency: `chrono` was already pulled in transitively via `duckdb`'s Arrow dependency, and using it directly for the civil-date arithmetic in `src/zarr_reader/cftime.rs` sidesteps the AGPL-3.0 `cftime-rs` blocker that had this deferred in `docs/design.md` decision 3.

### Added
- **`decode_times=` named parameter** on `read_zarr`: `read_zarr(store, decode_times := false)` restores the raw on-disk time offsets, mirroring `xarray.open_zarr(decode_times=False)`. This is the compatibility switch for the type change above.
- **WebAssembly build** (`make wasm_mvp`): the extension compiles for `wasm32-unknown-emscripten` and loads in duckdb-wasm. `zarrs_http` (reqwest) is gated off wasm and every store is routed through `DuckDbStore`, so duckdb-wasm's HTTP filesystem does the fetching; rayon's global pool is built with the current thread as its only worker at extension init; C dependencies built by cc-rs (zstd-sys) get `-fPIC` for the wasm triple only. Native builds are unchanged. The wasm platforms remain in `excluded_platforms`.
- **`string` dtype support** (Zarr v3 `string` / Zarr v2 `|O` + `vlen-utf8` filter), mapped to `VARCHAR`. This is the encoding anndata (and zarr-python generally) use for `obs`/`var` text columns such as `gene_symbol`, which previously failed to read with `unsupported dtype 'string'` (#40). Covers a plain data-variable column read via `read_zarr` from a local store, in both the v3 and v2 on-disk encodings. See `docs/design.md` §Type mapping > Variable-length strings for the implementation approach.

### Fixed
- Coordinate columns now honour their bind-time encoding when values are written. A coord's `ColumnEncoding` was resolved at bind (and drove its advertised DuckDB type) but the scan wrote every coord through the plain-scalar path, so a packed coordinate — integer on-disk with `scale_factor`/`add_offset` — was advertised as `DOUBLE` and then filled with its raw integer bits. Coords and data variables now share one encoding-aware filler.
- A packed-int or CF-time column on-disk as `uint64` with a value above `i64::MAX` no longer silently decodes with a flipped sign. The raw-read helper bit-reinterprets `uint64` into `i64` for arithmetic, which wrapped such values negative; the true magnitude is now recovered before scaling, and an out-of-range CF-time offset decodes to `NULL` like any other unrepresentable instant.
- `DuckDbStore::get_partial_many` loops on short reads instead of failing. DuckDB file systems may return fewer bytes than requested (duckdb-wasm's HTTP filesystem returns 16 KiB pieces).

## [0.1.3] - 2026-08-04

### Changed
- Target DuckDB **v1.5.5** (crate `=1.10505.0`) across the three pinned sites (`Makefile` `TARGET_DUCKDB_VERSION`, `MainDistributionPipeline.yml` `duckdb_version`, `Cargo.toml`) plus `Cargo.lock`. With `USE_UNSTABLE_C_API=1` a binary built for v1.5.4 is rejected by v1.5.5's loader (exact-version `C_STRUCT_UNSTABLE` ABI), which is why the community extension went unavailable after DuckDB 1.5.5. Compiles against 1.10505.0 with no source changes.

### Added
- **CI guard**: a `Release metadata consistency` job in `rust-quality.yml` runs `scripts/check_release_ready.py` on every PR, so the three DuckDB version sites can no longer merge out of sync (an inconsistent pin ships a binary no DuckDB can load).
- **`ref_next` tooling**: `scripts/render_community_descriptor.py` gains `--ref-next` (also `make render-community-descriptor REF=... REF_NEXT=...` and a `ref_next` dispatch input on `community-release.yml`); `check_release_ready.py` validates it is an immutable ref. `ref_next` is the one mechanism that closes the availability gap for a version-locked extension — pre-staging the upcoming-DuckDB commit means the community rebuild has a working binary the moment that DuckDB version ships. See `docs/community-extension-release.md`.
- **Drift bot reach**: `duckdb-version-drift.yml` now runs daily (was weekly) and, when a new DuckDB *minor/major* line appears (which its within-minor patch bumps deliberately skip), opens an idempotent `duckdb-major-bump` tracking issue instead of leaving the jump unnoticed.

### Fixed
- **Bind-time performance on wide remote v2 stores**: `read_zarr`, `read_zarr_metadata`, and `read_zarr_groups` classified dim-groups by calling `open_array()` on every array in the store several times over (coordinate/bounds-var scanning, dim-group classification, pre-opening data-variable arrays), and each `open_array()` costs up to 3 HTTP round trips (a v3 `zarr.json` probe, then `.zarray`, then `.zattrs`). On a store with O(100) arrays — not unusual for real-world multi-variable datasets — that's well over a thousand HTTP requests at bind time before a single byte of chunk data is read. A new `ConsolidatedCacheStore` (#169) serves every `zarr.json`/`.zarray`/`.zattrs`/`.zgroup` lookup — including negative ("not found") answers — from an in-memory map built once from the store's consolidated metadata (`.zmetadata` for v2, `consolidated_metadata` for v3), since that document is already authoritative over every node in the hierarchy. Turns O(arrays) bind-time round trips into O(1); measured wall-clock improvement on a real ~100-array remote store was two orders of magnitude.

## [0.1.1] - 2026-07-11

### Fixed
- Read Zarr **v2** stores over HTTP/S3/GCS/Azure by consuming their consolidated `.zmetadata`. `read_zarr`, `read_zarr_metadata`, and `read_zarr_groups` previously errored on *every* remote v2 store (`remote Zarr store has no consolidated metadata in zarr.json`) because `list_array_names_remote` only understood the Zarr v3 `consolidated_metadata` block. Object stores can't list directories, so v2's separate `.zmetadata` object is the only way to enumerate arrays remotely; the reader now falls back to it. Public v2 stores such as Pangeo GPCP and ARCO-ERA5 read directly from their URL. Covered by `test/test_http_integration.py` (v2 `.zmetadata`, v3 `consolidated_metadata`, and an OME-Zarr bioimage — `array_path` selection plus a nested label image — over a loopback HTTP server; `make test_http_debug`) and by `test/test_http_integration_real.py`, which reads the live public Pangeo GPCP v2 store end-to-end (`make test_http_real`, network-gated). The OME-Zarr fixture is now written with consolidated metadata so it is readable over HTTP as well as locally.
- Read OME-Zarr images by `array_path` even when the store has no consolidated metadata. `array_path=` now opens the requested array directly instead of first listing the whole store, and dimension names fall back to the OME `multiscales.axes` when the array carries neither `dimension_names` nor `_ARRAY_DIMENSIONS`. Real public bioimage stores (e.g. the IDR) now read via `read_zarr(url, array_path='0')` — covered by a network-gated test in `test/test_http_integration_real.py`.
- An array selected by `array_path` exposes its data as a `value` column, so numeric levels (`0`) and nested paths (`labels/nuclei/0`) no longer need to be double-quoted as SQL identifiers.
- `dims=` is now a `LIST(VARCHAR)` — `read_zarr(store, dims=['time','lat','lon'])`, the idiomatic SQL form. It was previously a `VARCHAR` that accepted only a comma-separated or JSON-array *string* (and the SQL-list form errored at bind with `expected ident`).
- Docs now use real, runnable examples verified against the extension — the public GPCP store in `README.md`, plus this repo's fixtures in `docs/README.md` and `docs/ome-zarr.md` — replacing the `path/to/...` and `image.ome.zarr` placeholders.

## [0.1.0] - 2026-07-07

### Fixed
- Rename the crate/extension from `duckdb_zarr` to `zarr` throughout (Cargo package name, `Makefile` `EXTENSION_NAME`, `MainDistributionPipeline.yml`, tests, docs). The prior partial rename only updated `description.yml` and added a `zarr_init_c_api` alias; native community builds still produced and looked for `duckdb_zarr` artifacts because the Rust crate name (and thus the compiled library filename) didn't match, and the `Makefile`'s plain `EXTENSION_NAME=duckdb_zarr` assignment overrode the `EXTENSION_NAME` env var the CI distribution workflow passes in.
- CI: align DuckDB to v1.5.4 across all three version sites — crate `=1.10504.0`, workflow `duckdb_version`, and `Makefile` `TARGET_DUCKDB_VERSION` (the last stamps the extension metadata `duckdb_version`; with `USE_UNSTABLE_C_API=1` the loader requires an *exact* match) — so the built extension loads in the `duckdb_sqllogictest` test runner, which had moved to 1.5.4. Fixes macOS-arm64/Windows test-load version-mismatch failures. Wrapped now-`unsafe` `FlatVector::as_mut_ptr` calls per the 1.10504.0 API.
- CI: make `generate_fixtures.py` resilient to transient xarray-tutorial downloads — skip the network when a fixture is already cached, and retry 5xx with exponential backoff (a GitHub-raw 500 on `ersstv5` was failing the build).

### Added

**Phase 4 — Remote stores, community extension**
- Storage adapter shim: dispatch `s3://`, `gs://`, `az://` paths to DuckDB's FileSystem FFI, enabling S3/GCS/Azure stores via `httpfs` + secrets manager (#19, #20)
- Community extension manifest + submission to duckdb/community-extensions (#22)

**Phase 3 — HTTP/S stores, multi-dim-group selection**
- HTTP/HTTPS store support via `zarrs_http` (#71)
- `read_zarr(path, dims=[...])` named parameter for multi-dim-group selection (#70, #14)

**Phase 2 — Zarr v2, Blosc, replacement scan, projection pushdown**
- Zarr v2 + Blosc/LZ4 codec support (#66)
- Replacement scan: bare `.zarr` paths and directories with `zarr.json`/`.zgroup` rewrite to `read_zarr(...)` automatically (#67, #9)
- Projection pushdown: non-requested data variables skip decompression; coord arrays always pre-loaded (#68, #6)

**Phase 1 — MVP table functions**
- `read_zarr` table function: bind, init, scan; single dim group; `_FillValue` → NULL (#5)
- `read_zarr_metadata` table function: per-array name, dims, dtype, shape, chunk_shape, attrs, role (#4)
- `read_zarr_groups` table function: lists dim groups with dims, shape, chunk_shape (#37)
- Schema inference: classify coord vs data via xarray dim metadata; map Zarr dtypes to DuckDB LogicalType (#3)
- v0.1 SQLLogicTest suite + synthetic Zarr v3 fixture generator (#7)
- v0.2 SQLLogicTest suite: v2 stores, codecs, replacement scan, all xarray tutorial fixtures (#13)

**Phase 0 — Design and spike**
- Design doc for native DuckDB Zarr integration (xarray-sql parity) (#1)
- Spike: verified duckdb-rs replacement-scan, ATTACH, dict-vector, config-var, and pushdown APIs (#24)
- Bootstrap: renamed crate to `duckdb_zarr`, dropped `rusty_quack` stub, added zarrs + ndarray deps (#2)
- Python+xarray test fixture generator covering 11 Zarr v3 test cases (#23)
- zarrs 0.23 Cargo feature audit (#27)

### Fixed
- require python>=3.11 in pyproject.toml so uv uses zarr 3.x on CI (#132)
- port h5py + ZARR_V3_EXPERIMENTAL_API fixes from impl-v2-half to impl-v2 (#131)
- fix generate_fixtures.py: set ZARR_V3_EXPERIMENTAL_API=1 for zarr 2.x compatibility on Python 3.10 CI (#130)
- fix projection pushdown: sort in init() destroys col_idx→out_vec_idx mapping in JOIN context (#129)
- add h5py to pyproject.toml dependencies (required by h5netcdf for ersstv5 fixture) (#128)
- Fix dims= named parameter not registered with DuckDB (#122)

- `copy_scalar!` macro used `from_le_bytes` but zarrs returns native-endian bytes (#75)
- `read_zarr_metadata` paginated scan silently truncated stores with >2048 arrays (#76)
- `read_zarr_metadata` `chunk_shape` column reported chunk-grid dimensions, not per-chunk element shape (#74)
- Float sentinel comparison now uses exact equality per CF §2.5.1 (not `f64::EPSILON` band) (#86)
- Dead code `let _ = n` in `load_coord_array` replaced with `debug_assert_eq!` (#85)
- `decode_work_unit` now reuses the cached `FilesystemStore` from `ReadZarrBind` instead of reopening per chunk (#79)
- `missing_value` CF attribute now used as NULL sentinel when `_FillValue` is NaN/absent (#41)
- Implicit (missing) chunks in sparse Zarr stores now filled with `_FillValue`, not crash (#31)
- `scale_factor`/`add_offset` packed-integer decoding now requires integer on-disk dtype (#57)
- Base64-encoded `_FillValue` in zarr attrs decoded correctly (#56)
- Scalar (0-dim) coordinate variables excluded from row schema (#43)
- CF bounds variables (`time_bnds`, `lat_bnds`) suppressed from dim-group schema (#42)
- 2D non-dimension auxiliary coordinates (`xc`, `yc`) silently excluded from row schema (#64)
- Intra-group chunk shape mismatch now reported as bind error (#40)
- `dimension_names` read from `zarr.json` metadata field (not attrs) for Zarr v3 (#46)
- `list_array_names` handles nested sub-groups at store root without confusing errors (#83)
- Coordinate-only dimension with no backing array synthesizes integer range (#44)
- `read_zarr_metadata` unit struct init; no per-call state (#84)
- Blosc snappy_src build failure on macOS Tahoe resolved (#27)
- SQLLogicTest missing_value coverage: exact null counts for basin_mask and ersstv5 sst (#82)
- Data-variable `ZarrArray` objects pre-opened at bind time; `decode_work_unit` no longer calls `Array::open` per chunk (#96)
- SQLLogicTest projection pushdown coverage: column subset selection with value validation (#90)
- Blosc/LZ4 fixture (`blosc_compressed.zarr`) and SQLLogicTest: end-to-end codec pipeline verification (#92)
- Replacement scan SQLLogicTest: trailing slash, non-existent path error, and multi-group error path (#97)
- `ColumnDef.dim_idx: Option<usize>` replaces fragile `dim_col_k` counter in `fill_chunk_slice` (#81)

### Changed
- S3/GCS/Azure stores without consolidated metadata produce unhelpful error — no fallback listing (#161)
- description.yml extended_description omits S3/GCS/Azure — manifest doesn't document Phase 4 feature (#159)
- description.yml has wrong language and build fields — will break community extension CI (#158)
- extract_file_system uses transmute_copy with no compile-time layout assertion — silent UB risk on duckdb-rs upgrade (#157)
- Duplicate test sections in read_zarr.test — merge artifact produces 4 redundant section blocks (#156)
- duckdb_file_handle_read return value unchecked — silent data corruption on read failure (#155)
- read_zarr dims= named parameter for multi-dim-group selection (#70)
- Write comprehensive SQL tests mirroring xarray-sql test_sql.py (#124)
- wire generate_fixtures as Makefile test prerequisite so CI generates zarr fixtures before running tests (#127)
- Fix rusty_quack remnant in GitHub workflow artifact paths (#123)
- address Phase 2 adversarial review round 2 (#98-#109) (#111)
- Adversarial code review of impl-v2 branch (Phase 2) (#110)
- float_baseline_http fixture is generated but untested — dead test fixture in the repo (#105)
- pushback #87 is correct: VTab init IS shared; Mutex serializes threads — local_init via raw FFI needed (#112)
- Add multi-chunk blosc_compressed fixture and value-checking tests (#120)
- Blosc/LZ4 test coverage is too thin — only COUNT(*) and SUM, no per-value or NULL correctness (#104)
- Blosc fixture uses a single chunk — multi-chunk Blosc decoding never tested (#107)
- Blosc fixture is single-chunk — add multi-chunk fixture and value-checking tests (#104) (#117)
- Projection pushdown tests verify counts only — no multi-column value correctness check (#99)
- projection pushdown tests verify counts only — add column-identity value checks (#114)
- Case-insensitive .ZARR extension in replacement scan: test is a stub that never exercises uppercase (#102)
- Replacement scan calls stat() twice for every non-.zarr table reference in every query (#100)
- Replacement scan claims suffix-less paths via zarr.json/.zgroup probe — contradicts design spec and adds stat overhead to every query (#109)
- Replacement scan: .zarr suffix fires without metadata existence check — asymmetric with non-.zarr probe (#101)
- Projection pushdown: out_col_idx sequential counter silently breaks if DuckDB returns unsorted column indices (#98)
- unsafe impl Sync for ReadZarrBind: SAFETY comment is misleading — ZarrArray is not 'pure data' (#103)
- fill_chunk_slice silently NULLs rows when projected data variable is absent from chunk_bytes — masks bugs as data (#108)
- projection pushdown out_col_idx assumes schema-order projected cols — use sorted structure (#113)
- SAFETY comment misleading: ZarrArray thread-safety not explained (#103) (#119)
- fill_chunk_slice else-branch silently NULLs unreachable path — use unreachable! (#108) (#118)
- replacement scan probes every non-.zarr name with stat() — remove suffix-less probing (#109) (#116)
- replacement scan: .zarr suffix skips zarr.json stat — design spec requires two-step probe (#101) (#115)
- Entrypoint min API version updated from `"v1.5.2"` to `"v1.2.0"` (matches duckdb-rs 1.10502.0 default) (#93)
- 2D non-dim coordinate arrays are silently excluded (not a bind error) — matches xarray behavior (#78, #95)
- `duckdb_vector_size()` queried at runtime instead of hardcoded `STANDARD_VECTOR_SIZE=2048` (#65)
- CHANGELOG Phase 1 entries reclassified from Changed to Added (#62, #80)
