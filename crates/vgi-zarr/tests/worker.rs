mod support;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use arrow_array::*;
use arrow_schema::{DataType, Field, Schema, SchemaRef, TimeUnit};
use arrow_select::filter::filter_record_batch;
use serde_json::json;
use support::{metadata, source, Fixture, HttpFixture};
use vgi_client::testing::{scan_options, Comparison, FilterExpr, ScanResult, TestWorker};
use vgi_client::{Result, RpcError, ScanOptions, WorkerLogMessage};
use vgi_rpc::errors::Code;

fn worker() -> Result<TestWorker> {
    TestWorker::connect_subprocess(&[env!("CARGO_BIN_EXE_vgi-zarr")], "zarr")
}

fn schema(fields: &[(&str, DataType)]) -> SchemaRef {
    Arc::new(Schema::new(
        fields
            .iter()
            .map(|(name, ty)| Field::new(*name, ty.clone(), true))
            .collect::<Vec<_>>(),
    ))
}

fn basic_schema() -> SchemaRef {
    schema(&[("x", DataType::Int64), ("a", DataType::Int64)])
}

fn i64_filter(name: &str, op: Comparison, value: i64) -> FilterExpr {
    FilterExpr::compare(name, op, Arc::new(Int64Array::from(vec![value])))
}

fn i64_at(batch: &RecordBatch, column: usize, row: usize) -> i64 {
    batch
        .column(column)
        .as_any()
        .downcast_ref::<Int64Array>()
        .unwrap()
        .value(row)
}

// This reference does not call VGI's evaluator or pruning code.
fn select(
    full: &ScanResult,
    predicate: impl Fn(&RecordBatch, usize) -> bool,
) -> Result<ScanResult> {
    let batches = full
        .batches()
        .iter()
        .map(|batch| {
            let mask = BooleanArray::from(
                (0..batch.num_rows())
                    .map(|row| predicate(batch, row))
                    .collect::<Vec<_>>(),
            );
            filter_record_batch(batch, &mask).map_err(|e| RpcError::runtime_error(e.to_string()))
        })
        .collect::<Result<Vec<_>>>()?;
    ScanResult::try_new(full.schema().clone(), batches)
}

#[test]
fn nullable_or_retains_rows_with_unknown_data() -> Result<()> {
    let fixture = Fixture::basic();
    let filter = i64_filter("x", Comparison::Eq, 1).or(FilterExpr::is_not_null("a"));
    let options = scan_options(&basic_schema(), &filter, None)?;
    worker()?.check_pushdown(&fixture.spec(), &options, |full| {
        let selected = select(full, |b, r| i64_at(b, 0, r) == 1 || b.column(1).is_valid(r))?;
        assert_eq!(selected.num_rows(), 4);
        Ok(selected)
    })
}

#[test]
fn exact_filters_precede_reordered_and_empty_projection() -> Result<()> {
    let fixture = Fixture::basic();
    for projection in [vec![0], vec![1, 0], vec![]] {
        let filter = i64_filter("a", Comparison::Gt, 20);
        let options = scan_options(&basic_schema(), &filter, Some(projection))?;
        worker()?.check_pushdown(&fixture.spec(), &options, |full| {
            select(full, |b, r| b.column(1).is_valid(r) && i64_at(b, 1, r) > 20)
        })?;
    }
    Ok(())
}

#[test]
fn in_not_and_null_predicates_match_independent_reference() -> Result<()> {
    let fixture = Fixture::basic();
    let cases = [
        (
            FilterExpr::in_values(
                "x",
                Arc::new(Int64Array::from(vec![Some(1), Some(3), None])),
            ),
            vec![1, 3],
        ),
        (
            FilterExpr::not_in_values("x", Arc::new(Int64Array::from(vec![Some(1), None]))),
            vec![],
        ),
        (!i64_filter("x", Comparison::Gt, 2), vec![1, 2]),
        (FilterExpr::is_null("a"), vec![2]),
        (
            i64_filter("x", Comparison::Gt, 2).and(FilterExpr::is_not_null("a")),
            vec![3, 4, 5],
        ),
    ];
    for (filter, ids) in cases {
        let options = scan_options(&basic_schema(), &filter, Some(vec![1]))?;
        worker()?.check_pushdown(&fixture.spec(), &options, |full| {
            select(full, |b, r| ids.contains(&i64_at(b, 0, r)))
        })?;
    }
    Ok(())
}

#[test]
fn pruning_every_chunk_preserves_empty_result_schema() -> Result<()> {
    let fixture = Fixture::basic();
    let options = scan_options(
        &basic_schema(),
        &i64_filter("x", Comparison::Gt, 100),
        Some(vec![1, 0]),
    )?;
    worker()?.check_pushdown(&fixture.spec(), &options, |full| select(full, |_, _| false))
}

#[test]
fn large_integer_coordinates_data_and_literals_are_exact() -> Result<()> {
    let mut f = Fixture::new();
    let ids = vec![
        9_007_199_254_740_993_i64,
        9_007_199_254_740_995,
        -9_007_199_254_740_993,
    ];
    let counts = vec![1_u64 << 63, u64::MAX, 42];
    f.array("x", metadata("int64", &[3], &[1], &["x"]), &ids);
    f.array("a", metadata("int64", &[3], &[1], &["x"]), &ids);
    f.array("count", metadata("uint64", &[3], &[1], &["x"]), &counts);
    let expected_schema = schema(&[
        ("x", DataType::Int64),
        ("a", DataType::Int64),
        ("count", DataType::UInt64),
    ]);
    let expected = ScanResult::try_new(
        expected_schema.clone(),
        vec![RecordBatch::try_new(
            expected_schema.clone(),
            vec![
                Arc::new(Int64Array::from(ids.clone())),
                Arc::new(Int64Array::from(ids.clone())),
                Arc::new(UInt64Array::from(counts)),
            ],
        )
        .unwrap()],
    )?;
    let mut w = worker()?;
    w.scan(&f.spec(), &ScanOptions::default())?
        .assert_matches(&expected)?;
    let filter = i64_filter("x", Comparison::Eq, ids[0]).and(FilterExpr::compare(
        "count",
        Comparison::GtEq,
        Arc::new(UInt64Array::from(vec![1_u64 << 63])),
    ));
    let options = scan_options(&expected_schema, &filter, Some(vec![2, 1]))?;
    w.check_pushdown(&f.spec(), &options, |full| {
        select(full, |b, r| i64_at(b, 0, r) == ids[0])
    })
}

#[test]
fn unsigned_coordinate_null_sentinels_are_not_rounded() -> Result<()> {
    let mut f = Fixture::new();
    let mut x = metadata("uint64", &[3], &[1], &["x"]);
    x["attributes"] = json!({"_FillValue": u64::MAX});
    f.array("x", x, &[1_u64 << 63, u64::MAX, (1_u64 << 63) + 1]);
    f.array(
        "a",
        metadata("int64", &[3], &[1], &["x"]),
        &[10_i64, 20, 30],
    );
    let s = schema(&[("x", DataType::UInt64), ("a", DataType::Int64)]);
    let filter = FilterExpr::compare(
        "x",
        Comparison::Eq,
        Arc::new(UInt64Array::from(vec![(1_u64 << 63) + 1])),
    )
    .or(FilterExpr::is_null("x"));
    let actual = worker()?.scan(&f.spec(), &scan_options(&s, &filter, Some(vec![1]))?)?;
    let projected = Arc::new(s.project(&[1]).unwrap());
    let expected = ScanResult::try_new(
        projected.clone(),
        vec![
            RecordBatch::try_new(projected, vec![Arc::new(Int64Array::from(vec![20, 30]))])
                .unwrap(),
        ],
    )?;
    actual.assert_matches(&expected)
}

#[test]
fn string_coordinates_and_data_round_trip_and_filter() -> Result<()> {
    let mut f = Fixture::new();
    f.array(
        "x",
        metadata("string", &[4], &[2], &["x"]),
        &["one", "two", "three", "four"],
    );
    f.array(
        "label",
        metadata("string", &[4], &[2], &["x"]),
        &["same", "", "same", "🌾"],
    );
    let s = schema(&[("x", DataType::Utf8), ("label", DataType::Utf8)]);
    let expected = ScanResult::try_new(
        s.clone(),
        vec![RecordBatch::try_new(
            s.clone(),
            vec![
                Arc::new(StringArray::from(vec!["one", "two", "three", "four"])),
                Arc::new(StringArray::from(vec!["same", "", "same", "🌾"])),
            ],
        )
        .unwrap()],
    )?;
    let mut w = worker()?;
    w.scan(&f.spec(), &ScanOptions::default())?
        .assert_matches(&expected)?;
    let filter = FilterExpr::in_values("x", Arc::new(StringArray::from(vec!["one", "three"])));
    w.check_pushdown(
        &f.spec(),
        &scan_options(&s, &filter, Some(vec![1]))?,
        |full| {
            select(full, |b, r| {
                let x = b
                    .column(0)
                    .as_any()
                    .downcast_ref::<StringArray>()
                    .unwrap()
                    .value(r);
                x == "one" || x == "three"
            })
        },
    )
}

#[test]
fn float_coordinates_use_exact_typed_bounds() -> Result<()> {
    let mut f = Fixture::new();
    f.array(
        "x",
        metadata("float64", &[5], &[1], &["x"]),
        &[40.5_f64, 40.75, 40.8, 41.0, 41.5],
    );
    f.array(
        "a",
        metadata("int64", &[5], &[1], &["x"]),
        &[1_i64, 2, 3, 4, 5],
    );
    let s = schema(&[("x", DataType::Float64), ("a", DataType::Int64)]);
    let filter = FilterExpr::compare(
        "x",
        Comparison::Gt,
        Arc::new(Float64Array::from(vec![40.75])),
    )
    .and(FilterExpr::compare(
        "x",
        Comparison::Lt,
        Arc::new(Float64Array::from(vec![41.0])),
    ));
    worker()?.check_pushdown(&f.spec(), &scan_options(&s, &filter, None)?, |full| {
        select(full, |b, r| {
            let x = b
                .column(0)
                .as_any()
                .downcast_ref::<Float64Array>()
                .unwrap()
                .value(r);
            x > 40.75 && x < 41.0
        })
    })
}

#[test]
fn timestamp_microseconds_do_not_pass_through_float() -> Result<()> {
    let mut f = Fixture::new();
    let mut x = metadata("int64", &[2], &[1], &["x"]);
    x["attributes"] = json!({"units":"microseconds since 1970-01-01 00:00:00"});
    let timestamps = vec![9_007_199_254_740_993_i64, 9_007_199_254_740_995];
    f.array("x", x, &timestamps);
    f.array("a", metadata("int64", &[2], &[1], &["x"]), &[10_i64, 20]);
    let s = schema(&[
        ("x", DataType::Timestamp(TimeUnit::Microsecond, None)),
        ("a", DataType::Int64),
    ]);
    let expected = ScanResult::try_new(
        s.clone(),
        vec![RecordBatch::try_new(
            s.clone(),
            vec![
                Arc::new(TimestampMicrosecondArray::from(timestamps.clone())),
                Arc::new(Int64Array::from(vec![10, 20])),
            ],
        )
        .unwrap()],
    )?;
    let mut w = worker()?;
    w.scan(&f.spec(), &ScanOptions::default())?
        .assert_matches(&expected)?;
    let filter = FilterExpr::compare(
        "x",
        Comparison::Eq,
        Arc::new(TimestampMicrosecondArray::from(vec![timestamps[0]])),
    );
    w.check_pushdown(&f.spec(), &scan_options(&s, &filter, None)?, |full| {
        select(full, |b, r| i64_at(b, 1, r) == 10)
    })
}

#[test]
fn two_dimensional_or_and_edge_chunks_keep_correct_rows() -> Result<()> {
    let mut f = Fixture::new();
    f.array("x", metadata("int64", &[3], &[2], &["x"]), &[1_i64, 2, 3]);
    f.array(
        "y",
        metadata("int64", &[3], &[2], &["y"]),
        &[10_i64, 20, 30],
    );
    f.array(
        "a",
        metadata("int64", &[3, 3], &[2, 2], &["x", "y"]),
        &(1_i64..=9).collect::<Vec<_>>(),
    );
    let s = schema(&[
        ("x", DataType::Int64),
        ("y", DataType::Int64),
        ("a", DataType::Int64),
    ]);
    let filter = i64_filter("x", Comparison::Eq, 1).or(i64_filter("y", Comparison::Eq, 30));
    worker()?.check_pushdown(
        &f.spec(),
        &scan_options(&s, &filter, Some(vec![2, 0]))?,
        |full| {
            let selected = select(full, |b, r| i64_at(b, 0, r) == 1 || i64_at(b, 1, r) == 30)?;
            assert_eq!(selected.num_rows(), 5);
            Ok(selected)
        },
    )
}

fn extra(message: &WorkerLogMessage, key: &str) -> Option<String> {
    message
        .extras
        .iter()
        .find(|(name, _)| name == key)
        .map(|(_, v)| v.clone())
}

fn capture(w: &mut TestWorker) -> Arc<Mutex<Vec<WorkerLogMessage>>> {
    let logs = Arc::new(Mutex::new(Vec::new()));
    let captured = logs.clone();
    assert!(w
        .client_mut()
        .set_worker_log_sink(Arc::new(move |message| captured
            .lock()
            .unwrap()
            .push(message))));
    logs
}

#[test]
fn http_pruning_avoids_reads_and_reports_execution_scoped_counters() -> Result<()> {
    let fixture = Fixture::basic();
    let http = HttpFixture::new(&fixture, HashMap::new(), false);
    let mut w = worker()?;
    let logs = capture(&mut w);
    let options = scan_options(
        &basic_schema(),
        &i64_filter("x", Comparison::Eq, 3),
        Some(vec![1]),
    )?;
    for _ in 0..2 {
        assert_eq!(w.scan(&source(&http.url), &options)?.num_rows(), 1);
    }
    let requests = http.requests.lock().unwrap();
    assert_eq!(requests.iter().filter(|key| *key == "a/c/1").count(), 2);
    assert!(!requests.iter().any(|key| key == "a/c/0" || key == "a/c/2"));
    let logs = logs.lock().unwrap();
    let starts = logs
        .iter()
        .filter(|m| extra(m, "event").as_deref() == Some("scan.started"))
        .collect::<Vec<_>>();
    let finishes = logs
        .iter()
        .filter(|m| extra(m, "event").as_deref() == Some("scan.finished"))
        .collect::<Vec<_>>();
    assert_eq!((starts.len(), finishes.len()), (2, 2));
    assert_ne!(
        extra(starts[0], "execution_id"),
        extra(starts[1], "execution_id")
    );
    for (start, finish) in starts.iter().zip(finishes) {
        assert_eq!(extra(start, "execution_id"), extra(finish, "execution_id"));
        for (key, value) in [
            ("function", "read_zarr"),
            ("chunks_read", "1"),
            ("chunks_skipped", "2"),
            ("rows_read", "2"),
            ("bytes_read", "16"),
        ] {
            assert_eq!(extra(finish, key).as_deref(), Some(value), "{key}");
        }
    }
    Ok(())
}

#[test]
fn http_permission_denied_and_transient_failures_remain_typed_errors() -> Result<()> {
    let fixture = Fixture::basic();
    for (status, code) in [(403, Code::PermissionDenied), (503, Code::Unavailable)] {
        let http = HttpFixture::new(&fixture, HashMap::from([("a/c/0".into(), status)]), false);
        let url = http.url.replace("http://", "http://user:secret-password@");
        let error = worker()?
            .scan(&source(&url), &ScanOptions::default())
            .unwrap_err();
        assert_eq!(error.code(), code, "{error:?}");
        let info = error.error_info().unwrap();
        assert_eq!(info["http_status"], status.to_string());
        assert_eq!(info["function"], "read_zarr");
        assert!(!info["execution_id"].is_empty());
        assert!(!format!("{error:?}").contains("secret-password"));
    }
    Ok(())
}

#[test]
fn missing_http_chunk_keeps_zarr_fill_semantics() -> Result<()> {
    let fixture = Fixture::basic();
    let http = HttpFixture::new(&fixture, HashMap::from([("a/c/0".into(), 404)]), false);
    let s = basic_schema();
    let expected = ScanResult::try_new(
        s.clone(),
        vec![RecordBatch::try_new(
            s,
            vec![
                Arc::new(Int64Array::from(vec![1, 2, 3, 4, 5])),
                Arc::new(Int64Array::from(vec![0, 0, 30, 40, 50])),
            ],
        )
        .unwrap()],
    )?;
    worker()?
        .scan(&source(&http.url), &ScanOptions::default())?
        .assert_matches(&expected)
}

fn sharded_fixture(index_location: &str, checksum: bool) -> Fixture {
    let mut f = Fixture::new();
    f.array(
        "x",
        metadata("int64", &[4], &[4], &["x"]),
        &[1_i64, 2, 3, 4],
    );
    let mut a = metadata("int64", &[4], &[4], &["x"]);
    let mut index_codecs = vec![json!({"name":"bytes", "configuration":{"endian":"little"}})];
    if checksum {
        index_codecs.push(json!({"name":"crc32c"}));
    }
    a["codecs"] = json!([{"name":"sharding_indexed", "configuration":{
        "chunk_shape":[2], "codecs":[{"name":"bytes", "configuration":{"endian":"little"}}],
        "index_codecs":index_codecs, "index_location":index_location
    }}]);
    f.array("a", a, &[10_i64, 20, 30, 40]);
    f
}

#[test]
fn sharded_queries_reuse_indexes_and_account_for_actual_http_bytes() -> Result<()> {
    for index_location in ["start", "end"] {
        for checksum in [false, true] {
            let f = sharded_fixture(index_location, checksum);
            let index_size = if checksum { 36 } else { 32 };
            let shard_size = index_size + 32;
            let index_header = if index_location == "start" {
                format!("bytes=0-{}", index_size - 1)
            } else {
                format!("bytes=-{index_size}")
            };
            for ignore_ranges in [false, true] {
                let http = HttpFixture::new(&f, HashMap::new(), ignore_ranges);
                let mut w = worker()?;
                let logs = capture(&mut w);
                // Different queries and subchunks, in the same worker process.
                for (query, x) in [3, 1, 3].into_iter().enumerate() {
                    let options =
                        scan_options(&basic_schema(), &i64_filter("x", Comparison::Eq, x), None)?;
                    let expected = ScanResult::try_new(
                        basic_schema(),
                        vec![RecordBatch::try_new(
                            basic_schema(),
                            vec![
                                Arc::new(Int64Array::from(vec![x])),
                                Arc::new(Int64Array::from(vec![x * 10])),
                            ],
                        )
                        .unwrap()],
                    )?;
                    w.scan(&source(&http.url), &options)?
                        .assert_matches(&expected)?;
                    let requests = http.requests.lock().unwrap();
                    assert_eq!(
                        requests.iter().filter(|key| *key == "a/c/0").count(),
                        query + 2
                    );
                    let messages = logs.lock().unwrap();
                    let finishes = messages
                        .iter()
                        .filter(|m| extra(m, "event").as_deref() == Some("scan.finished"))
                        .collect::<Vec<_>>();
                    assert_eq!(finishes.len(), query + 1);
                    let finish = finishes[query];
                    assert_eq!(extra(finish, "chunks_read").as_deref(), Some("1"));
                    assert_eq!(extra(finish, "chunks_skipped").as_deref(), Some("1"));
                    let bytes = if ignore_ranges {
                        shard_size * if query == 0 { 2 } else { 1 }
                    } else {
                        16 + if query == 0 { index_size } else { 0 }
                    };
                    assert_eq!(extra(finish, "bytes_read"), Some(bytes.to_string()));
                    if query > 0 {
                        assert_ne!(
                            extra(finishes[0], "execution_id"),
                            extra(finish, "execution_id")
                        );
                    }
                }
                let ranges = http.ranges.lock().unwrap();
                assert_eq!(
                    ranges
                        .iter()
                        .filter(|(key, range)| {
                            key == "a/c/0" && range.as_deref() == Some(index_header.as_str())
                        })
                        .count(),
                    1
                );
            }
        }
    }
    Ok(())
}

#[test]
fn evicting_opened_dataset_also_discards_its_indexes() -> Result<()> {
    let f = sharded_fixture("end", true);
    let http = HttpFixture::new(&f, HashMap::new(), false);
    let options = scan_options(&basic_schema(), &i64_filter("x", Comparison::Eq, 3), None)?;
    let mut w = worker()?;
    let logs = capture(&mut w);
    w.scan(&source(&http.url), &options)?;
    // The worker retains four opened stores. Keep fixtures alive so every path
    // stays distinct while filling that cache with other datasets.
    let others: Vec<_> = (0..4).map(|_| Fixture::basic()).collect();
    for other in &others {
        w.scan(&other.spec(), &ScanOptions::default())?;
    }
    w.scan(&source(&http.url), &options)?;
    assert_eq!(
        http.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|key| *key == "a/c/0")
            .count(),
        4
    );
    let logs = logs.lock().unwrap();
    let finish = logs
        .iter()
        .rev()
        .find(|m| extra(m, "event").as_deref() == Some("scan.finished"))
        .unwrap();
    assert_eq!(extra(finish, "bytes_read").as_deref(), Some("52"));
    Ok(())
}

#[test]
fn primitive_widths_and_cf_packing_preserve_values_and_nulls() -> Result<()> {
    let mut f = Fixture::new();
    f.array("x", metadata("int64", &[3], &[2], &["x"]), &[1_i64, 2, 3]);
    let mut fields = vec![("x", DataType::Int64)];
    let mut columns: Vec<ArrayRef> = vec![Arc::new(Int64Array::from(vec![1, 2, 3]))];
    macro_rules! column {
        ($name:literal, $dtype:literal, $ty:expr, $array:ty, $low:expr, $high:expr) => {{
            f.array(
                $name,
                metadata($dtype, &[3], &[2], &["x"]),
                &[$low, 0 as _, $high],
            );
            fields.push(($name, $ty));
            columns.push(Arc::new(<$array>::from(vec![
                Some($low),
                Some(0 as _),
                Some($high),
            ])));
        }};
    }
    column!("n01", "int8", DataType::Int8, Int8Array, i8::MIN, i8::MAX);
    column!(
        "n02",
        "int16",
        DataType::Int16,
        Int16Array,
        i16::MIN,
        i16::MAX
    );
    column!(
        "n03",
        "int32",
        DataType::Int32,
        Int32Array,
        i32::MIN,
        i32::MAX
    );
    column!(
        "n04",
        "int64",
        DataType::Int64,
        Int64Array,
        i64::MIN,
        i64::MAX
    );
    column!("n05", "uint8", DataType::UInt8, UInt8Array, 1_u8, u8::MAX);
    column!(
        "n06",
        "uint16",
        DataType::UInt16,
        UInt16Array,
        1_u16,
        u16::MAX
    );
    column!(
        "n07",
        "uint32",
        DataType::UInt32,
        UInt32Array,
        1_u32,
        u32::MAX
    );
    column!(
        "n08",
        "uint64",
        DataType::UInt64,
        UInt64Array,
        1_u64,
        u64::MAX
    );
    column!(
        "n09",
        "float32",
        DataType::Float32,
        Float32Array,
        -1.25_f32,
        1.25_f32
    );
    column!(
        "n10",
        "float64",
        DataType::Float64,
        Float64Array,
        -1.25_f64,
        1.25_f64
    );
    f.array(
        "n11",
        metadata("bool", &[3], &[2], &["x"]),
        &[true, false, true],
    );
    fields.push(("n11", DataType::Boolean));
    columns.push(Arc::new(BooleanArray::from(vec![true, false, true])));
    let mut packed = metadata("int16", &[3], &[2], &["x"]);
    packed["attributes"] = json!({"scale_factor":0.5, "add_offset":10, "_FillValue":-999});
    f.array("n12", packed, &[1_i16, -999, 5]);
    fields.push(("n12", DataType::Float64));
    columns.push(Arc::new(Float64Array::from(vec![
        Some(10.5),
        None,
        Some(12.5),
    ])));
    let s = schema(&fields);
    let expected = ScanResult::try_new(s.clone(), vec![RecordBatch::try_new(s, columns).unwrap()])?;
    worker()?
        .scan(&f.spec(), &ScanOptions::default())?
        .assert_matches(&expected)
}

#[test]
#[ignore = "requires HAYBARN_BIN and VGI_EXTENSION supporting vgi.v2 protocol 2.1"]
fn haybarn_sql_filters_types_and_diagnostics() {
    let binary = std::env::var("HAYBARN_BIN").expect("set HAYBARN_BIN to the Haybarn CLI");
    let extension =
        std::env::var("VGI_EXTENSION").expect("set VGI_EXTENSION to the matching vgi extension");
    let quote = |value: &str| format!("'{}'", value.replace('\'', "''"));
    let run = |query: String| {
        let sql = format!(
            "LOAD {}; ATTACH 'zarr' (TYPE vgi, LOCATION {}); {query}",
            quote(&extension),
            quote(env!("CARGO_BIN_EXE_vgi-zarr")),
        );
        let output = std::process::Command::new(&binary)
            .args(["-unsigned", "-json", "-batch", "-bail", "-c", &sql])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()
    };
    let fixture = Fixture::basic();
    let path = quote(fixture.dir.path().to_str().unwrap());
    assert_eq!(
        run(format!(
            "SELECT x FROM zarr.read_zarr({path}) WHERE x=1 OR a IS NOT NULL ORDER BY x;"
        )),
        json!([{"x":1}, {"x":3}, {"x":4}, {"x":5}])
    );
    assert_eq!(
        run(format!("SELECT x FROM zarr.read_zarr({path}) WHERE a=30;")),
        json!([{"x":3}])
    );
    assert_eq!(
        run(format!(
            "SELECT count(*) AS n FROM zarr.read_zarr({path}) WHERE a>20;"
        )),
        json!([{"n":3}])
    );
    assert_eq!(run(format!("SET logging_storage='memory'; SET logging_level='debug'; SET enable_logging=true;
        CREATE TEMP TABLE results AS SELECT x FROM zarr.read_zarr({path}) WHERE a=30;
        SELECT (SELECT list(x ORDER BY x) FROM results) AS xs,
            (SELECT count(*) FROM duckdb_logs() WHERE type='VGI' AND message LIKE '%source scan finished%') AS finishes;")),
        json!([{"xs":[3], "finishes":1}]));
    let mut exact = Fixture::new();
    exact.array(
        "x",
        metadata("int64", &[2], &[1], &["x"]),
        &[9_007_199_254_740_993_i64, 9_007_199_254_740_995],
    );
    exact.array(
        "a",
        metadata("uint64", &[2], &[1], &["x"]),
        &[u64::MAX, 1_u64 << 63],
    );
    exact.array(
        "label",
        metadata("string", &[2], &[1], &["x"]),
        &["precise", "next"],
    );
    let path = quote(exact.dir.path().to_str().unwrap());
    assert_eq!(run(format!("SELECT x, a::VARCHAR AS a, typeof(a) AS a_type, label FROM zarr.read_zarr({path}) WHERE x=9007199254740993 AND a=18446744073709551615::UBIGINT;")),
        json!([{"x":9_007_199_254_740_993_i64, "a":u64::MAX.to_string(), "a_type":"UBIGINT", "label":"precise"}]));
}
