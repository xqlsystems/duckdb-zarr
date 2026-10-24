use std::collections::HashMap;

use zarrs::storage::{
    byte_range::ByteRangeIterator, Bytes, MaybeBytesIterator, ReadableStorageTraits, StorageError,
    StoreKey,
};

use crate::meta::ZarrStore;

/// Wraps a remote store with an in-memory cache of its consolidated metadata.
///
/// `Array::open` issues up to three GETs per array (a v3 `zarr.json` probe,
/// then `.zarray`, then `.zattrs`), and dim-group inference in `meta.rs` opens
/// every array in the store several times over while classifying coords,
/// bounds vars, and dim groups. For a store with O(100) arrays — not unusual
/// for real-world multi-variable datasets — that is thousands of HTTP round
/// trips before a single byte of chunk data is read. Since consolidated metadata
/// (`.zmetadata` for v2, `consolidated_metadata` in `zarr.json` for v3)
/// already contains every array's metadata document, serving those lookups
/// from memory turns O(arrays) round trips into O(1).
pub struct ConsolidatedCacheStore {
    inner: ZarrStore,
    cache: HashMap<StoreKey, Bytes>,
}

impl ConsolidatedCacheStore {
    pub fn new(inner: ZarrStore, cache: HashMap<StoreKey, Bytes>) -> Self {
        Self { inner, cache }
    }
}

/// Whether `key` names a Zarr metadata document (as opposed to chunk data).
///
/// Consolidated metadata is authoritative over every metadata document in the
/// hierarchy — that is the entire premise of using it to enumerate a remote
/// store instead of listing directories (object stores can't list). So a
/// metadata key that isn't in the cache is not just uncached, it is *absent*:
/// e.g. `Array::open`'s v3 `{name}/zarr.json` probe against a v2-only store
/// (consolidated via `.zmetadata`) can be answered `None` from memory rather
/// than costing a real 404 round trip for every array opened.
fn is_metadata_key(key: &StoreKey) -> bool {
    let basename = key.as_str().rsplit('/').next().unwrap_or(key.as_str());
    matches!(basename, "zarr.json" | ".zarray" | ".zattrs" | ".zgroup")
}

impl ReadableStorageTraits for ConsolidatedCacheStore {
    fn get_partial_many<'a>(
        &'a self,
        key: &StoreKey,
        byte_ranges: ByteRangeIterator<'a>,
    ) -> Result<MaybeBytesIterator<'a>, StorageError> {
        if let Some(data) = self.cache.get(key) {
            let data = data.clone();
            let out = Box::new(byte_ranges.map(move |byte_range| {
                let size = data.len() as u64;
                // Clamp defensively: a byte range computed against a stale or
                // assumed size (or simply an out-of-bounds request) must not
                // panic `Bytes::slice` — return the largest valid slice instead.
                let start = byte_range.start(size).min(size) as usize;
                let end = byte_range.end(size).min(size).max(start as u64) as usize;
                Ok(data.slice(start..end))
            }));
            return Ok(Some(out));
        }
        if is_metadata_key(key) {
            return Ok(None);
        }
        self.inner.get_partial_many(key, byte_ranges)
    }

    fn size_key(&self, key: &StoreKey) -> Result<Option<u64>, StorageError> {
        if let Some(data) = self.cache.get(key) {
            return Ok(Some(data.len() as u64));
        }
        if is_metadata_key(key) {
            return Ok(None);
        }
        self.inner.size_key(key)
    }

    fn supports_get_partial(&self) -> bool {
        self.inner.supports_get_partial()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use zarrs::storage::byte_range::ByteRange;
    use zarrs::storage::store::MemoryStore;
    use zarrs::storage::WritableStorageTraits;

    use super::*;

    fn get(store: &dyn ReadableStorageTraits, key: &str) -> Option<Bytes> {
        store.get(&StoreKey::new(key).unwrap()).unwrap()
    }

    #[test]
    fn is_metadata_key_matches_only_metadata_basenames() {
        for k in [
            "zarr.json",
            ".zarray",
            ".zattrs",
            ".zgroup",
            "foo/.zarray",
            "a/b/.zattrs",
        ] {
            assert!(
                is_metadata_key(&StoreKey::new(k).unwrap()),
                "{k} should be a metadata key"
            );
        }
        // ".zmetadata" is deliberately excluded: it's a root singleton that's
        // always explicitly cached (see meta::build_consolidated_cache), not
        // part of the per-node zarr.json/.zarray/.zattrs/.zgroup pattern.
        for k in ["0.0.0", "foo/0.0.0", ".zmetadata", "foo/c/0.0"] {
            assert!(
                !is_metadata_key(&StoreKey::new(k).unwrap()),
                "{k} should not be a metadata key"
            );
        }
    }

    #[test]
    fn cached_metadata_key_is_served_from_memory() {
        let mut cache = HashMap::new();
        cache.insert(
            StoreKey::new("foo/.zarray").unwrap(),
            Bytes::from_static(b"metadata"),
        );
        let inner: ZarrStore = Arc::new(MemoryStore::new());
        let wrapped = ConsolidatedCacheStore::new(inner, cache);

        assert_eq!(get(&wrapped, "foo/.zarray").unwrap().as_ref(), b"metadata");
    }

    /// A store whose every read panics — used to prove that a lookup never
    /// reaches the inner store, rather than merely observing the right
    /// return value (which a lucky bug could also produce).
    struct PanicStore;

    impl ReadableStorageTraits for PanicStore {
        fn get_partial_many<'a>(
            &'a self,
            _key: &StoreKey,
            _byte_ranges: ByteRangeIterator<'a>,
        ) -> Result<MaybeBytesIterator<'a>, StorageError> {
            panic!("must not reach the inner store for an uncached metadata key");
        }

        fn size_key(&self, _key: &StoreKey) -> Result<Option<u64>, StorageError> {
            panic!("must not reach the inner store for an uncached metadata key");
        }

        fn supports_get_partial(&self) -> bool {
            true
        }
    }

    #[test]
    fn uncached_metadata_key_is_definitively_absent() {
        let inner: ZarrStore = Arc::new(PanicStore);
        let wrapped = ConsolidatedCacheStore::new(inner, HashMap::new());

        // If this fell through to `PanicStore`, the test would panic instead
        // of failing an assertion — the negative-cache path really is O(1).
        assert!(get(&wrapped, "bar/.zarray").is_none());
        assert_eq!(
            wrapped
                .size_key(&StoreKey::new("bar/.zarray").unwrap())
                .unwrap(),
            None
        );
    }

    #[test]
    fn non_metadata_key_delegates_to_inner_store() {
        let inner_store = MemoryStore::new();
        inner_store
            .set(
                &StoreKey::new("foo/0.0.0").unwrap(),
                Bytes::from_static(b"chunk-bytes"),
            )
            .unwrap();
        let inner: ZarrStore = Arc::new(inner_store);
        let wrapped = ConsolidatedCacheStore::new(inner, HashMap::new());

        assert_eq!(get(&wrapped, "foo/0.0.0").unwrap().as_ref(), b"chunk-bytes");
    }

    #[test]
    fn byte_range_past_end_of_cached_document_does_not_panic() {
        let mut cache = HashMap::new();
        cache.insert(
            StoreKey::new("foo/.zattrs").unwrap(),
            Bytes::from_static(b"12345"),
        );
        let inner: ZarrStore = Arc::new(MemoryStore::new());
        let wrapped = ConsolidatedCacheStore::new(inner, cache);

        // Request 100 bytes from a 5-byte cached document.
        let mut ranges = wrapped
            .get_partial_many(
                &StoreKey::new("foo/.zattrs").unwrap(),
                Box::new(std::iter::once(ByteRange::FromStart(0, Some(100)))),
            )
            .unwrap()
            .unwrap();
        let bytes = ranges.next().unwrap().unwrap();
        assert_eq!(bytes.as_ref(), b"12345");
    }

    /// Counts every call that reaches the wrapped inner store, so the O(1)
    /// claim behind this whole module — many array opens should cost zero
    /// inner round trips once consolidated metadata is cached — is a
    /// mechanical assertion instead of a manual timing observation.
    struct CountingStore {
        inner: MemoryStore,
        calls: AtomicUsize,
    }

    impl ReadableStorageTraits for CountingStore {
        fn get_partial_many<'a>(
            &'a self,
            key: &StoreKey,
            byte_ranges: ByteRangeIterator<'a>,
        ) -> Result<MaybeBytesIterator<'a>, StorageError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.inner.get_partial_many(key, byte_ranges)
        }

        fn size_key(&self, key: &StoreKey) -> Result<Option<u64>, StorageError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.inner.size_key(key)
        }

        fn supports_get_partial(&self) -> bool {
            true
        }
    }

    #[test]
    fn opening_many_arrays_costs_zero_inner_round_trips() {
        // Mirrors the real regression: Samudra's OM4.zarr has 110 arrays.
        const N: usize = 200;

        let mut cache = HashMap::new();
        for i in 0..N {
            cache.insert(
                StoreKey::new(format!("var_{i}/.zarray")).unwrap(),
                Bytes::from_static(b"{}"),
            );
            cache.insert(
                StoreKey::new(format!("var_{i}/.zattrs")).unwrap(),
                Bytes::from_static(b"{}"),
            );
        }

        let counting = Arc::new(CountingStore {
            inner: MemoryStore::new(),
            calls: AtomicUsize::new(0),
        });
        let inner: ZarrStore = counting.clone();
        let wrapped = ConsolidatedCacheStore::new(inner, cache);

        for i in 0..N {
            // Mirrors zarrs::Array::open_metadata: a v3 zarr.json probe (which
            // must come back "not found"), then the v2 .zarray and .zattrs.
            assert!(get(&wrapped, &format!("var_{i}/zarr.json")).is_none());
            assert!(get(&wrapped, &format!("var_{i}/.zarray")).is_some());
            assert!(get(&wrapped, &format!("var_{i}/.zattrs")).is_some());
        }

        assert_eq!(
            counting.calls.load(Ordering::Relaxed),
            0,
            "opening {N} arrays through the cache must not touch the inner store at all"
        );
    }
}
