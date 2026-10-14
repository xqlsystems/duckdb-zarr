use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde_json::{json, Value};
use tempfile::TempDir;
use tiny_http::{Header, Response, Server, StatusCode};
use vgi_client::{Arguments, BindSpec};
use zarrs::array::{Array, Element};

pub struct Fixture {
    pub dir: TempDir,
    metadata: serde_json::Map<String, Value>,
}

pub fn metadata(dtype: &str, shape: &[u64], chunk: &[u64], dims: &[&str]) -> Value {
    json!({
        "zarr_format": 3, "node_type": "array", "shape": shape,
        "data_type": dtype, "dimension_names": dims,
        "chunk_grid": {"name": "regular", "configuration": {"chunk_shape": chunk}},
        "chunk_key_encoding": {"name": "default", "configuration": {"separator": "/"}},
        "fill_value": match dtype { "string" => json!(""), "bool" => json!(false), _ => json!(0) },
        "codecs": if dtype == "string" { json!([{"name":"vlen-utf8"}]) }
            else { json!([{"name":"bytes", "configuration":{"endian":"little"}}]) },
        "attributes": {}
    })
}

impl Fixture {
    pub fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
            metadata: Default::default(),
        }
    }

    pub fn array<T: Element>(&mut self, name: &str, meta: Value, values: &[T]) {
        let directory = self.dir.path().join(name);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("zarr.json"), meta.to_string()).unwrap();
        let store = Arc::new(zarrs::filesystem::FilesystemStore::new(self.dir.path()).unwrap());
        let array = Array::open(store, &format!("/{name}")).unwrap();
        array
            .store_array_subset(&array.subset_all(), values)
            .unwrap();
        self.metadata.insert(name.into(), meta);
        std::fs::write(self.dir.path().join("zarr.json"), json!({
            "zarr_format":3, "node_type":"group", "attributes":{},
            "consolidated_metadata":{"kind":"inline", "must_understand":false, "metadata":self.metadata}
        }).to_string()).unwrap();
    }

    pub fn basic() -> Self {
        let mut f = Self::new();
        f.array(
            "x",
            metadata("int64", &[5], &[2], &["x"]),
            &[1_i64, 2, 3, 4, 5],
        );
        let mut a = metadata("int64", &[5], &[2], &["x"]);
        a["attributes"] = json!({"_FillValue": -999});
        f.array("a", a, &[10_i64, -999, 30, 40, 50]);
        f
    }

    pub fn spec(&self) -> BindSpec {
        source(self.dir.path().to_str().unwrap())
    }
}

pub fn source(path: &str) -> BindSpec {
    BindSpec::table("read_zarr")
        .in_schema("main")
        .with_arguments(Arguments::new().positional(path))
}

type RecordedRange = (String, Option<String>);

/// A local object server, with deterministic errors and optional Range support.
/// Requests are recorded so pruning tests check actual I/O as well as rows.
pub struct HttpFixture {
    pub url: String,
    pub requests: Arc<Mutex<Vec<String>>>,
    pub ranges: Arc<Mutex<Vec<RecordedRange>>>,
    stopped: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl HttpFixture {
    pub fn new(fixture: &Fixture, errors: HashMap<String, u16>, ignore_ranges: bool) -> Self {
        let server = Server::http("127.0.0.1:0").unwrap();
        let url = format!("http://{}/", server.server_addr());
        let root = fixture.dir.path().to_path_buf();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let ranges = Arc::new(Mutex::new(Vec::new()));
        let captured_ranges = ranges.clone();
        let stopped = Arc::new(AtomicBool::new(false));
        let stop = stopped.clone();
        let thread = thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let Some(request) = server.recv_timeout(Duration::from_millis(20)).unwrap() else {
                    continue;
                };
                let key = request.url().trim_start_matches('/');
                captured.lock().unwrap().push(key.to_string());
                let range = request.headers().iter().find(|h| h.field.equiv("Range"));
                captured_ranges.lock().unwrap().push((
                    key.to_string(),
                    range.map(|header| header.value.as_str().to_string()),
                ));
                if let Some(status) = errors.get(key) {
                    request
                        .respond(Response::empty(StatusCode(*status)))
                        .unwrap();
                    continue;
                }
                let Ok(bytes) = std::fs::read(root.join(key)) else {
                    request.respond(Response::empty(StatusCode(404))).unwrap();
                    continue;
                };
                if let Some(range) = range.filter(|_| !ignore_ranges) {
                    let (start, end) = range
                        .value
                        .as_str()
                        .strip_prefix("bytes=")
                        .unwrap()
                        .split_once('-')
                        .unwrap();
                    let (start, end) = if start.is_empty() {
                        (
                            bytes.len().saturating_sub(end.parse::<usize>().unwrap()),
                            bytes.len(),
                        )
                    } else {
                        (
                            start.parse::<usize>().unwrap(),
                            if end.is_empty() {
                                bytes.len()
                            } else {
                                end.parse::<usize>().unwrap() + 1
                            },
                        )
                    };
                    let header = Header::from_bytes(
                        "Content-Range",
                        format!("bytes {}-{}/{}", start, end - 1, bytes.len()),
                    )
                    .unwrap();
                    request
                        .respond(
                            Response::from_data(bytes[start..end].to_vec())
                                .with_status_code(206)
                                .with_header(header),
                        )
                        .unwrap();
                } else {
                    request.respond(Response::from_data(bytes)).unwrap();
                }
            }
        });
        Self {
            url,
            requests,
            ranges,
            stopped,
            thread: Some(thread),
        }
    }
}

impl Drop for HttpFixture {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
        self.thread.take().unwrap().join().unwrap();
    }
}
