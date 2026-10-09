# Contributing to duckdb-zarr

Thank you for your interest in contributing!

## Getting started

1. Fork the repository and clone your fork.
2. Install dependencies: Rust (stable toolchain) and DuckDB.
3. Build the extension: `make`
4. Run the test suite: `make test`

## How to contribute

We use GitHub Issues to track bugs and feature requests — searching before opening a new one helps avoid duplicates.

- **Bug reports** — open an issue with a minimal reproducible example, including your OS, Rust version, and DuckDB version.
- **Feature requests** — open an issue describing the use case and motivation before writing code. This lets maintainers give early feedback and avoids wasted effort.
- **Pull requests** — keep changes focused; one concern per PR. Link the related issue.

## Development workflow

```bash
# Build
make

# Run tests
make test

# Format Rust code
make fmt

# Check formatting and run Clippy
make lint

# Install the Git hooks
uv run prek install

# Run the hooks against the whole repository
uv run prek run --all-files
```

Tests live in `test/sql/` as DuckDB `.test` files. New functionality should include corresponding tests.

## Code style

- Rust: `make lint` must pass before submission.
- Git hooks: `prek` applies rustfmt and runs Clippy before each commit.
- Commit messages: short imperative subject line, blank line, then details if needed.

## Submitting a pull request

1. Ensure `make test` passes locally.
2. Describe *what* changed and *why* in the PR body.
3. Be responsive to review feedback — maintainers may suggest changes before merging.

## Creating a community-extension release

DuckDB community-extension releases are built from `description.yml` by
`duckdb/community-extensions`. This repository keeps the descriptor in-tree so
release changes can be reviewed before the community PR is opened.

Before publishing:

```bash
make release-check
make test_release
make render-community-descriptor REF=v0.1.0
```

The checked-in `description.yml` can keep `repo.ref: main`; the render command
creates the descriptor that must be submitted to `duckdb/community-extensions`
with `repo.ref` set to the release tag or commit hash.

Release checklist:

1. Confirm `Main Extension Distribution Pipeline` and `Rust quality` are green on
   the commit to publish.
2. Update `CHANGELOG.md` (still manual). Do **not** bump `Cargo.toml`,
   `pyproject.toml`, or `description.yml` — they keep a development-placeholder
   version that is synced from the release tag automatically; see
   [docs/versioning.md](docs/versioning.md).
3. Create and publish a GitHub Release with a tag matching the project version,
   such as `v0.1.0`.
4. Download the descriptor artifact from the `Community Extension Release`
   workflow.
5. Open a PR against `duckdb/community-extensions` adding or updating
   `extensions/zarr/description.yml` with that artifact.

See [docs/community-extension-release.md](docs/community-extension-release.md)
for the detailed release and DuckDB-version policy.

## DuckDB version policy

Community extensions are built for DuckDB's latest stable release. This extension
currently uses DuckDB's unstable C API, so the built binary only loads into the
exact DuckDB version stamped into the extension metadata.

When bumping DuckDB, update all of these in one PR:

- `Makefile` `TARGET_DUCKDB_VERSION`
- `Cargo.toml` exact `duckdb` crate pin, plus `Cargo.lock`

The distribution workflow reads its `duckdb_version` from the Makefile at build
time, so it needs no separate edit.

The crate pin follows the `duckdb-rs` encoding used by this project. For example,
DuckDB `v1.5.4` maps to `duckdb = "=1.10504.0"`.

If DuckDB's community repository is testing both latest stable and current
DuckDB `main`, keep the published stable-compatible commit in `repo.ref` and add
`repo.ref_next` only for a separate commit or branch that is compatible with
DuckDB `main`.

## Code of Conduct

This project follows the [Contributor Covenant Code of Conduct](CODE_OF_CONDUCT.md).
All participants are expected to uphold it.
