//! Read-only zarrs store over plain (ranged) HTTP GETs.
//!
//! One store for both builds, so native tests exercise the browser's request
//! pattern: no HEADs (`zarrs_http` HEADs every partial read for a
//! Content-Length, which Cloudflare-fronted buckets such as data.dynamical.org
//! omit), suffix ranges for shard indexes, one request per byte range.
//!
//! In the browser the transport is synchronous XMLHttpRequest
//! (`wasm/zarr_xhr_lib.js`). The worker's serve and rayon threads are
//! emscripten pthreads, i.e. Web Workers, where sync XHR is allowed and blocks
//! only that thread, so concurrency comes from threads: `PARALLEL_UNITS`
//! units decoded on the rayon pool are that many requests in flight.
//! Natively the transport is blocking reqwest.

use crate::source::storage_error;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use vgi::diagnostics::{source_error, source_http_error};
use vgi_rpc::{errors::Code, RpcError};

use zarrs::storage::{
    byte_range::{ByteRange, ByteRangeIterator},
    Bytes, MaybeBytesIterator, ReadableStorageTraits, StorageError, StoreKey,
};

/// GET `url` with an optional `Range` header value → (HTTP status, body).
#[cfg(target_os = "emscripten")]
fn http_get(url: &str, range: Option<&str>) -> Result<(u16, Vec<u8>), RpcError> {
    extern "C" {
        /// Returns the HTTP status (or -1 on a network error); on success
        /// `*out`/`*out_len` hold a malloc'd body that the caller frees.
        fn zarr_xhr_get(
            url: *const u8,
            url_len: usize,
            range: *const u8,
            range_len: usize,
            out: *mut *mut u8,
            out_len: *mut usize,
        ) -> i32;
        fn free(p: *mut std::ffi::c_void);
    }
    let range = range.unwrap_or_default();
    let mut out: *mut u8 = std::ptr::null_mut();
    let mut out_len: usize = 0;
    let status = unsafe {
        zarr_xhr_get(
            url.as_ptr(),
            url.len(),
            range.as_ptr(),
            range.len(),
            &mut out,
            &mut out_len,
        )
    };
    let body = if out.is_null() {
        Vec::new()
    } else {
        let v = unsafe { std::slice::from_raw_parts(out, out_len) }.to_vec();
        unsafe { free(out.cast()) };
        v
    };
    match status {
        s if s > 0 => Ok((s as u16, body)),
        _ => Err(source_error(Code::Unavailable, "GET", "Zarr object")),
    }
}

#[cfg(not(target_os = "emscripten"))]
fn http_get(url: &str, range: Option<&str>) -> Result<(u16, Vec<u8>), RpcError> {
    use std::sync::OnceLock;
    static CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    let client = CLIENT.get_or_init(reqwest::blocking::Client::new);
    let mut req = client.get(url);
    if let Some(r) = range {
        req = req.header(reqwest::header::RANGE, r);
    }
    let resp = req.send().map_err(|e| {
        source_error(
            if e.is_timeout() {
                Code::DeadlineExceeded
            } else {
                Code::Unavailable
            },
            "GET",
            "Zarr object",
        )
    })?;
    let status = resp.status().as_u16();
    let body = resp
        .bytes()
        .map_err(|_| source_error(Code::Unavailable, "read response", "Zarr object"))?;
    Ok((status, body.to_vec()))
}

pub struct HttpStore {
    base: String,
    bytes_read: Arc<AtomicU64>,
}

impl HttpStore {
    pub fn new(base: &str) -> Self {
        Self::with_counter(base, Arc::new(AtomicU64::new(0)))
    }

    pub fn with_counter(base: &str, bytes_read: Arc<AtomicU64>) -> Self {
        Self {
            base: base.trim_end_matches('/').to_string(),
            bytes_read,
        }
    }

    /// Only 404 means an absent object. An ambiguous 403 must fail the scan,
    /// never manufacture fill values in place of unreadable data.
    fn get_range(
        &self,
        key: &StoreKey,
        br: Option<ByteRange>,
    ) -> Result<Option<Bytes>, StorageError> {
        let url = format!("{}/{}", self.base, key.as_str());
        let header = br.map(|br| match br {
            ByteRange::FromStart(off, Some(len)) => format!("bytes={}-{}", off, off + len - 1),
            ByteRange::FromStart(off, None) => format!("bytes={off}-"),
            ByteRange::Suffix(len) => format!("bytes=-{len}"),
        });
        let (status, body) = http_get(&url, header.as_deref()).map_err(storage_error)?;
        self.bytes_read
            .fetch_add(body.len() as u64, Ordering::Relaxed);
        match status {
            206 => Ok(Some(Bytes::from(body))),
            // A server that ignores Range answers 200 with the whole object.
            200 => Ok(Some(match br {
                None => Bytes::from(body),
                Some(br) => {
                    let n = body.len() as u64;
                    let (s, e) = (br.start(n) as usize, br.end(n) as usize);
                    if s > e || e > body.len() {
                        return Err(storage_error(source_error(
                            Code::OutOfRange,
                            "read range",
                            "Zarr object",
                        )));
                    }
                    Bytes::from(body).slice(s..e)
                }
            })),
            404 => Ok(None),
            s => Err(storage_error(source_http_error("GET", "Zarr object", s))),
        }
    }
}

impl ReadableStorageTraits for HttpStore {
    fn get(&self, key: &StoreKey) -> Result<Option<Bytes>, StorageError> {
        self.get_range(key, None)
    }

    fn get_partial_many<'a>(
        &'a self,
        key: &StoreKey,
        byte_ranges: ByteRangeIterator<'a>,
    ) -> Result<MaybeBytesIterator<'a>, StorageError> {
        let mut out = Vec::new();
        for br in byte_ranges {
            if matches!(br, ByteRange::FromStart(_, Some(0)) | ByteRange::Suffix(0)) {
                out.push(Ok(Bytes::new()));
                continue;
            }
            match self.get_range(key, Some(br))? {
                Some(b) => out.push(Ok(b)),
                None => return Ok(None),
            }
        }
        Ok(Some(Box::new(out.into_iter())))
    }

    fn size_key(&self, key: &StoreKey) -> Result<Option<u64>, StorageError> {
        // Rare (sharded arrays with an unknown index location); a full GET is
        // correct where HEAD may lack Content-Length.
        Ok(self.get(key)?.map(|b| b.len() as u64))
    }

    fn supports_get_partial(&self) -> bool {
        true
    }
}
