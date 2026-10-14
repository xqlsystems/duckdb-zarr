//! `read_zarr(path, dims := 'time,lat,lon', decode_times := true)` as a VGI
//! table function, with filter pushdown turned into chunk pruning.
//!
//! Pruning is conservative (never drops a chunk that could hold a match), and
//! the SDK then applies the pushed filters to every emitted batch
//! (`FilterHandling::Sdk`), before final projection. Pruning is advisory; the
//! SDK owns exact filtering, so the worker retains filter-only columns too.

use std::collections::{HashMap, VecDeque};
use std::error::Error;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex, OnceLock,
};
use std::time::Instant;

use arrow_array::{new_null_array, ArrayRef, Float64Array, Int64Array, RecordBatch};
use arrow_schema::{DataType, Field, Schema, SchemaRef};
use rayon::prelude::*;
use vgi::diagnostics::{ExecutionDiagnostics, ScanStats as SourceStats};
use vgi::function::{ArgSpec, BindParams, BindResponse, FunctionMetadata, ProcessParams};
use vgi::pushdown::PushdownFilters;
use vgi::table_function::{TableCardinality, TableFunction, TableProducer};
use vgi::FilterHandling;
use vgi_rpc::{Result, RpcError};

use crate::shard_index_cache::{IndexCachedStore, IndexRange, ShardIndexCache};
use zarr_core::bounds::chunks_with_kept;
use zarr_core::meta::{
    build_column_defs, build_work_units_in, dimension_names, infer_dim_groups, list_array_names,
    load_coord_array, open_array, ZarrArray, ZarrStore,
};
use zarr_core::types::{
    ColumnDef, ColumnEncoding, CoordArray, FixedValues, SharedColumnValues, StringValues, WorkUnit,
    ZarrDtype,
};
use zarrs::array::{
    ArrayBytes, ArrayShardedExt, ArrayShardedReadableExt, ArrayShardedReadableExtCache,
    CodecOptions,
};

/// Work units decoded concurrently per refill (bounded by memory in the browser:
/// a dynamical.org analysis subchunk is ~14 MB decoded). Each unit is one chunk per data
/// variable, so this is also the number of in-flight chunk fetches.
const PARALLEL_UNITS: usize = 8;
/// Rows per emitted batch (a 721x1440 ERA5 chunk is ~1M rows).
const BATCH_ROWS: usize = 122_880;

fn err(e: impl std::fmt::Display) -> RpcError {
    RpcError::runtime_error(e.to_string())
}

// ---------------------------------------------------------------------------
// Opened store, cached per (path, dims, decode_times) for the life of the
// worker process / wasm module. Bind and producer may run on different calls
// (and natively, in different pooled processes), so nothing is passed between
// them except the call arguments.
// ---------------------------------------------------------------------------

struct Opened {
    dims: Vec<String>,
    shape: Vec<u64>,
    chunk_shape: Vec<u64>,
    columns: Vec<ColumnDef>,
    coord_arrays: HashMap<String, CoordArray>,
    arrays: HashMap<String, ZarrArray>,
    index_ranges: HashMap<String, IndexRange>,
    index_cache: Arc<ShardIndexCache>,
    /// Set when every data variable is sharded with the same subchunk shape:
    /// work units are then subchunks (`chunk_shape` is the subchunk shape) and
    /// each read fetches the shard index once plus one byte range, instead of
    /// a whole shard (a dynamical.org GFS shard is ~440 MB decoded).
    sharded: bool,
    schema: SchemaRef,
}

// SAFETY: zarrs arrays and the coord buffers are immutable after open and are
// only read concurrently, as in the extension's ReadZarrBind.
unsafe impl Send for Opened {}
unsafe impl Sync for Opened {}

type Key = (String, Option<String>, bool);
type OpenCache = Mutex<Vec<(Key, Arc<Opened>)>>;

/// Opened stores kept (least recently used evicted). Each holds metadata,
/// coordinate arrays: small next to decoded chunks, but the
/// browser demo switches between many stores and wasm memory never shrinks.
const OPEN_STORES: usize = 4;
/// Decoded indexes cached per execution/array before reset. Encoded indexes
/// have a separate shared byte budget in `ShardIndexCache`.
const SHARD_INDEXES: usize = 4096;

/// Most recently used last.
fn cache() -> &'static OpenCache {
    static CACHE: OnceLock<OpenCache> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

fn make_store(path: &str) -> std::result::Result<ZarrStore, Box<dyn Error>> {
    if path.starts_with("http://") || path.starts_with("https://") {
        return Ok(zarr_core::meta::with_consolidated_cache(
            path,
            Arc::new(crate::http_store::HttpStore::new(path)),
        ));
    }
    // Local paths (native only). s3/gs need DuckDB's FileSystem, which a
    // worker does not have; their public buckets are reachable over https.
    Ok(Arc::new(zarrs::filesystem::FilesystemStore::new(path)?))
}

fn open(path: &str, dims: Option<&str>, decode_times: bool) -> Result<Arc<Opened>> {
    let key = (path.to_string(), dims.map(str::to_string), decode_times);
    {
        let mut c = cache().lock().unwrap();
        if let Some(i) = c.iter().position(|(k, _)| *k == key) {
            let hit = c.remove(i);
            let o = hit.1.clone();
            c.push(hit);
            return Ok(o);
        }
    }
    let t0 = Instant::now();
    let opened = Arc::new(
        open_uncached(path, dims, decode_times)
            .map_err(|e| crate::source::read_error(e.as_ref()))?,
    );
    log::info!(
        "vgi-zarr: opened {path} in {:?}: dims={:?} shape={:?} read unit={:?}{}",
        t0.elapsed(),
        opened.dims,
        opened.shape,
        opened.chunk_shape,
        if opened.sharded { " (subchunk)" } else { "" }
    );
    let mut c = cache().lock().unwrap();
    c.retain(|(k, _)| *k != key);
    c.push((key, opened.clone()));
    if c.len() > OPEN_STORES {
        let evicted = c.remove(0);
        log::info!("vgi-zarr: evicted {}", evicted.0 .0);
    }
    Ok(opened)
}

fn open_uncached(
    path: &str,
    dims: Option<&str>,
    decode_times: bool,
) -> std::result::Result<Opened, Box<dyn Error>> {
    let store = make_store(path)?;
    let names = list_array_names(path, &store)?;
    let (groups, _) = infer_dim_groups(&store, &names)?;
    let wanted: Option<Vec<String>> =
        dims.map(|d| d.split(',').map(|s| s.trim().to_string()).collect());
    let group = match &wanted {
        Some(w) => groups.iter().find(|g| &g.dims == w).ok_or_else(|| {
            RpcError::value_error(format!(
                "no dimension group matches dims={w:?}; available: {:?}",
                groups.iter().map(|g| &g.dims).collect::<Vec<_>>()
            ))
        })?,
        None if groups.len() == 1 => &groups[0],
        None => {
            return Err(RpcError::value_error(format!(
                "store has {} dimension groups {:?}; pass dims := 'a,b,c'",
                groups.len(),
                groups.iter().map(|g| &g.dims).collect::<Vec<_>>()
            ))
            .into())
        }
    };

    let mut coord_arrays = HashMap::new();
    for coord_name in &group.coord_var_names {
        let ca = load_coord_array(&store, coord_name, decode_times)?;
        let arr = open_array(&store, coord_name)?;
        if let Some(dim) = dimension_names(&arr, coord_name)?.first() {
            coord_arrays.insert(dim.clone(), ca);
        }
    }
    let columns = build_column_defs(&store, group, &coord_arrays, decode_times)?;
    let mut arrays = HashMap::new();
    for col in columns.iter().filter(|c| !c.is_coord) {
        arrays.insert(col.name.clone(), open_array(&store, &col.name)?);
    }
    // SPIKE: belongs in the core's work-unit planning, for the extension too.
    let subchunk_shapes: Vec<Option<Vec<u64>>> = arrays
        .values()
        .map(|a| {
            a.effective_subchunk_shape()
                .map(|s| s.iter().map(|n| n.get()).collect())
        })
        .collect();
    let (chunk_shape, sharded) = match subchunk_shapes.first() {
        Some(Some(first)) if subchunk_shapes.iter().all(|s| s.as_ref() == Some(first)) => {
            (first.clone(), true)
        }
        _ => (group.chunk_shape.clone(), false),
    };
    let schema = Arc::new(Schema::new(
        columns
            .iter()
            .map(|c| Field::new(&c.name, crate::arrow_values::data_type(c), true))
            .collect::<Vec<_>>(),
    ));
    let index_ranges = arrays
        .iter()
        .filter_map(|(name, array)| IndexRange::for_array(array).map(|range| (name.clone(), range)))
        .collect();
    Ok(Opened {
        dims: group.dims.clone(),
        shape: group.shape.clone(),
        chunk_shape,
        columns,
        coord_arrays,
        arrays,
        index_ranges,
        index_cache: Arc::new(ShardIndexCache::default()),
        sharded,
        schema,
    })
}

fn args(a: &vgi::arguments::Arguments) -> Result<(String, Option<String>, bool)> {
    let path = a
        .const_str(0)
        .ok_or_else(|| RpcError::value_error("read_zarr(path): path is required"))?;
    Ok((
        path,
        a.named_str("dims"),
        a.named_bool("decode_times").unwrap_or(true),
    ))
}

// ---------------------------------------------------------------------------
// Table function
// ---------------------------------------------------------------------------

pub struct ReadZarr;

impl TableFunction for ReadZarr {
    fn name(&self) -> &str {
        "read_zarr"
    }

    fn metadata(&self) -> FunctionMetadata {
        FunctionMetadata {
            description: "Read a Zarr store as a table (one row per grid cell)".to_string(),
            projection_pushdown: true,
            ..Default::default()
        }
        .with_filter_handling(FilterHandling::Sdk)
    }

    fn argument_specs(&self) -> Vec<ArgSpec> {
        vec![
            ArgSpec::const_arg("path", 0, "varchar", "Zarr store URL or path"),
            ArgSpec::const_arg("dims", -1, "varchar", "Comma-separated dimension group"),
            ArgSpec::const_arg("decode_times", -1, "boolean", "Decode CF time to TIMESTAMP"),
        ]
    }

    fn on_bind(&self, params: &BindParams) -> Result<BindResponse> {
        let (path, dims, decode_times) = args(&params.arguments)?;
        let o = open(&path, dims.as_deref(), decode_times)?;
        Ok(BindResponse {
            output_schema: o.schema.clone(),
            opaque_data: Vec::new(),
        })
    }

    fn cardinality(&self, params: &BindParams) -> Option<TableCardinality> {
        let (path, dims, decode_times) = args(&params.arguments).ok()?;
        let o = open(&path, dims.as_deref(), decode_times).ok()?;
        let n = o
            .shape
            .iter()
            .try_fold(1_u64, |n, &size| n.checked_mul(size))?;
        let n = i64::try_from(n).ok()?;
        Some(TableCardinality {
            estimate: Some(n),
            max: Some(n),
        })
    }

    fn producer(&self, params: &ProcessParams) -> Result<Box<dyn TableProducer>> {
        let diagnostics = ExecutionDiagnostics::new("read_zarr", &params.execution_id);
        let (path, dims, decode_times) = args(&params.arguments)?;
        let o = open(&path, dims.as_deref(), decode_times)
            .map_err(|e| diagnostics.annotate_error(e))?;

        let mut projected: Vec<bool> = match &params.projection_ids {
            Some(ids) => {
                let mut p = vec![false; o.columns.len()];
                for &i in ids {
                    if let Some(slot) = p.get_mut(i as usize) {
                        *slot = true;
                    }
                }
                p
            }
            None => vec![true; o.columns.len()],
        };

        let filters = params.current_pushdown_filters.as_ref();
        if let Some(filters) = filters {
            let required = filters.filtered_columns();
            for (i, col) in o.columns.iter().enumerate() {
                projected[i] |= required.contains(&col.name);
            }
        }
        let masks = dim_masks(&o, filters).map_err(|e| diagnostics.annotate_error(e))?;
        let chunks_per_dim: Vec<Vec<u64>> = (0..o.dims.len())
            .map(|k| match &masks[k] {
                Some(m) => chunks_with_kept(m, o.shape[k], o.chunk_shape[k]),
                None => (0..o.shape[k].div_ceil(o.chunk_shape[k])).collect(),
            })
            .collect();
        let units = build_work_units_in(&chunks_per_dim);
        let total: u64 = (0..o.dims.len())
            .map(|k| o.shape[k].div_ceil(o.chunk_shape[k]))
            .product();
        log::info!(
            "vgi-zarr: pushed={} projected={:?} chunks {}/{} per-dim={:?}",
            filters
                .map(|f| f.format_pushed())
                .unwrap_or_else(|| "(none)".into()),
            params.projection_ids,
            units.len(),
            total,
            chunks_per_dim.iter().map(Vec::len).collect::<Vec<_>>()
        );
        *LAST_SCAN.lock().unwrap() = Some(ScanStats {
            pushed: filters
                .map(|f| f.format_pushed())
                .unwrap_or_else(|| "(none)".into()),
            chunks_read: units.len() as i64,
            chunks_total: total as i64,
            rows: 0,
            worker_ms: 0.0,
        });

        // Reuse metadata and coordinates, but attach per-execution storage so
        // parallel reads and interleaved scans cannot share byte counters.
        let bytes_read = Arc::new(AtomicU64::new(0));
        let store: ZarrStore = if path.starts_with("http://") || path.starts_with("https://") {
            Arc::new(crate::http_store::HttpStore::with_counter(
                &path,
                bytes_read.clone(),
            ))
        } else {
            Arc::new(crate::source::MeteredStore {
                inner: make_store(&path).map_err(|e| {
                    diagnostics.annotate_error(crate::source::read_error(e.as_ref()))
                })?,
                bytes_read: bytes_read.clone(),
            })
        };
        let arrays: HashMap<String, ZarrArray> = o
            .arrays
            .iter()
            .map(|(name, array)| {
                let reader: ZarrStore = match o.index_ranges.get(name) {
                    Some(&index_range) => Arc::new(IndexCachedStore {
                        inner: store.clone(),
                        cache: o.index_cache.clone(),
                        index_range,
                    }),
                    None => store.clone(),
                };
                (name.clone(), array.with_storage(reader))
            })
            .collect();
        // zarrs caches partial decoders as well as indexes; keep them with this
        // execution's storage rather than retaining another scan's counters.
        // IndexCachedStore reuses encoded indexes without retaining readers.
        let shard_caches = o.sharded.then(|| {
            arrays
                .iter()
                .map(|(name, array)| (name.clone(), ArrayShardedReadableExtCache::new(array)))
                .collect()
        });
        let stats = SourceStats {
            chunks_skipped: total - units.len() as u64,
            ..Default::default()
        };
        Ok(Box::new(Producer {
            diagnostics,
            stats,
            bytes_read,
            arrays,
            shard_caches,
            scan_started: false,
            finished: false,
            o,
            projected,
            masks,
            units,
            next_unit: 0,
            queue: VecDeque::new(),
            started: Instant::now(),
            rows: 0,
        }))
    }
}

/// Each batch contains only the known coordinate column. Absent data/other
/// dimension columns are unknown, not SQL NULL. Only `false` proves exclusion;
/// `true` still requires the SDK's exact filter on real, decoded rows.
fn dim_masks(o: &Opened, filters: Option<&PushdownFilters>) -> Result<Vec<Option<Vec<bool>>>> {
    let Some(filters) = filters else {
        return Ok(vec![None; o.dims.len()]);
    };
    o.dims
        .iter()
        .enumerate()
        .map(|(k, dim)| {
            if !filters.has_filter_for_column(dim) {
                return Ok(None);
            }
            let n = o.shape[k] as usize;
            let values: ArrayRef = match o.coord_arrays.get(dim) {
                Some(ca) => crate::arrow_values::to_array(ca.data.as_ref(), 0..n)?,
                None => Arc::new(Int64Array::from_iter_values(0..n as i64)),
            };
            let field = o.schema.field_with_name(dim).map_err(err)?.clone();
            let known = RecordBatch::try_new(Arc::new(Schema::new(vec![field])), vec![values])
                .map_err(err)?;
            let keep = filters.may_match(&known)?;
            Ok(Some(keep.values().iter().collect()))
        })
        .collect()
}

/// `describe_dims(path, dims := ..)`: one row per dimension with its length,
/// read unit (chunk or subchunk extent) and first/last coordinate (TIMESTAMPs
/// as epoch microseconds). Lets a client write queries that fit the layout.
pub struct DescribeDims;

/// Axes up to this long get their coordinate values in `describe_dims`.
const DESCRIBE_VALUES: usize = 16_384;

impl DescribeDims {
    fn schema() -> SchemaRef {
        Arc::new(Schema::new(vec![
            Field::new("dim", DataType::Utf8, false),
            Field::new("length", DataType::Int64, false),
            Field::new("read_unit", DataType::Int64, false),
            Field::new("kind", DataType::Utf8, false),
            Field::new("first", DataType::Float64, true),
            Field::new("last", DataType::Float64, true),
            Field::new(
                "values",
                DataType::List(Arc::new(Field::new("item", DataType::Float64, true))),
                true,
            ),
        ]))
    }
}

impl TableFunction for DescribeDims {
    fn name(&self) -> &str {
        "describe_dims"
    }
    fn metadata(&self) -> FunctionMetadata {
        FunctionMetadata {
            description: "Dimensions of a read_zarr table: length, read unit, coordinate range"
                .to_string(),
            ..Default::default()
        }
    }
    fn argument_specs(&self) -> Vec<ArgSpec> {
        ReadZarr.argument_specs()
    }
    fn on_bind(&self, _: &BindParams) -> Result<BindResponse> {
        Ok(BindResponse {
            output_schema: Self::schema(),
            opaque_data: Vec::new(),
        })
    }
    fn producer(&self, params: &ProcessParams) -> Result<Box<dyn TableProducer>> {
        let (path, dims, decode_times) = args(&params.arguments)?;
        let o = open(&path, dims.as_deref(), decode_times)?;
        let mut names = Vec::new();
        let (mut len, mut unit, mut kind, mut first, mut last, mut values) = (
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
        );
        for (k, d) in o.dims.iter().enumerate() {
            let n = o.shape[k] as usize;
            let ca = o.coord_arrays.get(d);
            names.push(d.clone());
            len.push(n as i64);
            unit.push(o.chunk_shape[k] as i64);
            kind.push(match ca.map(|c| &c.encoding) {
                Some(ColumnEncoding::CfTime(_)) => "time",
                Some(_) => "number",
                None => "index",
            });
            let at = |i: usize| match ca {
                Some(c) => c.data.key_at(i),
                None => Some(i as f64),
            };
            first.push((n > 0).then(|| at(0)).flatten());
            last.push(n.checked_sub(1).and_then(at));
            // Every coordinate, for axes short enough to ship (lead times,
            // members, irregular grids); long regular axes go by first/last.
            values.push((n <= DESCRIBE_VALUES).then(|| (0..n).map(at).collect::<Vec<_>>()));
        }
        let batch = RecordBatch::try_new(
            Self::schema(),
            vec![
                Arc::new(arrow_array::StringArray::from(names)),
                Arc::new(Int64Array::from(len)),
                Arc::new(Int64Array::from(unit)),
                Arc::new(arrow_array::StringArray::from(kind)),
                Arc::new(Float64Array::from(first)),
                Arc::new(Float64Array::from(last)),
                Arc::new(arrow_array::ListArray::from_iter_primitive::<
                    arrow_array::types::Float64Type,
                    _,
                    _,
                >(values)),
            ],
        )
        .map_err(err)?;
        Ok(Box::new(Once(Some(batch))))
    }
}

/// Stats of the most recent scan in this worker process, for `last_scan()`.
#[derive(Clone)]
struct ScanStats {
    pushed: String,
    chunks_read: i64,
    chunks_total: i64,
    rows: i64,
    worker_ms: f64,
}

static LAST_SCAN: Mutex<Option<ScanStats>> = Mutex::new(None);

/// Browser only: post a finished scan's stats on the `vgi-zarr` BroadcastChannel
/// (`wasm/zarr_xhr_lib.js`). The demo page reads them there instead of calling
/// `last_scan()` after every query: a second RPC right after a large result
/// stream can fail ("response stream EOF") or hang in Haybarn-wasm 1.5.5-rc8's
/// VGI bridge.
fn report_scan(s: &ScanStats) {
    #[cfg(target_os = "emscripten")]
    {
        extern "C" {
            fn zarr_report_scan(json: *const u8, len: usize);
        }
        let json = serde_json::json!({
            "pushed_filters": s.pushed,
            "chunks_read": s.chunks_read,
            "chunks_total": s.chunks_total,
            "rows_emitted": s.rows,
            "worker_ms": s.worker_ms,
        })
        .to_string();
        unsafe { zarr_report_scan(json.as_ptr(), json.len()) };
    }
    #[cfg(not(target_os = "emscripten"))]
    let _ = s;
}

/// `last_scan()`: what the most recent `read_zarr` scan pruned to (demo aid).
/// Per worker process; natively the pool may route calls to another process.
pub struct LastScan;

impl LastScan {
    fn schema() -> SchemaRef {
        Arc::new(Schema::new(vec![
            Field::new("pushed_filters", DataType::Utf8, true),
            Field::new("chunks_read", DataType::Int64, true),
            Field::new("chunks_total", DataType::Int64, true),
            Field::new("rows_emitted", DataType::Int64, true),
            Field::new("worker_ms", DataType::Float64, true),
        ]))
    }
}

impl TableFunction for LastScan {
    fn name(&self) -> &str {
        "last_scan"
    }
    fn metadata(&self) -> FunctionMetadata {
        FunctionMetadata {
            description: "Pruning stats of the most recent read_zarr scan".to_string(),
            stability: Some("VOLATILE".to_string()),
            ..Default::default()
        }
    }
    fn argument_specs(&self) -> Vec<ArgSpec> {
        vec![]
    }
    fn on_bind(&self, _: &BindParams) -> Result<BindResponse> {
        Ok(BindResponse {
            output_schema: Self::schema(),
            opaque_data: Vec::new(),
        })
    }
    fn producer(&self, _: &ProcessParams) -> Result<Box<dyn TableProducer>> {
        let s = LAST_SCAN.lock().unwrap().clone();
        let batch = s
            .map(|s| {
                RecordBatch::try_new(
                    Self::schema(),
                    vec![
                        Arc::new(arrow_array::StringArray::from(vec![s.pushed])),
                        Arc::new(Int64Array::from(vec![s.chunks_read])),
                        Arc::new(Int64Array::from(vec![s.chunks_total])),
                        Arc::new(Int64Array::from(vec![s.rows])),
                        Arc::new(Float64Array::from(vec![s.worker_ms])),
                    ],
                )
            })
            .transpose()
            .map_err(err)?
            .unwrap_or_else(|| RecordBatch::new_empty(Self::schema()));
        Ok(Box::new(Once(Some(batch))))
    }
}

struct Once(Option<RecordBatch>);

impl TableProducer for Once {
    fn next_batch(&mut self, _: &mut vgi_rpc::OutputCollector) -> Result<Option<RecordBatch>> {
        Ok(self.0.take())
    }
    fn resume_supported(&self) -> bool {
        false
    }
}

struct Producer {
    o: Arc<Opened>,
    projected: Vec<bool>,
    masks: Vec<Option<Vec<bool>>>,
    units: Vec<WorkUnit>,
    next_unit: usize,
    queue: VecDeque<RecordBatch>,
    started: Instant,
    rows: usize,
    diagnostics: ExecutionDiagnostics,
    stats: SourceStats,
    bytes_read: Arc<AtomicU64>,
    arrays: HashMap<String, ZarrArray>,
    shard_caches: Option<HashMap<String, ArrayShardedReadableExtCache>>,
    scan_started: bool,
    finished: bool,
}

impl TableProducer for Producer {
    fn next_batch(&mut self, out: &mut vgi_rpc::OutputCollector) -> Result<Option<RecordBatch>> {
        if self.finished {
            return Ok(None);
        }
        if !self.scan_started {
            self.diagnostics.scan_started(out);
            self.scan_started = true;
        }
        while self.queue.is_empty() {
            if self.next_unit >= self.units.len() {
                log::info!(
                    "vgi-zarr: done, {} units, {} rows in {:?}{}",
                    self.units.len(),
                    self.rows,
                    self.started.elapsed(),
                    heap_note()
                );
                if let Some(s) = LAST_SCAN.lock().unwrap().as_mut() {
                    s.rows = self.rows as i64;
                    s.worker_ms = self.started.elapsed().as_secs_f64() * 1e3;
                    report_scan(s);
                }
                self.stats.bytes_read = self.bytes_read.load(Ordering::Relaxed);
                self.diagnostics.scan_finished(out, &self.stats);
                self.finished = true;
                return Ok(None);
            }
            for cache in self.shard_caches.iter().flat_map(HashMap::values) {
                if cache.len() > SHARD_INDEXES {
                    cache.clear();
                }
            }
            let end = (self.next_unit + PARALLEL_UNITS).min(self.units.len());
            let batch = &self.units[self.next_unit..end];
            self.next_unit = end;
            let (o, projected, masks) = (&self.o, &self.projected, &self.masks);
            let t = Instant::now();
            let decoded: Vec<RecordBatch> = batch
                .par_iter()
                .map(|wu| {
                    decode_unit(
                        o,
                        &self.arrays,
                        self.shard_caches.as_ref(),
                        projected,
                        masks,
                        wu,
                    )
                })
                .collect::<std::result::Result<_, _>>()
                .map_err(|e| {
                    self.diagnostics
                        .annotate_error(crate::source::read_error(e.as_ref()))
                })?;
            if o.columns
                .iter()
                .enumerate()
                .any(|(i, col)| projected[i] && !col.is_coord)
            {
                self.stats.chunks_read += batch.len() as u64;
            }
            // Count the logical grid cells decoded before coordinate masks or
            // SDK filters. Edge-chunk padding is not a source row.
            self.stats.rows_read += batch
                .iter()
                .map(|wu| {
                    (0..o.dims.len())
                        .map(|k| {
                            (o.shape[k] - wu.chunk_indices[k] * o.chunk_shape[k])
                                .min(o.chunk_shape[k])
                        })
                        .product::<u64>()
                })
                .sum::<u64>();
            log::debug!(
                "vgi-zarr: refill {} units on {} threads in {:?}",
                batch.len(),
                rayon::current_num_threads(),
                t.elapsed()
            );
            for b in decoded {
                let mut off = 0;
                while off < b.num_rows() {
                    let n = BATCH_ROWS.min(b.num_rows() - off);
                    self.queue.push_back(b.slice(off, n));
                    off += n;
                }
            }
        }
        let b = self.queue.pop_front();
        self.rows += b.as_ref().map_or(0, RecordBatch::num_rows);
        Ok(b)
    }

    fn resume_supported(&self) -> bool {
        false
    }
}

/// Decode one chunk tuple into a batch of the rows its masks keep.
fn decode_unit(
    o: &Opened,
    arrays: &HashMap<String, ZarrArray>,
    shard_caches: Option<&HashMap<String, ArrayShardedReadableExtCache>>,
    projected: &[bool],
    masks: &[Option<Vec<bool>>],
    wu: &WorkUnit,
) -> std::result::Result<RecordBatch, Box<dyn Error + Send + Sync>> {
    let ndim = o.dims.len();
    let origin: Vec<usize> = (0..ndim)
        .map(|k| (wu.chunk_indices[k] * o.chunk_shape[k]) as usize)
        .collect();
    let logical: Vec<usize> = (0..ndim)
        .map(|k| (o.shape[k] as usize - origin[k]).min(o.chunk_shape[k] as usize))
        .collect();
    let mut phys = vec![1usize; ndim];
    for k in (0..ndim.saturating_sub(1)).rev() {
        phys[k] = phys[k + 1] * o.chunk_shape[k + 1] as usize;
    }

    // Kept local indices per dim; rows are their cartesian product in row-major
    // order, so the cost is O(kept rows), not O(chunk size).
    let kept: Vec<Vec<usize>> = (0..ndim)
        .map(|k| {
            (0..logical[k])
                .filter(|&l| masks[k].as_ref().is_none_or(|m| m[origin[k] + l]))
                .collect()
        })
        .collect();
    // (global index per dim, physical offset in the decoded chunk) per kept row.
    let mut rows: Vec<(Vec<usize>, usize)> = Vec::new();
    if kept.iter().all(|v| !v.is_empty()) {
        let mut pos = vec![0usize; ndim];
        'odometer: loop {
            let global = (0..ndim).map(|k| origin[k] + kept[k][pos[k]]).collect();
            let p = (0..ndim).map(|k| kept[k][pos[k]] * phys[k]).sum();
            rows.push((global, p));
            for k in (0..ndim).rev() {
                pos[k] += 1;
                if pos[k] < kept[k].len() {
                    continue 'odometer;
                }
                pos[k] = 0;
            }
            break;
        }
    }
    let t_fetch = Instant::now();
    let mut fetch = std::time::Duration::ZERO;

    let mut cols: Vec<ArrayRef> = Vec::with_capacity(o.columns.len());
    for (i, col) in o.columns.iter().enumerate() {
        let ty = o.schema.field(i).data_type().clone();
        if !projected[i] || rows.is_empty() {
            cols.push(new_null_array(&ty, rows.len()));
            continue;
        }
        if let Some(k) = col.dim_idx {
            match o.coord_arrays.get(&col.name) {
                Some(ca) => cols.push(crate::arrow_values::to_array(
                    ca.data.as_ref(),
                    rows.iter().map(|r| r.0[k]),
                )?),
                None => cols.push(Arc::new(Int64Array::from_iter_values(
                    rows.iter().map(|r| r.0[k] as i64),
                ))),
            }
            continue;
        }
        let arr = &arrays[&col.name];
        if col.on_disk_dtype == ZarrDtype::String {
            let strings = match shard_caches.and_then(|c| c.get(&col.name)) {
                Some(cache) => arr.retrieve_subchunk_opt::<Vec<String>>(
                    cache,
                    &wu.chunk_indices,
                    &CodecOptions::default(),
                ),
                None => arr.retrieve_chunk::<Vec<String>>(&wu.chunk_indices),
            }?;
            cols.push(crate::arrow_values::to_array(
                &StringValues { strings },
                rows.iter().map(|r| r.1),
            )?);
            continue;
        }
        let t = Instant::now();
        let raw = match shard_caches.and_then(|c| c.get(&col.name)) {
            Some(cache) => arr.retrieve_subchunk_opt::<ArrayBytes<'static>>(
                cache,
                &wu.chunk_indices,
                &CodecOptions::default(),
            ),
            None => arr.retrieve_chunk::<ArrayBytes<'static>>(&wu.chunk_indices),
        }?;
        fetch += t.elapsed();
        let bytes = raw
            .into_fixed()
            .map_err(|_| format!("variable-length dtype for '{}'", col.name))?
            .into_owned();
        let values: SharedColumnValues = Arc::new(
            FixedValues::new(
                bytes,
                col.on_disk_dtype.clone(),
                col.encoding.clone(),
                col.sentinel.clone(),
            )
            .ok_or("not fixed width")?,
        );
        cols.push(crate::arrow_values::to_array(
            values.as_ref(),
            rows.iter().map(|r| r.1),
        )?);
    }
    log::debug!(
        "vgi-zarr: unit {:?}: {} rows, fetch+decode {:?}, total {:?}",
        wu.chunk_indices,
        rows.len(),
        fetch,
        t_fetch.elapsed()
    );
    Ok(RecordBatch::try_new(o.schema.clone(), cols)?)
}

/// Browser only: the wasm heap's current size (it grows, never shrinks).
fn heap_note() -> String {
    #[cfg(target_arch = "wasm32")]
    {
        let bytes = core::arch::wasm32::memory_size(0) * 65536;
        return format!(", wasm heap {} MB", bytes >> 20);
    }
    #[allow(unreachable_code)]
    String::new()
}
