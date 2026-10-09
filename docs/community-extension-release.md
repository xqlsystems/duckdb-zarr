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

Because `USE_UNSTABLE_C_API=1` binds each binary to an exact DuckDB version (see
[DuckDB Version Policy](#duckdb-version-policy)), the community extension goes
stale the moment DuckDB ships a version this extension has not been rebuilt for.
Three pieces of automation keep that window small:

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

The descriptor excludes `wasm_mvp`, `wasm_eh`, `wasm_threads`, and
`linux_amd64_musl`. The local distribution workflow uses the same exclusion set.
Re-enable platforms only after the CI build and SQLLogic tests pass for them.

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
This extension currently sets `USE_UNSTABLE_C_API=1`, so the produced binary is
not forward-compatible across DuckDB patch releases: the loader expects the exact
DuckDB version stamped into the extension metadata.

That means a DuckDB bump must update all version pins in one reviewed change:

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

The `duckdb` crate version used here encodes the DuckDB version. For the current
pin, DuckDB `v1.5.4` maps to:

```toml
duckdb = { version = "=1.10504.0", features = ["loadable-extension"] }
```

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

For a version-locked (Rust / `USE_UNSTABLE_C_API=1`) extension this is the one
mechanism that closes the availability gap entirely: because a single commit can
only build against one DuckDB version, `ref` covers today's stable and `ref_next`
covers the upcoming release, so the community rebuild has a working binary the
**moment** the new DuckDB version ships — no reactive catch-up.

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
