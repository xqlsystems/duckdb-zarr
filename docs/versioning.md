# Versioning

`duckdb-zarr` follows a **dynamic versioning** model: the git release tag is
the source of truth, and the version strings checked into the repository are
development placeholders that get synced from the tag at release time. This
mirrors the approach used by the sibling [`arrow-lint`][arrow-lint] and
[`zarr-lint`][zarr-lint] projects.

## Two independent version concepts

This project has two version numbers that are easy to conflate:

1. **The DuckDB extension's own `extension_version` metadata** — stamped into
   the compiled `.duckdb_extension` binary and reported by DuckDB at runtime.
2. **The `Cargo.toml` / `pyproject.toml` / `description.yml` version trio** —
   used for the community-extension descriptor and human-facing docs.

They are synced independently, by two different mechanisms.

### The extension's runtime version already reads the git tag

DuckDB's own build tooling (`extension-ci-tools/scripts/configure_helper.py`,
vendored as a submodule) autodetects the extension version from git and
writes it to `configure/extension_version.txt`:

```python
git_tag = subprocess.getoutput("git tag --points-at HEAD")
EXTENSION_VERSION = git_tag or subprocess.getoutput("git --no-pager log -1 --format=%h")
```

The `Makefile` stamps that file's contents into the compiled binary's
metadata (`-evf configure/extension_version.txt`). This already works today,
with no code in this repository involved — every build, including the one
`duckdb/community-extensions` performs against this repo's tagged `ref`, is
tied to the git tag automatically:

```sql
LOAD zarr;
SELECT extension_version FROM duckdb_extensions() WHERE extension_name = 'zarr';
```

`configure/` is gitignored and rebuilt by `make configure`, so there is
nothing to keep in sync by hand here — this is why the extension itself never
needed a Rust source change to "read the git tag": `duckdb_extensions()` is
the read path, and `zarrs`/the crate's own `Cargo.toml` `version` field never
enters the picture. (This differs from a typical `cargo install`able binary,
where `env!("CARGO_PKG_VERSION")` really would end up in the shipped artifact
— it does not here, because the extension is loaded via DuckDB's C API, not
run as a standalone binary carrying its own `--version`.)

## Source of truth for the descriptor/docs trio

The Cargo, Python, and community-descriptor version fields are a separate,
purely cosmetic concern: they must agree with each other so the rendered
`description.yml` submitted to `duckdb/community-extensions` and the
generated docs are consistent. The committed values are **development
placeholders** — not expected to track the latest release:

```toml
# Cargo.toml
[package]
version = "0.1.3"
```

```toml
# pyproject.toml
[project]
version = "0.1.3"
```

```yaml
# description.yml
extension:
  version: 0.1.3
```

## Verifying consistency

[`scripts/check_release_ready.py`](../scripts/check_release_ready.py) runs on
every PR (`Release metadata consistency` job in
[`.github/workflows/rust-quality.yml`](../.github/workflows/rust-quality.yml))
and fails if these three placeholders drift apart from each other. It does
**not** check them against any git tag — that's not the release gate, since
the placeholder is never expected to already equal the next tag.

## Release tags

Release tags use a `v` prefix; the package version omits it:

| Git tag  | Package version |
| -------- | ---------------- |
| `v0.1.4` | `0.1.4`          |

At release time,
[`scripts/sync_release_version.py`](../scripts/sync_release_version.py)
rewrites `Cargo.toml`, `pyproject.toml`, `description.yml`, and the `zarr`
package entry in `Cargo.lock` to match the tag, inside the release workflow's
CI checkout — **no commit is made to `main` (or any branch)**:

```console
$ python3 scripts/sync_release_version.py v0.1.4
tag=v0.1.4 version=0.1.4: synced Cargo.toml, pyproject.toml, description.yml, Cargo.lock

$ python3 scripts/sync_release_version.py v0.1.4 --check
OK: tag v0.1.4 matches all version sites.
```

This is an **ephemeral edit**: it only runs inside
[`.github/workflows/community-release.yml`](../.github/workflows/community-release.yml)'s
CI checkout, right before rendering the community descriptor, and is never
committed. The repository keeps its development-placeholder version between
releases — publishing a release no longer requires a manual `Cargo.toml` /
`pyproject.toml` / `description.yml` bump beforehand, only a `CHANGELOG.md`
entry (still maintained by hand; see
[`CHANGELOG.md`](../CHANGELOG.md)) and the tag itself.

Committing the synced version back to `main` was considered and rejected: it
would require the release workflow to push a commit (or open a PR) after
every tag, adding a second source of truth that could drift from the tag it
was derived from, plus bot-authored commits/PRs to review on every release.
Leaving the sync ephemeral keeps the git tag as the single source of truth
and matches the pattern already proven in `arrow-lint`/`zarr-lint`.

If a `workflow_dispatch` run targets a raw commit hash instead of a tag (the
`description.yml` `repo.ref` "publish an older/manual ref" path), there is no
tag to sync from, so the sync step is skipped and the checked-in placeholder
is rendered as-is — the same behavior this workflow had before dynamic
versioning existed.

## Why not release-plz?

[release-plz](https://release-plz.dev/) automates conventional-commit-driven
version bumps, changelog generation, and crates.io publishing via release
PRs. It was considered for this issue but doesn't fit `duckdb-zarr`: this
crate is never published to crates.io (it's a `cdylib` C API extension, not a
library), its DuckDB engine test pin and C API compatibility floor are managed
separately (see
[`docs/community-extension-release.md`](community-extension-release.md#duckdb-version-policy)),
and the three-file descriptor trio it would still need a custom step for is
exactly what `sync_release_version.py` already handles. The
placeholder-plus-tag-sync pattern from `arrow-lint`/`zarr-lint` covers the
same need with less new surface area and matches the pattern already proven
in this maintainer's other projects.

[arrow-lint]: https://github.com/d33bs/arrow-lint
[zarr-lint]: https://github.com/d33bs/zarr-lint
