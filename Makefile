.PHONY: clean clean_all clippy fmt fmt-check generate_fixtures lint release-check render-community-descriptor test_http test_http_debug test_http_release test_http_real test_kerchunk test_kerchunk_debug test_kerchunk_deep

PROJ_DIR := $(dir $(abspath $(lastword $(MAKEFILE_LIST))))

EXTENSION_NAME=zarr

# Set to 1 to enable Unstable API (binaries will only work on TARGET_DUCKDB_VERSION, forwards compatibility will be broken)
# Note: currently extension-template-rs requires this, as duckdb-rs relies on unstable C API functionality
USE_UNSTABLE_C_API=1

# Target DuckDB version
TARGET_DUCKDB_VERSION=v1.5.5

all: configure debug

# Include makefiles from DuckDB
include extension-ci-tools/makefiles/c_api_extensions/base.Makefile
include extension-ci-tools/makefiles/c_api_extensions/rust.Makefile

configure: venv platform extension_version

# DuckDB CI sets CC=gcc/CXX=g++ for windows_amd64_mingw but the Rust default
# target on Windows is x86_64-pc-windows-msvc (MSVC linker).  cc-rs picks up
# the ambient CC=gcc and emits GCC COMDAT sections that link.exe rejects (LNK1143).
# Fix: build for the GNU Windows target instead — same GCC toolchain end-to-end.
# The CI already installs x86_64-pc-windows-gnu via dtolnay/rust-toolchain@stable.
# GNU Make expands recipe variables at execution time, so these post-include
# assignments override the values set in rust.Makefile's else branch.
ifeq ($(DUCKDB_PLATFORM),windows_amd64_mingw)
TARGET := x86_64-pc-windows-gnu
TARGET_INFO := --target $(TARGET)
TARGET_PATH := ./target/$(TARGET)
endif

debug: build_extension_library_debug build_extension_with_metadata_debug
release: build_extension_library_release build_extension_with_metadata_release

test: test_debug
test_debug: generate_fixtures test_extension_debug
test_release: generate_fixtures test_extension_release

# HTTP integration tests. Build the extension first (make debug / make release).
# pytest runs via uv, so no venv setup is needed; duckdb is pinned to the target
# DuckDB version. Both files run together as one suite:
#   test_http_integration.py      — loopback http.server + synthetic fixtures
#                                   (deterministic; v2 .zmetadata + v3 consolidated_metadata)
#   test_http_integration_real.py — reads a real public v2 store over the internet
#                                   (network-marked; skips when the store is unreachable)
# test_http_real runs only the real-data file.
test_http: test_http_debug
test_http_debug: generate_fixtures
	uv run --with 'duckdb==$(TARGET_DUCKDB_VERSION:v%=%)' \
		pytest test/test_http_integration.py test/test_http_integration_real.py \
		--extension build/debug/$(EXTENSION_NAME).duckdb_extension -v
test_http_release: generate_fixtures
	uv run --with 'duckdb==$(TARGET_DUCKDB_VERSION:v%=%)' \
		pytest test/test_http_integration.py test/test_http_integration_real.py \
		--extension build/release/$(EXTENSION_NAME).duckdb_extension -v
test_http_real:
	uv run --with 'duckdb==$(TARGET_DUCKDB_VERSION:v%=%)' \
		pytest test/test_http_integration_real.py \
		--extension build/debug/$(EXTENSION_NAME).duckdb_extension -v

# pytest and hypothesis come from the `dev` dependency group in pyproject.toml
# (uv run installs it by default). Only the Python duckdb package is pinned on
# the command line, to match TARGET_DUCKDB_VERSION.
# Property-based tests for kerchunk manifests (test/test_kerchunk_property.py).
# Hypothesis generates random datasets, VirtualiZarr indexes them, and the
# extension must read the manifest exactly like the data. Build first.
# HYPOTHESIS_PROFILE=ci|default|deep sets the example count; test_kerchunk_deep
# is the long search for hunting bugs.
test_kerchunk: generate_fixtures
	uv run --with 'duckdb==$(TARGET_DUCKDB_VERSION:v%=%)' \
		pytest test/test_kerchunk_property.py \
		--extension build/release/$(EXTENSION_NAME).duckdb_extension -v
test_kerchunk_debug: generate_fixtures
	uv run --with 'duckdb==$(TARGET_DUCKDB_VERSION:v%=%)' \
		pytest test/test_kerchunk_property.py \
		--extension build/debug/$(EXTENSION_NAME).duckdb_extension -v
test_kerchunk_deep: generate_fixtures
	HYPOTHESIS_PROFILE=deep uv run --with 'duckdb==$(TARGET_DUCKDB_VERSION:v%=%)' \
		pytest test/test_kerchunk_property.py \
		--extension build/release/$(EXTENSION_NAME).duckdb_extension -v

fmt:
	cargo fmt --all

fmt-check:
	cargo fmt --all -- --check

clippy:
	cargo clippy --lib --all-features -- -D warnings

lint: fmt-check clippy

release-check:
	python3 scripts/check_release_ready.py

# Optional REF_NEXT= pins the commit built for the upcoming DuckDB version
# (emitted as repo.ref_next). Usage: make render-community-descriptor REF=v0.1.1 [REF_NEXT=<sha>]
render-community-descriptor:
	@test -n "$(REF)" || (echo "Usage: make render-community-descriptor REF=v0.1.1 [REF_NEXT=<commit>]" >&2; exit 1)
	python3 scripts/render_community_descriptor.py --ref "$(REF)" $(if $(REF_NEXT),--ref-next "$(REF_NEXT)",) --out build/community-extensions/extensions/zarr/description.yml
	python3 scripts/check_release_ready.py --description-path build/community-extensions/extensions/zarr/description.yml --strict-community-ref

generate_fixtures:
	@if command -v uv >/dev/null 2>&1; then \
		uv run scripts/generate_fixtures.py; \
	elif [ -f "$(PYTHON_VENV_BIN)" ]; then \
		$(PYTHON_VENV_BIN) -m pip install --quiet "xarray" "zarr>=3.0.0" numpy scipy h5netcdf h5py pooch \
			"virtualizarr>=2.5.1" "tifffile>=2026.3.3" "virtual-tiff>=0.5.0"; \
		$(PYTHON_VENV_BIN) scripts/generate_fixtures.py; \
	else \
		echo "Error: neither uv nor configure/venv found. Run 'make configure' or install uv." >&2; \
		exit 1; \
	fi

clean: clean_build clean_rust
clean_all: clean_configure clean
