# vgi-zarr

`read_zarr` as a [VGI](https://query.farm/vgi/) worker, built on
[`zarr-core`](../zarr-core). Experimental.

Not a member of the root Cargo workspace: it depends on Query.Farm's VGI SDK,
which is under the Query.Farm Source-Available License.

| | |
|---|---|
| `src/read_zarr.rs` | `read_zarr`, `describe_dims`, `last_scan` table functions; pushdown → chunk pruning |
| `src/http_store.rs` | zarrs store over ranged GETs (sync XHR in the browser, reqwest natively) |
| `src/arrow_values.rs` | lossless conversion of decoded values to Arrow |
| `src/source.rs` | typed source errors and execution-local byte accounting |

Build with Rust 1.97 or newer:

```sh
cargo build --release --locked --manifest-path crates/vgi-zarr/Cargo.toml
```

This worker uses the published VGI 0.44 SDK and requires a VGI extension
that supports `vgi.v2` protocol 2.1. If attach reports a 2.0/2.1 mismatch,
update the VGI extension for your Haybarn version (`FORCE INSTALL vgi FROM
community`). Older cached extension binaries may still speak protocol 2.0. Coordinate pruning calls
`PushdownFilters::may_match` with a batch containing only the known coordinate
column. Other columns are **unknown**, not SQL NULL, so predicates such as
`x = 1 OR measurement IS NOT NULL` cannot discard possible matches.
`FilterHandling::Sdk` assigns exact filtering to VGI, before final projection;
the worker decodes both selected and filter-only columns.

Plain numeric columns preserve their Arrow widths and signedness, including
`UInt64`; strings use `Utf8`. This changes the spike's original schema, which
widened integers to `Int64` and floats to `Float64`. CF-packed integers still
produce `Float64`, and decoded CF time produces `Timestamp(Microsecond)`.
Integer and timestamp values no longer pass through `f64`.

Functions (in the catalog `zarr` once attached):

```sql
ATTACH 'zarr' (TYPE vgi, LOCATION '<path>/target/release/vgi-zarr');  -- browser: 'worker:<url>/vgi-worker-boot.js'
SELECT * FROM zarr.describe_dims('https://data.dynamical.org/noaa/gfs/analysis/latest.zarr');
SELECT time, temperature_2m
FROM zarr.read_zarr('https://data.dynamical.org/noaa/gfs/analysis/latest.zarr')
WHERE time BETWEEN TIMESTAMP '2025-07-01' AND TIMESTAMP '2025-07-31'
  AND latitude = 40.75 AND longitude = -74;
SELECT * FROM zarr.last_scan();  -- optional, process-local demo aid
```

Scan diagnostics arrive through the normal VGI result stream. In Haybarn:

```sql
CALL enable_logging(level := 'debug', storage := 'memory');
-- Run a read_zarr query, then inspect its start/finish messages:
SELECT message FROM duckdb_logs()
WHERE type = 'VGI' AND message LIKE '%source scan%';
```

Rust clients can install a `WorkerLogSink` for structured `scan.started` and
`scan.finished` events with function and execution IDs. Finish counters describe
source work before exact SDK filtering: logical grid rows decoded, chunk tuples
(or subchunk tuples) read/skipped, and actual data-read bytes, including shard
indexes. A tuple counts once even when several data variables are read.
Coordinate-only projections read no data chunks. Metadata/coordinate loading at
bind time is excluded. HTTP byte counts include the full response when a server
ignores Range. Counters and shard decoders belong to one execution. Metadata,
coordinates, and encoded shard indexes are cached across executions in the same
worker process. A warm index costs no source bytes; its data read is charged to
the current execution. Concurrent readers share a pending index fetch, whose
bytes are charged only to the execution performing that fetch. Failed or
cancelled scans may have no finish event.

The shared index cache retains at most 4 MiB and 4096 entries per opened dataset,
evicting the least recently used indexes. It caches known index ranges for
regular, exclusively sharded arrays without storage transformers; other layouts
use the existing uncached reads. Only index bytes are retained, even if an HTTP
server returns the entire shard. Index decoding remains local to each execution.
The cache is discarded with the opened dataset (the worker retains four stores).
As with cached metadata and coordinates, stores are assumed unchanged while
open. Use versioned dataset paths or restart the worker after modifying a store
in place; the cache does not revalidate remote objects on every query.

Only HTTP 404 means a missing chunk and uses Zarr fill semantics. HTTP 403 fails
with `PermissionDenied`; 5xx errors remain failures. Source errors carry typed
status and execution context without including source URLs or credentials.

`last_scan()` is retained for the demo, but a pooled client may route it to a
different process. Use the in-band diagnostics for query statistics.
`describe_dims()` retains its approximate Float64 summaries; it does not expose
exact integer or string coordinate values. These summaries are not used for
pruning.

For additional local debug output, set `VGI_LOG=info VGI_ZARR_LOG=<file>`
(the extension discards worker stderr). Browser debug logs go to the console.

See [testing.md](testing.md) for the subprocess regression suite and examples
using VGI's worker testing kit. CI checks this standalone crate on Linux and
macOS; browser builds belong to the subsequent browser PR.
