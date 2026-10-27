# Community Extension Release

This project is set up to publish through DuckDB's community extension repository.
DuckDB's process is descriptor-driven: submit a PR to `duckdb/community-extensions`
with this repository's `description.yml`, and the community repository builds it
with the same `extension-ci-tools` distribution workflow used here.

## Local Release Gate

Run the local checks before opening the community PR:

```sh
make release-check
make test_release
```

`make release-check` verifies the release-critical metadata that is easy to drift
in a Rust C API extension:

- `Makefile` `TARGET_DUCKDB_VERSION`
- `Cargo.toml` exact `duckdb` crate pin
- `description.yml` `language`, `build`, `requires_toolchains`, and excluded
  platforms
- `Cargo.toml` / `pyproject.toml` / `description.yml` development-placeholder
  versions agreeing with each other (see
  [docs/versioning.md](versioning.md) — this does not check them against any
  git tag; that sync happens automatically at release time)
- that `MainDistributionPipeline.yml` resolves `duckdb_version` from the
  Makefile pin at build time rather than carrying its own literal (see
  [DuckDB Version Policy](#duckdb-version-policy))

For the final community PR, also run:

```sh
python3 scripts/check_release_ready.py --strict-community-ref
```

That strict mode fails until `description.yml` `repo.ref` is an immutable release
tag such as `v0.1.0` or a 40-character commit hash. Do not submit `ref: main`.

For release automation, leave the checked-in descriptor at `ref: main` and
render the community descriptor for a tag:

```sh
make render-community-descriptor REF=v0.1.0
```

The rendered file is written to
`build/community-extensions/extensions/zarr/description.yml` with
`repo.ref` set to the release tag.

## GitHub Release Automation

Publishing a GitHub Release runs `.github/workflows/community-release.yml`.
That workflow:

1. Renders a community descriptor for the release tag.
2. Validates the generated descriptor in strict mode.
3. Uploads the descriptor as a workflow artifact.

The workflow intentionally does not store or use a cross-repository token. A
maintainer downloads the artifact and opens the PR to
`duckdb/community-extensions` manually. This keeps release automation read-only
inside this repository while still making the submitted descriptor reproducible.

Running the workflow via **Run workflow** (workflow_dispatch) also accepts an
optional `ref_next` input. Supply the commit built for the upcoming DuckDB
version and it is emitted as `repo.ref_next` in the rendered descriptor — see
[Stable vs. Current DuckDB Main](#stable-vs-current-duckdb-main).

## Staying Current With DuckDB Releases

The native adapter uses DuckDB's frozen v1.5.6 C API (see
[DuckDB Version Policy](#duckdb-version-policy)). A version bump no longer
necessarily requires a different binary, but every supported engine still needs
build and query tests. Three pieces of automation support that process:

- **CI guard** — the `Release metadata consistency` job in
  `.github/workflows/rust-quality.yml` runs `scripts/check_release_ready.py` on
  every PR, so the DuckDB version pins can never merge out of sync.
- **`DuckDB Version Drift`** (`.github/workflows/duckdb-version-drift.yml`) runs
  **daily**. Within the current minor line it opens a `chore/bump-duckdb-*` PR
  as soon as pip `duckdb` and the matching `duckdb` crate publish a new patch.
  When a new **minor or major** DuckDB line appears (e.g. `1.5.x → 1.6.0`) it
  opens a `duckdb-major-bump` tracking issue instead, because that is a manual
  migration (new CI-tools codename branch, possible source changes).
- **`ref_next`** lets you pre-stage the next line so there is *no* gap at all —
  covered below.

The maintainer still cuts the release and opens the community PR (a deliberate
choice — see above), but the source is kept release-ready automatically.

## Descriptor Notes

The descriptor intentionally marks this as a Rust cargo extension and requests
the extra toolchains that the community CI must install:

```yaml
extension:
  language: Rust
  build: cargo
  requires_toolchains: "rust;python3"
```

The descriptor excludes only `linux_amd64_musl`. The local distribution workflow
uses the same exclusion set. The `wasm_mvp`, `wasm_eh`, and `wasm_threads`
platforms are built by CI (the wasm matrix has no test step — it builds and
uploads artifacts only); re-add one to the exclusions if its build stops
passing.

## Submission Steps

1. Ensure `main` is green for `Main Extension Distribution Pipeline` and
   `Rust quality`.
2. Update `CHANGELOG.md` (still manual). Do **not** bump `Cargo.toml`,
   `pyproject.toml`, or `description.yml` — they keep a development-placeholder
   version that `scripts/sync_release_version.py` syncs from the release tag
   automatically inside `community-release.yml`; see
   [docs/versioning.md](versioning.md).
3. Create and publish a GitHub Release with a tag matching the project version,
   such as `v0.1.0`.
4. Download the validated descriptor artifact from the `Community Extension
   Release` workflow.
5. Open a PR against `duckdb/community-extensions` adding or updating
   `extensions/zarr/description.yml` with that artifact.

After the PR is merged and built by DuckDB's community infrastructure, users can
install with:

```sql
INSTALL zarr FROM community;
LOAD zarr;
```

## DuckDB Version Policy

DuckDB community extensions are distributed for DuckDB's latest stable release.
This adapter sets `USE_UNSTABLE_C_API=0` and `DUCKDB_CAPI_VERSION=v1.5.6`.
DuckDB 2.0 stabilized the formerly unstable v1 functions at that API version.
The binary therefore advertises `C_STRUCT` with `v1.5.6` metadata and exports
`zarr_init_c_api`. Advertising `C_STRUCT_UNSTABLE` on DuckDB 2.0 instead selects
the incompatible `zarr_init_c_api_v2` entrypoint.

`TARGET_DUCKDB_VERSION=v2.0.0` is the engine used by CI and Python tests, not
the binary's API floor. The same native build has been tested on DuckDB 1.5.6
and 2.0.0. Older engines are below its declared API floor. Other engines and
platforms need their own validation; accepting the metadata is not a query test.

A target-engine bump must update these pins in one reviewed change:

- `Makefile` `TARGET_DUCKDB_VERSION`
- `Cargo.toml` exact `duckdb` crate pin
- `Cargo.lock`

The local release gate checks the first two and that the distribution pipeline
resolves its `duckdb_version` from the Makefile. `Cargo.lock` should change when
`cargo update -p duckdb --precise <crate-version>` is run.

The distribution pipeline deliberately carries **no** `duckdb_version:` literal
of its own: `.github/workflows/MainDistributionPipeline.yml` resolves it from
`TARGET_DUCKDB_VERSION` at build time. A literal inside a workflow file cannot
be updated by an automated PR — GitHub refuses to let the Actions `GITHUB_TOKEN`
push changes to files under `.github/workflows/` under any `permissions` grant
— so the drift bot's bump PRs would fail to push and the literal would silently
go stale. One pin site, the Makefile, keeps the automated path working.

The `duckdb` crate version encodes the engine's major, minor and patch numbers:
`v1.5.6` maps to `1.10506.0`, and the current `v2.0.0` maps to:

```toml
duckdb = { version = "=1.20000.0", features = ["loadable-extension"] }
```

Do not bump `DUCKDB_CAPI_VERSION` with a routine engine update. It describes the
API this adapter needs, not the installed engine. If that requirement changes,
update the version requested in `src/lib.rs` and the compatibility tests too.

Use the automated `DuckDB Version Drift` workflow for routine patch-line bumps.
Treat major or minor DuckDB bumps as manual migrations: update the pins, rebuild,
run `make lint`, and run `make test_release` before changing `description.yml`
for community publication.

## Stable vs. Current DuckDB Main

Near DuckDB releases, `duckdb/community-extensions` can test extensions against
both the latest stable release and DuckDB `main`. If one commit cannot support
both, maintain two refs:

```yaml
repo:
  github: xqlsystems/duckdb-zarr
  ref: <stable-compatible-tag-or-commit>
  ref_next: <duckdb-main-compatible-commit>
```

Use `ref` for the commit that works with the latest stable DuckDB release. Use
`ref_next` only when DuckDB `main` needs source changes that should not replace
the stable build yet. After DuckDB releases, the community repository can promote
the `ref_next` commit to `ref`.

The current frozen-v1 adapter can use the same source and binary on the two
validated engine versions. `ref_next` is only needed when a future engine needs
source or ABI changes that cannot serve the current stable release. Validate
both refs before publication; `ref_next` by itself does not establish support.

Render a descriptor carrying both refs with:

```sh
make render-community-descriptor REF=v0.1.1 REF_NEXT=<upcoming-commit-sha>
```

`REF_NEXT` accepts a 40-character commit hash or a `vX.Y.Z` tag; the strict
release check validates it is immutable. The rendered descriptor is written to
`build/community-extensions/extensions/zarr/description.yml`. The
`community-release.yml` workflow exposes the same `ref_next` as a dispatch input.

Practical flow when a new DuckDB line is announced:

1. Branch off `main`, apply the migration (CI-tools codename branch, version
   pins, any source fixes), and get CI green — this is the commit tracked by the
   `duckdb-major-bump` issue the drift bot opens.
2. Submit a community PR whose descriptor sets `ref` to the current stable
   release and `ref_next` to that branch's HEAD commit.
3. Once DuckDB releases, promote `ref_next` to `ref` on your next routine
   submission.
