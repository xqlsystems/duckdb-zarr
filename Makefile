.PHONY: clean clean_all generate_fixtures

PROJ_DIR := $(dir $(abspath $(lastword $(MAKEFILE_LIST))))

EXTENSION_NAME=duckdb_zarr

# Set to 1 to enable Unstable API (binaries will only work on TARGET_DUCKDB_VERSION, forwards compatibility will be broken)
# Note: currently extension-template-rs requires this, as duckdb-rs relies on unstable C API functionality
USE_UNSTABLE_C_API=1

# Target DuckDB version
TARGET_DUCKDB_VERSION=v1.5.2

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

generate_fixtures:
	@if command -v uv >/dev/null 2>&1; then \
		uv run scripts/generate_fixtures.py; \
	elif [ -f "$(PYTHON_VENV_BIN)" ]; then \
		$(PYTHON_VENV_BIN) -m pip install --quiet "xarray" "zarr>=3.0.0" numpy scipy h5netcdf h5py pooch; \
		$(PYTHON_VENV_BIN) scripts/generate_fixtures.py; \
	else \
		echo "Error: neither uv nor configure/venv found. Run 'make configure' or install uv." >&2; \
		exit 1; \
	fi

clean: clean_build clean_rust
clean_all: clean_configure clean
