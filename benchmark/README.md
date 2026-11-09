# Native performance benchmarks

Start small: one deterministic benchmark for the real compiled extension.
This follows the separate benchmark-job pattern discussed in
[xarray-sql PR #306](https://github.com/xqlsystems/xarray-sql/pull/306)
and addresses [duckdb-zarr issue #23](https://github.com/xqlsystems/duckdb-zarr/issues/23).

## Run locally

Requires the usual native build tools, Python 3.11+ and uv. From a checkout
with submodules initialized:

```sh
make benchmark
```

The target configures and builds the release extension, uses the DuckDB
engine pinned in the Makefile, and installs the optional `benchmark`
dependency group from `uv.lock`. It saves a pytest-benchmark report to
`benchmark/results/latest.json`. Keep a separate run with:

```sh
make benchmark BENCHMARK_OUTPUT=benchmark/results/baseline.json
```

Generated reports and pytest-benchmark's optional `.benchmarks/` storage
are ignored by git. No remote dataset is downloaded. Building and installing
dependencies can still need network access.

Normal pytest runs exclude the `benchmark` marker by default, and normal
`make test` targets do not invoke this benchmark. To opt in directly, pass
`-m benchmark`, `--benchmark-only` and the actual extension path. Direct
invocations record an unspecified build profile unless
`DUCKDB_ZARR_BENCHMARK_PROFILE` is set; `make benchmark` records `release`.

## What is measured

The synthetic fixture has a fixed seed and one float64 variable on a
100 × 50 × 50 `(time, lat, lon)` grid. It is a local Zarr v3 store with
10 × 25 × 25 chunks. Its actual codec configuration and data hash are
recorded in the report.

Values come from explicit PCG64 integer draws scaled by a power of two, so
each float is exactly representable. The test checks a fixed SHA-256 of their
little-endian float64 bytes. This avoids the different fixture hashes observed
with normal-distribution draws on ARM macOS and Linux x86-64. Changing this
workload requires deliberately updating that fingerprint before collecting
new history; do not compare it with the earlier normal-distribution prototype.

The benchmark runs this query with one DuckDB thread:

```sql
SELECT lat, lon, AVG(temperature) AS mean_temperature
FROM read_zarr(?)
GROUP BY lat, lon
ORDER BY lat, lon;
```

The timed function binds and executes the query, reads/decodes the store,
groups the data, and materializes all 2,500 output rows with `fetchall()`.
After timing, every coordinate and grouped mean is checked against NumPy.
The benchmark fails on wrong results regardless of its speed.

Store creation, connection setup, extension loading, metadata capture and
NumPy checks are outside the timed region. There is one warm-up round and
ten measured rounds, each with one query. The same store and connection
are reused. This is a warm repeated local query: the OS cache is not flushed.
It is not a cold-disk, remote HTTP, browser/WASM, peak-memory or broad
scientific-workload benchmark.

Besides pytest-benchmark's timings and machine information, `extra_info`
records the checkout revision and dirty state, actual extension SHA-256,
build profile, runtime/package versions, Rust version, thread count,
shape/chunks/dtype/codecs, seed and fixture-values hash. The checkout revision
identifies the harness checkout, not proof of an arbitrary supplied binary's
source; the binary hash identifies what actually ran.

## CI results and history

The dedicated `Native benchmarks` workflow runs on relevant pull requests,
main-branch pushes and manual dispatches. It builds the native release binary
on Ubuntu 24.04 with Python 3.12. Every run attempts to upload the raw JSON
as `native-benchmark`, retained for 90 days. Failed correctness/build tests
still fail CI; timing changes alone do not.

Only a successful push/manual run on upstream `main` can write persistent
history. Pull requests, including fork PRs, have read-only permissions and
cannot publish history. Fork main-branch runs also cannot write upstream
history. All new actions use full commit pins.

One-time maintainer setup: create a branch named `benchmark-history` from
`main` in the upstream repository. Until it exists, the history job reports
a notice and skips its write; raw JSON artifacts are still saved. Network
or permission failures are not treated as a missing branch.

The history job uses github-action-benchmark to append data under `benchmark/`
on that branch and adds a comparison summary. Automatic alert comments and
timing-based failures are disabled. The configured 200% threshold is reserved
for future alerts, not an active timing gate.
Hosted-runner noise, package/compiler changes and machine differences need
history before tighter thresholds make sense.

This is a history store, not a new Pages deployment. The existing documentation
site is published through a separate workflow, so these branch writes do not
publish a dashboard at the documentation URL or change Pages settings. A
public chart can be integrated into that build later. The fork's existing
`gh-pages` preview is untouched.

CodSpeed is not enabled. The standard `benchmark` fixture leaves that option
open, but adding a service/app setup is separate work.

## Reading comparisons

Compare runs with the same workload, release profile, thread count, fixture
hash, codec configuration and machine class. Inspect package/compiler changes
before attributing a difference to extension code. Local macOS numbers and
Ubuntu CI numbers are separate baselines. A dirty local checkout is useful
for experiments, but not a clean commit baseline.

Keep this first case stable. Add separate named cases for projection, bounds,
other codecs or remote stores when a specific question needs them; do not
silently change this fixture and treat it as the same historical workload.
