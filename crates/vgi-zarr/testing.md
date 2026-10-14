# Testing the VGI worker

From the repository root:

```sh
cargo fmt --manifest-path crates/vgi-zarr/Cargo.toml -- --check
cargo clippy --manifest-path crates/vgi-zarr/Cargo.toml --locked --all-targets -- -D warnings
cargo test --manifest-path crates/vgi-zarr/Cargo.toml --locked
```

The worker is excluded from the root workspace, so `cargo test --workspace`
alone does not run these tests. Building requires crates.io access on the first
run; the tests themselves need no public datasets, DuckDB installation, or Python
packages. They create temporary Zarr stores and loopback HTTP servers and spawn
the compiled `vgi-zarr` binary through its real attach/bind/scan protocol.

## Add a regression with the testing kit

The development dependency enables `vgi-client`'s `testing` feature. In an
integration test, start the actual binary and create a typed filter:

```rust
use std::sync::Arc;
use arrow_array::Int64Array;
use vgi_client::testing::{scan_options, Comparison, FilterExpr, TestWorker};

let mut worker = TestWorker::connect_subprocess(
    &[env!("CARGO_BIN_EXE_vgi-zarr")], "zarr",
)?;
let filter = FilterExpr::compare(
    "x", Comparison::Eq, Arc::new(Int64Array::from(vec![1])),
).or(FilterExpr::is_not_null("a"));
// The schema is the full bind schema; project x while filtering on a too.
let options = scan_options(&full_schema, &filter, Some(vec![0]))?;
```

Use `worker.scan(&bind_spec, &options)` with a hand-written `ScanResult` to
assert exact types and values. `ScanResult::assert_matches` compares the schema
and row multiset, including NULLs and duplicate counts, while allowing different
batch boundaries and row order. This catches integer rounding and unsigned
overflow that a floating-point comparison would hide.

For pushdown equivalence, use `worker.check_pushdown(&bind_spec, &options,
|full| ...)`: the callback receives an unfiltered full scan and returns the rows
selected by an **independent** reference predicate. The kit projects those
expected rows and compares them with the pushed result. Do not use VGI's filter
evaluator in the reference: the same mistake could then pass on both sides.
The `select` helper in [tests/worker.rs](tests/worker.rs) builds an Arrow Boolean
mask using ordinary Rust predicates, then uses Arrow's `filter_record_batch`.

The fixture writer in [tests/support/mod.rs](tests/support/mod.rs) creates small
v3 stores using zarrs. Add explicit expected-value tests when changing decoding,
and reference-selection tests when changing pruning. The suite covers:

- Unknown columns under OR; SQL NULL, IN, NOT IN, and NOT semantics.
- Filter-only columns, reordered/empty projections, and fully pruned results.
- Integer widths, signedness, values beyond 2^53, and exact timestamp literals.
- CF packing, fill sentinels, strings, floating bounds, and multidimensional edge chunks.
- Actual avoided HTTP requests, 403/503 errors, and legitimate 404 fill values.
- Sharded reads with 206 ranges and 200 full-object fallback; indexes at either
  end of the shard, with and without CRC32C.
- Cross-query index reuse, independent byte counters, and dataset eviction.
- In-band lifecycle messages, execution IDs, and measured source counters.

## Inspect execution diagnostics

`TestWorker::client_mut().set_worker_log_sink(...)` captures logs from the normal
RPC stream. The diagnostic test installs a sink before scanning and checks both
start/finish correlation and the actual requested object keys. Running two scans
in one process verifies that cached metadata does not carry over counters or
execution IDs. The sharding test executes different queries against the same
shard and asserts exact received byte totals: the first query fetches the index
and data, and each subsequent query fetches only its data. It also records Range
headers to verify that the index is requested exactly once. When a server ignores
Range, each actual full-object response is charged in full.

The cache unit tests exercise eight concurrent execution-local readers sharing
one index cache: one reader pays for the index, all pay for their own data, and
the cache retains no execution counters. They also cover byte/entry limits, LRU
eviction, pending loads, retries after failed or missing reads, and copying index
slices so a cached index cannot retain a full-shard response. The subprocess
eviction test opens four other datasets and verifies that returning to the first
dataset fetches its index again.

For interactive SQL inspection, use the logging query in [README.md](README.md).
`last_scan()` is a process-local convenience and is not the test oracle.

## Optional Haybarn SQL smoke test

The ignored `haybarn_sql_filters_types_and_diagnostics` test exercises SQL
planning, exact values, a filter-only projection, `COUNT(*)`, and messages in
`duckdb_logs()`. Supply a matching Haybarn binary and VGI extension supporting
`vgi.v2` protocol 2.1:

```sh
HAYBARN_BIN=/path/to/haybarn VGI_EXTENSION=/path/to/vgi.duckdb_extension \
  cargo test --manifest-path crates/vgi-zarr/Cargo.toml --locked \
  --test worker haybarn_sql_filters_types_and_diagnostics -- --ignored
```

A protocol 2.0/2.1 mismatch means the installed VGI extension needs updating.
The ordinary subprocess tests run independently of the DuckDB extension and
are always enabled in CI.
