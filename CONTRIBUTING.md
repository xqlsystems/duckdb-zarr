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

## Code of Conduct

This project follows the [Contributor Covenant Code of Conduct](CODE_OF_CONDUCT.md).
All participants are expected to uphold it.
