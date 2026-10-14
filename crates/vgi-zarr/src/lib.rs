//! SPIKE: `read_zarr` served as a VGI worker.
//!
//! The same crate builds a native worker binary (stdio transport, spawned by
//! the `vgi` DuckDB extension) and, on `wasm32-unknown-emscripten`, a browser
//! worker served over Haybarn-wasm's SharedArrayBuffer channel.

mod arrow_values;
mod http_store;
mod read_zarr;
mod shard_index_cache;
mod source;

use vgi::catalog::CatalogModel;

pub fn build() -> vgi::Worker {
    let mut w = vgi::Worker::new();
    w.register_table(read_zarr::ReadZarr);
    w.register_table(read_zarr::LastScan);
    w.register_table(read_zarr::DescribeDims);
    w.set_catalog(CatalogModel {
        name: "zarr".to_string(),
        comment: Some("Zarr stores as DuckDB tables (duckdb-zarr VGI spike)".to_string()),
        ..Default::default()
    });
    w
}

/// Browser only: size rayon's pool up front, from the module's main thread
/// (emscripten pthreads come from a pre-spawned pool; see wasm/build.sh).
#[cfg(target_os = "emscripten")]
fn init_wasm() {
    let _ = rayon::ThreadPoolBuilder::new()
        .num_threads(8)
        .build_global();
    let _ = log::set_logger(&ConsoleLog).map(|()| log::set_max_level(log::LevelFilter::Info));
}

/// stderr is the browser console under emscripten.
#[cfg(target_os = "emscripten")]
struct ConsoleLog;

#[cfg(target_os = "emscripten")]
impl log::Log for ConsoleLog {
    fn enabled(&self, _: &log::Metadata) -> bool {
        true
    }
    fn log(&self, record: &log::Record) {
        if record.target().starts_with("vgi_zarr") {
            eprintln!("{}", record.args());
        }
    }
    fn flush(&self) {}
}

#[cfg(target_os = "emscripten")]
vgi::wasm_worker! { init = init_wasm, build = build }
