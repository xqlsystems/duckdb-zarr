//! A read-only zarrs store backed by a kerchunk reference manifest.
//!
//! A kerchunk manifest (the "virtual Zarr" produced by VirtualiZarr,
//! kerchunk, virtual-tiff and friends) is a JSON document mapping Zarr store
//! keys to either inline values (metadata documents, small coordinate
//! arrays) or `[path, offset, length]` byte ranges into existing files:
//! NetCDF4/HDF5, GRIB, GeoTIFF, or real Zarr chunks. Reading one means
//! serving metadata from memory and every chunk key as a range read of the
//! referenced file. See <https://fsspec.github.io/kerchunk/spec.html>.
//!
//! Referenced files are opened through DuckDB's FileSystem API, so `s3://`,
//! `gs://`, `az://`, `https://` and local paths all work and the secrets
//! manager applies, exactly as for [`DuckDbStore`](super::duckdb_store::DuckDbStore).
//! Unlike that store, which opens and closes a file per key because each Zarr
//! chunk is its own object, one source file here backs thousands of chunks,
//! so open handles are pooled and reused across calls.

use std::collections::HashMap;
use std::ffi::CString;
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use zarrs::storage::{
    byte_range::ByteRangeIterator, Bytes, MaybeBytesIterator, ReadableStorageTraits, StorageError,
    StoreKey,
};

use duckdb::ffi::{
    duckdb_create_file_open_options, duckdb_destroy_file_handle, duckdb_destroy_file_open_options,
    duckdb_destroy_file_system, duckdb_file_flag_DUCKDB_FILE_FLAG_READ, duckdb_file_handle,
    duckdb_file_handle_close, duckdb_file_handle_read, duckdb_file_handle_seek,
    duckdb_file_handle_size, duckdb_file_open_options_set_flag, duckdb_file_system,
    duckdb_file_system_open, DuckDBSuccess,
};

/// Upper bound on pooled open handles across all referenced files. A manifest
/// built from one NetCDF file per day for a year references 365 files; the
/// scan touches them in order, so a modest pool keeps the working set open
/// without holding hundreds of remote connections.
const MAX_POOLED_HANDLES: usize = 32;

/// One manifest entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestEntry {
    /// The value is stored in the manifest itself: a metadata document, or a
    /// small chunk (plain text or `base64:`-prefixed).
    Inline(Bytes),
    /// The value is a byte range of a referenced file. `length: None` means
    /// the whole file from `offset`. Paths are shared between entries: one
    /// source file backs thousands of chunks, and a million-reference
    /// manifest must not hold a million copies of the same path.
    Range {
        path: Arc<str>,
        offset: u64,
        length: Option<u64>,
    },
}

/// A kerchunk document as written: version 1 has `refs` (plus optional
/// `templates` and `gen`); version 0 is the bare reference map, which shows
/// up here as `refs: None` and is re-read as a flat map.
#[derive(serde::Deserialize)]
struct ManifestDoc {
    refs: Option<HashMap<String, RefValue>>,
    templates: Option<HashMap<String, String>>,
    gen: Option<Vec<serde_json::Value>>,
}

/// One reference: an inline value, a `[path]` / `[path, offset, length]`
/// list, or something malformed that is reported by key.
///
/// Deserialised by hand: serde's `untagged` enums buffer every value into an
/// intermediate tree before trying the variants, which is exactly the memory
/// cost typed parsing is meant to avoid.
enum RefValue {
    Inline(String),
    Range(RangeRef),
    Malformed(String),
}

struct RangeRef {
    path: String,
    offset: Option<u64>,
    length: Option<u64>,
}

impl<'de> serde::Deserialize<'de> for RefValue {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct RefVisitor;

        impl<'de> serde::de::Visitor<'de> for RefVisitor {
            type Value = RefValue;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a string or a [path, offset, length] list")
            }

            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<RefValue, E> {
                Ok(RefValue::Inline(v.to_string()))
            }

            fn visit_string<E: serde::de::Error>(self, v: String) -> Result<RefValue, E> {
                Ok(RefValue::Inline(v))
            }

            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<RefValue, A::Error> {
                let Some(path) = seq.next_element::<String>().ok().flatten() else {
                    drain(&mut seq)?;
                    return Ok(RefValue::Malformed(
                        "reference must start with a path".into(),
                    ));
                };
                let offset = match seq.next_element::<u64>() {
                    Ok(v) => v,
                    Err(_) => {
                        drain(&mut seq)?;
                        return Ok(RefValue::Malformed(
                            "offset and length must be non-negative integers".into(),
                        ));
                    }
                };
                let length = match seq.next_element::<u64>() {
                    Ok(v) => v,
                    Err(_) => {
                        drain(&mut seq)?;
                        return Ok(RefValue::Malformed(
                            "offset and length must be non-negative integers".into(),
                        ));
                    }
                };
                let mut extra = 0;
                while seq.next_element::<serde::de::IgnoredAny>()?.is_some() {
                    extra += 1;
                }
                match (offset, length, extra) {
                    (None, None, 0) => Ok(RefValue::Range(RangeRef {
                        path,
                        offset: None,
                        length: None,
                    })),
                    (Some(offset), Some(length), 0) => Ok(RefValue::Range(RangeRef {
                        path,
                        offset: Some(offset),
                        length: Some(length),
                    })),
                    _ => {
                        let n = 1 + offset.is_some() as usize + length.is_some() as usize + extra;
                        Ok(RefValue::Malformed(format!(
                            "reference must be [path] or [path, offset, length], got {n} elements"
                        )))
                    }
                }
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<RefValue, A::Error> {
                while map
                    .next_entry::<serde::de::IgnoredAny, serde::de::IgnoredAny>()?
                    .is_some()
                {}
                Ok(RefValue::Malformed("unsupported value (object)".into()))
            }

            fn visit_unit<E: serde::de::Error>(self) -> Result<RefValue, E> {
                Ok(RefValue::Malformed("unsupported value (null)".into()))
            }

            fn visit_bool<E: serde::de::Error>(self, v: bool) -> Result<RefValue, E> {
                Ok(RefValue::Malformed(format!("unsupported value ({v})")))
            }

            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<RefValue, E> {
                Ok(RefValue::Malformed(format!("unsupported value ({v})")))
            }

            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<RefValue, E> {
                Ok(RefValue::Malformed(format!("unsupported value ({v})")))
            }

            fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<RefValue, E> {
                Ok(RefValue::Malformed(format!("unsupported value ({v})")))
            }
        }

        fn drain<'de, A: serde::de::SeqAccess<'de>>(seq: &mut A) -> Result<(), A::Error> {
            while seq.next_element::<serde::de::IgnoredAny>()?.is_some() {}
            Ok(())
        }

        deserializer.deserialize_any(RefVisitor)
    }
}

/// Parse a kerchunk JSON document into store entries.
///
/// Accepts both spec versions: version 0 is a flat `{key: value}` map,
/// version 1 wraps it as `{"version": 1, "refs": {...}}` and may add
/// `templates` (substituted here) and `gen` (rejected: it needs a
/// jinja-style expression evaluator, and VirtualiZarr never emits it).
///
/// Metadata documents are normalised so that zarrs can parse them: kerchunk
/// writers serialise with Python's `json`, which emits bare `NaN` and
/// `Infinity` tokens (for `_FillValue`, most often) that strict JSON parsers
/// reject. Those become the quoted strings the Zarr v2 spec uses for
/// `fill_value`.
///
/// A `.zmetadata` document is synthesised when the manifest lacks one, so the
/// array listing that already works for consolidated remote stores works for
/// manifests unchanged.
pub fn parse_manifest(doc: &[u8]) -> Result<HashMap<StoreKey, ManifestEntry>, String> {
    // Typed deserialisation rather than a `serde_json::Value` tree: a
    // manifest with a million references is tens of megabytes of JSON, and a
    // generic tree costs an order of magnitude more memory than the entries.
    let ManifestDoc {
        refs,
        templates,
        gen,
    } = serde_json::from_slice(doc).map_err(|e| format!("kerchunk manifest is not JSON: {e}"))?;
    let (refs, templates) = match refs {
        Some(refs) => {
            if gen.is_some_and(|gen| !gen.is_empty()) {
                return Err("kerchunk manifest uses `gen` (generated references), \
                     which is not supported; expand it with kerchunk first"
                    .into());
            }
            (refs, templates.unwrap_or_default())
        }
        // Version 0: the document itself is the flat map of references.
        None => {
            let flat: HashMap<String, RefValue> = serde_json::from_slice(doc)
                .map_err(|e| format!("kerchunk manifest is not a reference map: {e}"))?;
            (flat, HashMap::new())
        }
    };

    let mut entries = HashMap::with_capacity(refs.len() + 1);
    let mut consolidated = serde_json::Map::new();
    let mut paths: HashMap<String, Arc<str>> = HashMap::new();
    for (raw_key, value) in refs {
        let key_str = raw_key.trim_start_matches('/');
        if key_str.is_empty() {
            continue;
        }
        let key = StoreKey::new(key_str).map_err(|e| {
            format!("kerchunk manifest key {raw_key:?} is not a valid store key: {e}")
        })?;
        let entry = match value {
            RefValue::Inline(s) => {
                if let Some(encoded) = s.strip_prefix("base64:") {
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(encoded)
                        .map_err(|e| {
                            format!("kerchunk manifest key {raw_key:?}: bad base64: {e}")
                        })?;
                    ManifestEntry::Inline(Bytes::from(bytes))
                } else if is_metadata_key(key_str) {
                    let normalised = normalise_json_specials(&s);
                    let mut parsed: serde_json::Value =
                        serde_json::from_str(&normalised).map_err(|e| {
                            format!("kerchunk manifest key {raw_key:?} is not a JSON document: {e}")
                        })?;
                    let bytes = if key_str.ends_with(".zarray") && normalise_codec_ids(&mut parsed)
                    {
                        serde_json::to_vec(&parsed).map_err(|e| e.to_string())?
                    } else {
                        normalised.into_bytes()
                    };
                    consolidated.insert(key_str.to_string(), parsed);
                    ManifestEntry::Inline(Bytes::from(bytes))
                } else {
                    ManifestEntry::Inline(Bytes::from(s))
                }
            }
            RefValue::Range(RangeRef {
                path,
                offset,
                length,
            }) => {
                let path = substitute_templates(&path, &templates)
                    .map_err(|e| format!("kerchunk manifest key {raw_key:?}: {e}"))?;
                let path = normalise_path(&path);
                let path = match paths.get(&path) {
                    Some(shared) => Arc::clone(shared),
                    None => {
                        let shared: Arc<str> = Arc::from(path.as_str());
                        paths.insert(path, Arc::clone(&shared));
                        shared
                    }
                };
                ManifestEntry::Range {
                    path,
                    offset: offset.unwrap_or(0),
                    length,
                }
            }
            RefValue::Malformed(why) => {
                return Err(format!("kerchunk manifest key {raw_key:?}: {why}"))
            }
        };
        entries.insert(key, entry);
    }

    let zmetadata_key = StoreKey::new(".zmetadata").map_err(|e| e.to_string())?;
    if !entries.contains_key(&zmetadata_key) && !consolidated.is_empty() {
        let doc = serde_json::json!({
            "zarr_consolidated_format": 1,
            "metadata": consolidated,
        });
        entries.insert(
            zmetadata_key,
            ManifestEntry::Inline(Bytes::from(
                serde_json::to_vec(&doc).map_err(|e| e.to_string())?,
            )),
        );
    }

    Ok(entries)
}

/// Whether `key` names a Zarr v2 metadata document.
fn is_metadata_key(key: &str) -> bool {
    let basename = key.rsplit('/').next().unwrap_or(key);
    matches!(basename, ".zarray" | ".zattrs" | ".zgroup" | ".zmetadata")
}

/// Codec ids some manifest writers emit that name a codec zarrs already has
/// under its numcodecs id. virtual-tiff writes `imagecodecs_*` ids for TIFF
/// compressions decoded by the `imagecodecs` package; the byte streams are
/// plain zstd / deflate, so the numcodecs codec decodes them unchanged.
const CODEC_ID_ALIASES: &[(&str, &str)] = &[
    ("imagecodecs_zstd", "zstd"),
    ("imagecodecs_deflate", "zlib"),
    ("imagecodecs_zlib", "zlib"),
    ("imagecodecs_gzip", "gzip"),
];

/// Rewrite aliased codec ids in a `.zarray` document's `filters` and
/// `compressor`. Returns whether anything changed.
fn normalise_codec_ids(zarray: &mut serde_json::Value) -> bool {
    let mut changed = false;
    let mut visit = |codec: &mut serde_json::Value| {
        let Some(id) = codec.get("id").and_then(serde_json::Value::as_str) else {
            return;
        };
        if let Some((_, canonical)) = CODEC_ID_ALIASES.iter().find(|(alias, _)| *alias == id) {
            codec["id"] = serde_json::Value::String((*canonical).to_string());
            changed = true;
        }
    };
    if let Some(filters) = zarray
        .get_mut("filters")
        .and_then(serde_json::Value::as_array_mut)
    {
        filters.iter_mut().for_each(&mut visit);
    }
    if let Some(compressor) = zarray.get_mut("compressor") {
        visit(compressor);
    }
    changed
}

/// Rewrite bare `NaN`, `Infinity` and `-Infinity` tokens (outside of JSON
/// strings) into the quoted forms the Zarr v2 spec uses for `fill_value`.
/// Python's `json.dumps` emits the bare tokens by default and every kerchunk
/// writer goes through it; strict parsers such as `serde_json` reject them.
fn normalise_json_specials(doc: &str) -> String {
    let bytes = doc.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(doc.len() + 8);
    let mut i = 0;
    let mut in_string = false;
    let mut escaped = false;
    while i < bytes.len() {
        let c = bytes[i];
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == b'"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        if c == b'"' {
            in_string = true;
            out.push(c);
            i += 1;
            continue;
        }
        let rest = &bytes[i..];
        let token = [&b"NaN"[..], b"-Infinity", b"Infinity"]
            .into_iter()
            .find(|t| rest.starts_with(t) && !followed_by_ident_char(rest, t.len()));
        if let Some(t) = token {
            out.push(b'"');
            out.extend_from_slice(t);
            out.push(b'"');
            i += t.len();
            continue;
        }
        out.push(c);
        i += 1;
    }
    // Only ASCII bytes were inserted, and every input byte was copied in
    // order, so the output is valid UTF-8 whenever the input was.
    String::from_utf8(out).unwrap_or_else(|_| doc.to_string())
}

fn followed_by_ident_char(rest: &[u8], len: usize) -> bool {
    rest.get(len)
        .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_')
}

/// Substitute `{{name}}` placeholders from a version-1 manifest's `templates`.
fn substitute_templates(path: &str, templates: &HashMap<String, String>) -> Result<String, String> {
    if !path.contains("{{") {
        return Ok(path.to_string());
    }
    let mut out = String::with_capacity(path.len());
    let mut rest = path;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after
            .find("}}")
            .ok_or_else(|| format!("unterminated template placeholder in {path:?}"))?;
        let name = after[..end].trim();
        let value = templates.get(name).ok_or_else(|| {
            format!("template {name:?} in {path:?} is not defined in `templates`")
        })?;
        out.push_str(value);
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Kerchunk writers emit `file://` URLs for local files; DuckDB wants plain paths.
fn normalise_path(path: &str) -> String {
    match path.strip_prefix("file://") {
        Some(rest) => rest.to_string(),
        None => path.to_string(),
    }
}

/// One open source file. The store needs three things from it: its size,
/// positioning, and a `read` that may return fewer bytes than asked for.
/// The DuckDB file handle implements this over FFI; tests implement it over
/// a byte vector so the read loop, pooling and error paths run without a
/// database.
pub trait SourceFile: Send {
    fn size(&mut self) -> u64;
    /// Position the next read at `offset`. `false` means the seek failed.
    fn seek(&mut self, offset: u64) -> bool;
    /// Read up to `buf.len()` bytes. Mirrors the C API: negative is an
    /// error, zero is end of file, positive is the number of bytes read.
    fn read(&mut self, buf: &mut [u8]) -> i64;
}

/// Opens the files a manifest references.
pub trait SourceOpener: Send + Sync {
    /// `Ok(None)` when the file cannot be opened.
    fn open(&self, path: &str) -> Result<Option<Box<dyn SourceFile>>, StorageError>;
}

/// Read exactly `length` bytes at `offset`. DuckDB's HTTP filesystem may
/// return fewer bytes per call than requested, so loop until done, and turn
/// a short file or a failed call into an error naming the offset and count.
fn read_exact_at(
    file: &mut dyn SourceFile,
    offset: u64,
    length: u64,
) -> Result<Bytes, StorageError> {
    if !file.seek(offset) {
        return Err(StorageError::Other(format!(
            "seek to offset {offset} failed"
        )));
    }
    let mut buf = vec![0u8; length as usize];
    let mut filled = 0usize;
    while filled < buf.len() {
        let n = file.read(&mut buf[filled..]);
        if n < 0 {
            return Err(StorageError::Other(format!(
                "read of {length} bytes at offset {offset} failed"
            )));
        }
        if n == 0 {
            return Err(StorageError::Other(format!(
                "read of {length} bytes at offset {offset} hit end of file after {filled} bytes"
            )));
        }
        filled += n as usize;
    }
    Ok(Bytes::from(buf))
}

/// Read an entire file: the manifest document itself, which may live
/// anywhere the opener can reach.
fn read_whole(opener: &dyn SourceOpener, path: &str) -> Result<Bytes, StorageError> {
    let mut file = opener
        .open(path)?
        .ok_or_else(|| StorageError::Other(format!("could not open '{path}'")))?;
    let size = file.size();
    read_exact_at(file.as_mut(), 0, size)
}

/// An open DuckDB file handle that closes itself on drop.
struct DuckDbFile(duckdb_file_handle);

// SAFETY: a handle is only ever used by one thread at a time (it is checked
// out of the pool under a mutex and returned after use), and DuckDB file
// handles are not bound to the thread that opened them.
unsafe impl Send for DuckDbFile {}

impl Drop for DuckDbFile {
    fn drop(&mut self) {
        unsafe {
            duckdb_file_handle_close(self.0);
            duckdb_destroy_file_handle(&mut self.0);
        }
    }
}

impl SourceFile for DuckDbFile {
    fn size(&mut self) -> u64 {
        unsafe { duckdb_file_handle_size(self.0) }.max(0) as u64
    }

    fn seek(&mut self, offset: u64) -> bool {
        unsafe { duckdb_file_handle_seek(self.0, offset as i64) == DuckDBSuccess }
    }

    fn read(&mut self, buf: &mut [u8]) -> i64 {
        unsafe { duckdb_file_handle_read(self.0, buf.as_mut_ptr().cast(), buf.len() as i64) }
    }
}

/// Opens files through DuckDB's FileSystem API. Owns the filesystem handle
/// and destroys it on drop.
pub struct DuckDbOpener {
    file_system: duckdb_file_system,
}

// SAFETY: as for `DuckDbStore`: DuckDB's FileSystem uses internal locking and
// the raw pointer is not mutated after construction.
unsafe impl Send for DuckDbOpener {}
unsafe impl Sync for DuckDbOpener {}

impl DuckDbOpener {
    /// # Safety
    /// `file_system` must be a live handle obtained from a DuckDB client
    /// context. The opener takes ownership of it.
    pub unsafe fn new(file_system: duckdb_file_system) -> Self {
        Self { file_system }
    }
}

impl Drop for DuckDbOpener {
    fn drop(&mut self) {
        if !self.file_system.is_null() {
            unsafe { duckdb_destroy_file_system(&mut self.file_system) }
        }
    }
}

impl SourceOpener for DuckDbOpener {
    fn open(&self, path: &str) -> Result<Option<Box<dyn SourceFile>>, StorageError> {
        let path_cstr = CString::new(path).map_err(|e| StorageError::Other(e.to_string()))?;
        let mut handle: duckdb_file_handle = std::ptr::null_mut();
        let state = unsafe {
            let mut opts = duckdb_create_file_open_options();
            duckdb_file_open_options_set_flag(opts, duckdb_file_flag_DUCKDB_FILE_FLAG_READ, true);
            let state =
                duckdb_file_system_open(self.file_system, path_cstr.as_ptr(), opts, &mut handle);
            duckdb_destroy_file_open_options(&mut opts);
            state
        };
        if state != DuckDBSuccess || handle.is_null() {
            Ok(None)
        } else {
            Ok(Some(Box::new(DuckDbFile(handle))))
        }
    }
}

#[derive(Default)]
struct HandlePool {
    idle: HashMap<String, Vec<Box<dyn SourceFile>>>,
    count: usize,
}

/// A read-only zarrs store that serves a kerchunk manifest.
pub struct ManifestStore {
    /// Pooled handles into referenced files. Declared before `opener` so
    /// they close before the filesystem is destroyed.
    handles: Mutex<HandlePool>,
    entries: HashMap<StoreKey, ManifestEntry>,
    opener: Box<dyn SourceOpener>,
}

impl ManifestStore {
    /// Read and parse the manifest at `manifest_path` through DuckDB's
    /// filesystem. The store takes ownership of `file_system`.
    ///
    /// # Safety
    /// `file_system` must be a live handle obtained from a DuckDB client context.
    pub unsafe fn open(
        file_system: duckdb_file_system,
        manifest_path: &str,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        Self::open_with(Box::new(DuckDbOpener::new(file_system)), manifest_path)
    }

    /// Read and parse the manifest at `manifest_path` through `opener`, which
    /// then serves the referenced files too.
    pub fn open_with(
        opener: Box<dyn SourceOpener>,
        manifest_path: &str,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let doc = read_whole(opener.as_ref(), manifest_path)
            .map_err(|err| format!("could not read kerchunk manifest '{manifest_path}': {err}"))?;
        let entries = parse_manifest(&doc).map_err(|err| format!("'{manifest_path}': {err}"))?;
        Ok(Self::from_entries(opener, entries))
    }

    /// Build a store from already-parsed entries.
    pub fn from_entries(
        opener: Box<dyn SourceOpener>,
        entries: HashMap<StoreKey, ManifestEntry>,
    ) -> Self {
        Self {
            handles: Mutex::new(HandlePool::default()),
            entries,
            opener,
        }
    }

    fn acquire(&self, path: &str) -> Result<Box<dyn SourceFile>, StorageError> {
        {
            let mut pool = self
                .handles
                .lock()
                .map_err(|_| StorageError::Other("handle pool poisoned".into()))?;
            if let Some(handle) = pool.idle.get_mut(path).and_then(Vec::pop) {
                pool.count -= 1;
                return Ok(handle);
            }
        }
        self.opener
            .open(path)?
            .ok_or_else(|| StorageError::Other(format!("could not open referenced file '{path}'")))
    }

    fn release(&self, path: &str, handle: Box<dyn SourceFile>) {
        if let Ok(mut pool) = self.handles.lock() {
            if pool.count < MAX_POOLED_HANDLES {
                pool.idle.entry(path.to_string()).or_default().push(handle);
                pool.count += 1;
                return;
            }
        }
        drop(handle);
    }

    /// Serve byte ranges of an in-memory value, clamped to its length so an
    /// out-of-bounds request cannot panic `Bytes::slice`.
    fn inline_ranges<'a>(
        data: Bytes,
        byte_ranges: ByteRangeIterator<'a>,
    ) -> MaybeBytesIterator<'a> {
        Some(Box::new(byte_ranges.map(move |byte_range| {
            let size = data.len() as u64;
            let start = byte_range.start(size).min(size) as usize;
            let end = byte_range.end(size).min(size).max(start as u64) as usize;
            Ok(data.slice(start..end))
        })))
    }

    fn range_reads<'a>(
        &self,
        path: &str,
        offset: u64,
        length: Option<u64>,
        byte_ranges: ByteRangeIterator<'a>,
    ) -> Result<MaybeBytesIterator<'a>, StorageError> {
        let mut handle = self.acquire(path)?;
        let size = match length {
            Some(len) => len,
            None => handle.size().saturating_sub(offset),
        };
        let mut results: Vec<Result<Bytes, StorageError>> = Vec::new();
        for byte_range in byte_ranges {
            let start = byte_range.start(size).min(size);
            let len = byte_range.length(size).min(size - start);
            results.push(
                read_exact_at(handle.as_mut(), offset + start, len)
                    .map_err(|e| StorageError::Other(format!("referenced file '{path}': {e}"))),
            );
        }
        self.release(path, handle);
        Ok(Some(Box::new(results.into_iter())))
    }
}

impl ReadableStorageTraits for ManifestStore {
    fn get_partial_many<'a>(
        &'a self,
        key: &StoreKey,
        byte_ranges: ByteRangeIterator<'a>,
    ) -> Result<MaybeBytesIterator<'a>, StorageError> {
        match self.entries.get(key) {
            // The manifest is authoritative: a key it does not list does not
            // exist. For a chunk key zarrs then uses the fill value, which is
            // exactly how HDF5 represents never-written chunks.
            None => Ok(None),
            Some(ManifestEntry::Inline(data)) => Ok(Self::inline_ranges(data.clone(), byte_ranges)),
            Some(ManifestEntry::Range {
                path,
                offset,
                length,
            }) => self.range_reads(path, *offset, *length, byte_ranges),
        }
    }

    fn size_key(&self, key: &StoreKey) -> Result<Option<u64>, StorageError> {
        match self.entries.get(key) {
            None => Ok(None),
            Some(ManifestEntry::Inline(data)) => Ok(Some(data.len() as u64)),
            Some(ManifestEntry::Range {
                length: Some(len), ..
            }) => Ok(Some(*len)),
            Some(ManifestEntry::Range {
                path,
                offset,
                length: None,
            }) => {
                let mut handle = self.acquire(path)?;
                let size = handle.size().saturating_sub(*offset);
                self.release(path, handle);
                Ok(Some(size))
            }
        }
    }

    fn supports_get_partial(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use zarrs::storage::byte_range::ByteRange;

    use super::*;

    use std::sync::atomic::{AtomicUsize, Ordering};

    /// In-memory stand-in for DuckDB's filesystem. `max_read` caps the bytes
    /// returned per `read` call to imitate the HTTP filesystem's short reads;
    /// `fail_seek` and `fail_read` force the FFI status codes the real handle
    /// can return.
    #[derive(Default)]
    struct MemoryOpener {
        files: HashMap<String, Vec<u8>>,
        max_read: usize,
        fail_seek: bool,
        fail_read: bool,
        opens: AtomicUsize,
    }

    impl MemoryOpener {
        fn with_file(mut self, path: &str, data: &[u8]) -> Self {
            self.files.insert(path.to_string(), data.to_vec());
            self
        }
    }

    struct MemoryFile {
        data: Vec<u8>,
        pos: usize,
        max_read: usize,
        fail_seek: bool,
        fail_read: bool,
    }

    impl SourceFile for MemoryFile {
        fn size(&mut self) -> u64 {
            self.data.len() as u64
        }

        fn seek(&mut self, offset: u64) -> bool {
            if self.fail_seek {
                return false;
            }
            self.pos = offset as usize;
            true
        }

        fn read(&mut self, buf: &mut [u8]) -> i64 {
            if self.fail_read {
                return -1;
            }
            let available = self.data.len().saturating_sub(self.pos);
            let n = buf.len().min(available);
            let n = if self.max_read > 0 {
                n.min(self.max_read)
            } else {
                n
            };
            buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
            self.pos += n;
            n as i64
        }
    }

    impl SourceOpener for MemoryOpener {
        fn open(&self, path: &str) -> Result<Option<Box<dyn SourceFile>>, StorageError> {
            self.opens.fetch_add(1, Ordering::SeqCst);
            Ok(self.files.get(path).map(|data| {
                Box::new(MemoryFile {
                    data: data.clone(),
                    pos: 0,
                    max_read: self.max_read,
                    fail_seek: self.fail_seek,
                    fail_read: self.fail_read,
                }) as Box<dyn SourceFile>
            }))
        }
    }

    fn opener_with(path: &str, data: &[u8]) -> MemoryOpener {
        MemoryOpener::default().with_file(path, data)
    }

    fn err_text<T>(r: Result<T, StorageError>) -> String {
        match r {
            Ok(_) => panic!("expected an error"),
            Err(e) => e.to_string(),
        }
    }

    fn key(k: &str) -> StoreKey {
        StoreKey::new(k).unwrap()
    }

    fn store(doc: &str) -> ManifestStore {
        ManifestStore::from_entries(
            Box::new(MemoryOpener::default()),
            parse_manifest(doc.as_bytes()).unwrap(),
        )
    }

    #[test]
    fn parses_every_reference_form() {
        let entries = parse_manifest(
            br#"{
                "version": 1,
                "refs": {
                    ".zgroup": "{\"zarr_format\":2}",
                    "t/.zarray": "{\"shape\":[2],\"chunks\":[2],\"dtype\":\"<i8\",\"fill_value\":null,\"order\":\"C\",\"filters\":null,\"compressor\":null,\"zarr_format\":2}",
                    "t/0": "base64:AQAAAAAAAAACAAAAAAAAAA==",
                    "u/0": ["file:///data/a.nc"],
                    "u/1": ["s3://bucket/a.nc", 4290, 1121],
                    "u/2": "plain text"
                }
            }"#,
        )
        .unwrap();
        assert_eq!(
            entries[&key("t/0")],
            ManifestEntry::Inline(Bytes::from_static(&[
                1, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0
            ]))
        );
        assert_eq!(
            entries[&key("u/0")],
            ManifestEntry::Range {
                path: "/data/a.nc".into(),
                offset: 0,
                length: None
            }
        );
        assert_eq!(
            entries[&key("u/1")],
            ManifestEntry::Range {
                path: "s3://bucket/a.nc".into(),
                offset: 4290,
                length: Some(1121)
            }
        );
        assert_eq!(
            entries[&key("u/2")],
            ManifestEntry::Inline(Bytes::from_static(b"plain text"))
        );
        // A .zmetadata document is synthesised from the metadata keys so the
        // consolidated-metadata array listing works unchanged.
        let ManifestEntry::Inline(zmeta) = &entries[&key(".zmetadata")] else {
            panic!(".zmetadata must be inline")
        };
        let zmeta: serde_json::Value = serde_json::from_slice(zmeta).unwrap();
        assert_eq!(zmeta["metadata"]["t/.zarray"]["dtype"], "<i8");
        assert_eq!(zmeta["metadata"][".zgroup"]["zarr_format"], 2);
    }

    #[test]
    fn version_0_flat_map_is_accepted() {
        let entries =
            parse_manifest(br#"{".zgroup": "{\"zarr_format\":2}", "a/0": ["x.nc", 0, 10]}"#)
                .unwrap();
        assert!(entries.contains_key(&key("a/0")));
        assert!(entries.contains_key(&key(".zmetadata")));
    }

    #[test]
    fn existing_zmetadata_is_kept() {
        let entries = parse_manifest(
            br#"{".zmetadata": "{\"zarr_consolidated_format\":1,\"metadata\":{}}", ".zgroup": "{\"zarr_format\":2}"}"#,
        )
        .unwrap();
        let ManifestEntry::Inline(zmeta) = &entries[&key(".zmetadata")] else {
            panic!()
        };
        let zmeta: serde_json::Value = serde_json::from_slice(zmeta).unwrap();
        assert!(zmeta["metadata"].as_object().unwrap().is_empty());
    }

    #[test]
    fn templates_are_substituted() {
        let entries = parse_manifest(
            br#"{"version": 1, "templates": {"u": "s3://bucket/prefix"}, "refs": {"a/0": ["{{u}}/a.nc", 1, 2]}}"#,
        )
        .unwrap();
        assert_eq!(
            entries[&key("a/0")],
            ManifestEntry::Range {
                path: "s3://bucket/prefix/a.nc".into(),
                offset: 1,
                length: Some(2)
            }
        );
    }

    #[test]
    fn gen_is_rejected_with_a_clear_error() {
        let err = parse_manifest(
            br#"{"version": 1, "gen": [{"key": "a/{{i}}", "url": "x", "dimensions": {"i": {"stop": 2}}}], "refs": {}}"#,
        )
        .unwrap_err();
        assert!(err.contains("gen"), "{err}");
    }

    #[test]
    fn malformed_references_are_rejected() {
        for doc in [
            r#"{"a/0": ["x.nc", 1]}"#,
            r#"{"a/0": ["x.nc", "one", 2]}"#,
            r#"{"a/0": 42}"#,
            r#"{"a/0": "base64:!!!"}"#,
            r#"not json"#,
        ] {
            assert!(parse_manifest(doc.as_bytes()).is_err(), "{doc}");
        }
    }

    #[test]
    fn bare_nan_in_metadata_is_normalised() {
        let entries = parse_manifest(
            br#"{"lat/.zattrs": "{\"_FillValue\":NaN,\"_ARRAY_DIMENSIONS\":[\"lat\"],\"note\":\"NaN stays in strings\"}",
                 "lat/.zarray": "{\"fill_value\":-Infinity,\"shape\":[1]}"}"#,
        )
        .unwrap();
        let ManifestEntry::Inline(attrs) = &entries[&key("lat/.zattrs")] else {
            panic!()
        };
        let attrs: serde_json::Value = serde_json::from_slice(attrs).unwrap();
        assert_eq!(attrs["_FillValue"], "NaN");
        assert_eq!(attrs["note"], "NaN stays in strings");
        let ManifestEntry::Inline(zarray) = &entries[&key("lat/.zarray")] else {
            panic!()
        };
        let zarray: serde_json::Value = serde_json::from_slice(zarray).unwrap();
        assert_eq!(zarray["fill_value"], "-Infinity");
    }

    #[test]
    fn imagecodecs_ids_are_aliased_to_numcodecs_ids() {
        let entries = parse_manifest(
            br#"{"0/.zarray": "{\"filters\":[{\"id\":\"imagecodecs_zstd\",\"level\":9}],\"compressor\":{\"id\":\"imagecodecs_deflate\",\"level\":1},\"shape\":[1]}",
                 "1/.zarray": "{\"filters\":null,\"compressor\":{\"id\":\"zlib\",\"level\":1},\"shape\":[1]}"}"#,
        )
        .unwrap();
        let ManifestEntry::Inline(zarray) = &entries[&key("0/.zarray")] else {
            panic!()
        };
        let zarray: serde_json::Value = serde_json::from_slice(zarray).unwrap();
        assert_eq!(zarray["filters"][0]["id"], "zstd");
        assert_eq!(zarray["filters"][0]["level"], 9);
        assert_eq!(zarray["compressor"]["id"], "zlib");
        // The synthesised .zmetadata carries the rewritten ids too.
        let ManifestEntry::Inline(zmeta) = &entries[&key(".zmetadata")] else {
            panic!()
        };
        let zmeta: serde_json::Value = serde_json::from_slice(zmeta).unwrap();
        assert_eq!(zmeta["metadata"]["0/.zarray"]["filters"][0]["id"], "zstd");
        // Untouched documents keep their original bytes.
        let ManifestEntry::Inline(one) = &entries[&key("1/.zarray")] else {
            panic!()
        };
        assert_eq!(
            one.as_ref(),
            br#"{"filters":null,"compressor":{"id":"zlib","level":1},"shape":[1]}"#
        );
    }

    #[test]
    fn normalise_leaves_identifiers_and_escapes_alone() {
        assert_eq!(
            normalise_json_specials(r#"{"NaNny": "a\"NaN\"b", "x": NaN}"#),
            r#"{"NaNny": "a\"NaN\"b", "x": "NaN"}"#
        );
        assert_eq!(normalise_json_specials(r#"{"x": NaNx}"#), r#"{"x": NaNx}"#);
    }

    #[test]
    fn absent_key_is_none() {
        let s = store(r#"{"a/0": "abc"}"#);
        assert!(s.get(&key("a/1")).unwrap().is_none());
        assert_eq!(s.size_key(&key("a/1")).unwrap(), None);
        assert!(s.get(&key("a/zarr.json")).unwrap().is_none());
    }

    #[test]
    fn inline_value_is_served_with_ranges_and_size() {
        let s = store(r#"{"a/0": "base64:AAECAwQ="}"#);
        assert_eq!(
            s.get(&key("a/0")).unwrap().unwrap().as_ref(),
            &[0, 1, 2, 3, 4]
        );
        assert_eq!(s.size_key(&key("a/0")).unwrap(), Some(5));
        let mut ranges = s
            .get_partial_many(
                &key("a/0"),
                Box::new(
                    [
                        ByteRange::FromStart(1, Some(2)),
                        ByteRange::Suffix(2),
                        ByteRange::FromStart(0, Some(100)),
                        ByteRange::FromStart(10, Some(1)),
                    ]
                    .into_iter(),
                ),
            )
            .unwrap()
            .unwrap();
        assert_eq!(ranges.next().unwrap().unwrap().as_ref(), &[1, 2]);
        assert_eq!(ranges.next().unwrap().unwrap().as_ref(), &[3, 4]);
        assert_eq!(ranges.next().unwrap().unwrap().as_ref(), &[0, 1, 2, 3, 4]);
        // Past the end: the largest valid (empty) slice, never a panic.
        assert_eq!(ranges.next().unwrap().unwrap().len(), 0);
    }

    #[test]
    fn range_entry_without_filesystem_errors_instead_of_crashing() {
        let s = store(r#"{"a/0": ["missing.nc", 0, 4]}"#);
        assert_eq!(s.size_key(&key("a/0")).unwrap(), Some(4));
        assert!(s.get(&key("a/0")).is_err());
    }

    // ── source file reads ────────────────────────────────────────────────

    #[test]
    fn read_exact_at_loops_over_short_reads() {
        let opener = MemoryOpener {
            max_read: 3,
            ..opener_with("a.nc", &(0..20u8).collect::<Vec<_>>())
        };
        let mut file = opener.open("a.nc").unwrap().unwrap();
        let got = read_exact_at(file.as_mut(), 2, 7).unwrap();
        assert_eq!(got.as_ref(), &[2, 3, 4, 5, 6, 7, 8]);
        let got = read_exact_at(file.as_mut(), 0, 0).unwrap();
        assert!(got.is_empty());
    }

    #[test]
    fn read_past_end_of_file_names_offset_and_progress() {
        let opener = opener_with("a.nc", &[0u8; 10]);
        let mut file = opener.open("a.nc").unwrap().unwrap();
        let msg = err_text(read_exact_at(file.as_mut(), 5, 8));
        assert!(msg.contains("8 bytes at offset 5"), "{msg}");
        assert!(msg.contains("after 5 bytes"), "{msg}");
    }

    #[test]
    fn failed_seek_and_failed_read_are_errors_not_panics() {
        let opener = MemoryOpener {
            fail_seek: true,
            ..opener_with("a.nc", &[0u8; 10])
        };
        let mut file = opener.open("a.nc").unwrap().unwrap();
        assert!(err_text(read_exact_at(file.as_mut(), 3, 1)).contains("seek to offset 3"));

        let opener = MemoryOpener {
            fail_read: true,
            ..opener_with("a.nc", &[0u8; 10])
        };
        let mut file = opener.open("a.nc").unwrap().unwrap();
        assert!(err_text(read_exact_at(file.as_mut(), 0, 4))
            .contains("read of 4 bytes at offset 0 failed"));
    }

    // ── range entries through the store ──────────────────────────────────

    #[test]
    fn range_entries_read_the_referenced_bytes_and_pool_the_handle() {
        let data: Vec<u8> = (0..100u8).collect();
        let opener = MemoryOpener {
            max_read: 7,
            ..opener_with("a.nc", &data)
        };
        let s = ManifestStore::from_entries(
            Box::new(opener),
            parse_manifest(
                br#"{"v/0": ["a.nc", 10, 20], "v/1": ["a.nc", 30, 20], "w/0": ["a.nc"]}"#,
            )
            .unwrap(),
        );
        assert_eq!(s.get(&key("v/0")).unwrap().unwrap().as_ref(), &data[10..30]);
        assert_eq!(s.get(&key("v/1")).unwrap().unwrap().as_ref(), &data[30..50]);
        assert_eq!(s.size_key(&key("v/0")).unwrap(), Some(20));
        // Whole-file references: size comes from the file, minus the offset.
        assert_eq!(s.get(&key("w/0")).unwrap().unwrap().as_ref(), &data[..]);
        assert_eq!(s.size_key(&key("w/0")).unwrap(), Some(100));

        // Partial ranges are relative to the reference, clamped to its length.
        let mut ranges = s
            .get_partial_many(
                &key("v/1"),
                Box::new(
                    [
                        ByteRange::FromStart(5, Some(3)),
                        ByteRange::Suffix(4),
                        ByteRange::FromStart(15, Some(100)),
                        ByteRange::FromStart(50, Some(1)),
                    ]
                    .into_iter(),
                ),
            )
            .unwrap()
            .unwrap();
        assert_eq!(ranges.next().unwrap().unwrap().as_ref(), &data[35..38]);
        assert_eq!(ranges.next().unwrap().unwrap().as_ref(), &data[46..50]);
        assert_eq!(ranges.next().unwrap().unwrap().as_ref(), &data[45..50]);
        assert_eq!(ranges.next().unwrap().unwrap().len(), 0);
        assert!(ranges.next().is_none());

        // Every read above went through one pooled handle.
        let pool = s.handles.lock().unwrap();
        assert_eq!(pool.count, 1);
        assert_eq!(pool.idle["a.nc"].len(), 1);
    }

    #[test]
    fn pool_holds_at_most_the_cap() {
        let mut opener = MemoryOpener::default();
        let mut refs = String::from("{");
        for i in 0..(MAX_POOLED_HANDLES + 5) {
            opener = opener.with_file(&format!("f{i}.nc"), &[1, 2, 3, 4]);
            refs.push_str(&format!("\"a/{i}\": [\"f{i}.nc\", 0, 4],"));
        }
        refs.pop();
        refs.push('}');
        let s =
            ManifestStore::from_entries(Box::new(opener), parse_manifest(refs.as_bytes()).unwrap());
        for i in 0..(MAX_POOLED_HANDLES + 5) {
            assert_eq!(s.get(&key(&format!("a/{i}"))).unwrap().unwrap().len(), 4);
        }
        assert_eq!(s.handles.lock().unwrap().count, MAX_POOLED_HANDLES);
    }

    #[test]
    fn reference_beyond_end_of_file_is_an_error_naming_the_file() {
        let s = ManifestStore::from_entries(
            Box::new(opener_with("short.nc", &[0u8; 16])),
            parse_manifest(br#"{"a/0": ["short.nc", 8, 16]}"#).unwrap(),
        );
        // The manifest says 16 bytes at 8; the file ends at 16.
        let msg = err_text(s.get(&key("a/0")));
        assert!(msg.contains("short.nc"), "{msg}");
        assert!(msg.contains("hit end of file after 8 bytes"), "{msg}");
        // size_key trusts the manifest; the read is what fails.
        assert_eq!(s.size_key(&key("a/0")).unwrap(), Some(16));
    }

    #[test]
    fn missing_referenced_file_names_the_path() {
        let s = ManifestStore::from_entries(
            Box::new(opener_with("present.nc", &[0u8; 4])),
            parse_manifest(br#"{"a/0": ["absent.nc", 0, 4], "b/0": ["absent.nc"]}"#).unwrap(),
        );
        let msg = err_text(s.get(&key("a/0")));
        assert_eq!(msg, "could not open referenced file 'absent.nc'");
        // Whole-file references need the file even for size_key.
        let msg = err_text(s.size_key(&key("b/0")));
        assert!(msg.contains("absent.nc"), "{msg}");
    }

    // ── opening the manifest itself ──────────────────────────────────────

    #[test]
    fn open_with_reads_the_manifest_through_the_opener() {
        let opener = MemoryOpener {
            max_read: 5,
            ..opener_with("refs.json", br#"{"a/0": ["data.bin", 1, 3], "a/1": "xyz"}"#)
        }
        .with_file("data.bin", b"0123456789");
        let s = ManifestStore::open_with(Box::new(opener), "refs.json").unwrap();
        assert_eq!(s.get(&key("a/0")).unwrap().unwrap().as_ref(), b"123");
        assert_eq!(s.get(&key("a/1")).unwrap().unwrap().as_ref(), b"xyz");
    }

    #[test]
    fn unopenable_or_invalid_manifest_errors_name_the_manifest() {
        let msg = ManifestStore::open_with(Box::new(MemoryOpener::default()), "nope.json")
            .err()
            .unwrap()
            .to_string();
        assert!(
            msg.contains("could not read kerchunk manifest 'nope.json'"),
            "{msg}"
        );

        let msg = ManifestStore::open_with(
            Box::new(opener_with("bad.json", b"\x89HDF\r\n")),
            "bad.json",
        )
        .err()
        .unwrap()
        .to_string();
        assert!(
            msg.starts_with("'bad.json': kerchunk manifest is not JSON"),
            "{msg}"
        );
    }
}
