//! Encoded shard indexes shared by queries, without retaining their readers.
//!
//! This cache sits above execution-local metered storage: only a cache miss
//! performs I/O and charges that execution. zarrs' decoded-index caches still
//! belong to individual executions because they retain their storage handles.

use std::collections::HashMap;
use std::num::NonZeroU64;
use std::sync::{Arc, Mutex, OnceLock};

use zarr_core::meta::{ZarrArray, ZarrStore};
use zarrs::array::{
    codec::{
        api::ArrayToBytesCodecTraits, CodecChain, ShardingCodecConfigurationV1,
        ShardingIndexLocation,
    },
    data_type, ArrayShardedExt, BytesRepresentation, FillValue,
};
use zarrs::storage::{
    byte_range::{ByteRange, ByteRangeIterator},
    Bytes, MaybeBytesIterator, ReadableStorageTraits, StorageError, StoreKey,
};

/// Limits apply to all arrays in one opened dataset, including pending loads.
const MAX_ENTRIES: usize = 4096;
const MAX_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum IndexRange {
    Start(u64),
    End(u64),
}

impl IndexRange {
    /// Recognize only layouts where the index maps directly to a storage range.
    /// Other layouts continue through zarrs without this optional cache.
    pub fn for_array(array: &ZarrArray) -> Option<Self> {
        if !array.is_exclusively_sharded()
            || !array.storage_transformers().create_metadatas().is_empty()
        {
            return None;
        }
        if array.chunk_grid().name_v3().as_deref() != Some("regular") {
            return None;
        }
        let shard_shape = array.chunk_shape(&vec![0; array.shape().len()]).ok()?;
        let configuration = array
            .codecs()
            .array_to_bytes_codec()
            .configuration_v3(&Default::default())?;
        let config: ShardingCodecConfigurationV1 =
            serde_json::from_value(serde_json::Value::Object(configuration.into())).ok()?;
        if shard_shape.len() != config.chunk_shape.len() {
            return None;
        }
        let mut index_shape = shard_shape
            .iter()
            .zip(config.chunk_shape.iter())
            .map(|(shard, chunk)| {
                if shard.get().is_multiple_of(chunk.get()) {
                    NonZeroU64::new(shard.get() / chunk.get())
                } else {
                    None
                }
            })
            .collect::<Option<Vec<_>>>()?;
        // Each subchunk has an offset and length, both uint64. Ask the index
        // codecs for their encoded size, including checksums such as CRC32C.
        index_shape.push(NonZeroU64::new(2).unwrap());
        let codecs = CodecChain::from_metadata(&config.index_codecs).ok()?;
        let BytesRepresentation::FixedSize(size) = codecs
            .encoded_representation(
                &index_shape,
                &data_type::uint64(),
                &FillValue::from(u64::MAX),
            )
            .ok()?
        else {
            return None;
        };
        match config.index_location {
            ShardingIndexLocation::Start => Some(Self::Start(size)),
            ShardingIndexLocation::End => Some(Self::End(size)),
        }
    }

    fn byte_range(self) -> ByteRange {
        match self {
            Self::Start(size) => ByteRange::FromStart(0, Some(size)),
            Self::End(size) => ByteRange::Suffix(size),
        }
    }

    fn size(self) -> u64 {
        match self {
            Self::Start(size) | Self::End(size) => size,
        }
    }
}

type LoadResult = Result<Option<Bytes>, StorageError>;
type CacheKey = (StoreKey, IndexRange);

struct Entry {
    value: Arc<OnceLock<LoadResult>>,
    last_used: u64,
    reserved_bytes: usize,
}

#[derive(Default)]
struct State {
    entries: HashMap<CacheKey, Entry>,
    bytes: usize,
    clock: u64,
}

pub(crate) struct ShardIndexCache {
    state: Mutex<State>,
    max_entries: usize,
    max_bytes: usize,
}

impl Default for ShardIndexCache {
    fn default() -> Self {
        Self::new(MAX_ENTRIES, MAX_BYTES)
    }
}

impl ShardIndexCache {
    fn new(max_entries: usize, max_bytes: usize) -> Self {
        Self {
            state: Mutex::new(State::default()),
            max_entries,
            max_bytes,
        }
    }

    fn get_or_load(
        &self,
        key: &StoreKey,
        range: IndexRange,
        load: impl FnOnce() -> LoadResult,
    ) -> LoadResult {
        let Ok(size) = usize::try_from(range.size()) else {
            return load();
        };
        if size > self.max_bytes || self.max_entries == 0 {
            return load();
        }
        let key = (key.clone(), range);
        let value = {
            let mut state = self.state.lock().unwrap();
            state.clock = state.clock.saturating_add(1);
            let last_used = state.clock;
            if let Some(entry) = state.entries.get_mut(&key) {
                entry.last_used = last_used;
                entry.value.clone()
            } else {
                while state.entries.len() >= self.max_entries || state.bytes > self.max_bytes - size
                {
                    // Never evict a pending load: other queries can join it.
                    let oldest = state
                        .entries
                        .iter()
                        .filter(|(_, entry)| entry.value.get().is_some())
                        .min_by_key(|(_, entry)| entry.last_used)
                        .map(|(key, _)| key.clone());
                    let Some(oldest) = oldest else {
                        // All capacity is reserved by other pending indexes.
                        // Bypass caching instead of blocking unrelated reads.
                        drop(state);
                        return load();
                    };
                    state.bytes -= state.entries.remove(&oldest).unwrap().reserved_bytes;
                }
                let value = Arc::new(OnceLock::new());
                state.entries.insert(
                    key.clone(),
                    Entry {
                        value: value.clone(),
                        last_used,
                        reserved_bytes: size,
                    },
                );
                state.bytes += size;
                value
            }
        };
        // No cache-wide lock during I/O. Exactly one caller initializes this
        // entry, using its own metered reader; waiters incur no source bytes.
        let result = value.get_or_init(|| {
            load().map(|bytes| {
                bytes.map(|bytes| {
                    if bytes.len() == size {
                        // HTTP 200 fallback returns a slice of the full shard.
                        // Copy the index so the cache cannot retain that body.
                        Bytes::copy_from_slice(&bytes)
                    } else {
                        bytes
                    }
                })
            })
        });
        if !matches!(result, Ok(Some(bytes)) if bytes.len() == size) {
            // Do not persist failures, absent objects or malformed responses.
            let mut state = self.state.lock().unwrap();
            if state
                .entries
                .get(&key)
                .is_some_and(|entry| Arc::ptr_eq(&entry.value, &value))
            {
                state.bytes -= state.entries.remove(&key).unwrap().reserved_bytes;
            }
        }
        result.clone()
    }
}

/// One array's reader for one execution, sharing only immutable index bytes.
pub(crate) struct IndexCachedStore {
    pub inner: ZarrStore,
    pub cache: Arc<ShardIndexCache>,
    pub index_range: IndexRange,
}

impl ReadableStorageTraits for IndexCachedStore {
    fn get(&self, key: &StoreKey) -> LoadResult {
        self.inner.get(key)
    }

    fn get_partial_many<'a>(
        &'a self,
        key: &StoreKey,
        ranges: ByteRangeIterator<'a>,
    ) -> Result<MaybeBytesIterator<'a>, StorageError> {
        let ranges: Vec<_> = ranges.collect();
        if !ranges.contains(&self.index_range.byte_range()) {
            return self
                .inner
                .get_partial_many(key, Box::new(ranges.into_iter()));
        }
        let mut parts = Vec::with_capacity(ranges.len());
        for range in ranges {
            let load = || self.inner.get_partial(key, range);
            let bytes = if range == self.index_range.byte_range() {
                self.cache.get_or_load(key, self.index_range, load)?
            } else {
                load()?
            };
            let Some(bytes) = bytes else {
                return Ok(None);
            };
            parts.push(Ok(bytes));
        }
        Ok(Some(Box::new(parts.into_iter())))
    }

    fn size_key(&self, key: &StoreKey) -> Result<Option<u64>, StorageError> {
        self.inner.size_key(key)
    }

    fn supports_get_partial(&self) -> bool {
        self.inner.supports_get_partial()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::MeteredStore;
    use std::sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Barrier,
    };
    use std::time::Duration;

    struct CountingStore {
        body: Bytes,
        reads: AtomicUsize,
    }

    impl ReadableStorageTraits for CountingStore {
        fn get(&self, _: &StoreKey) -> LoadResult {
            self.reads.fetch_add(1, Ordering::Relaxed);
            Ok(Some(self.body.clone()))
        }

        fn get_partial_many<'a>(
            &'a self,
            _: &StoreKey,
            ranges: ByteRangeIterator<'a>,
        ) -> Result<MaybeBytesIterator<'a>, StorageError> {
            let parts: Vec<_> = ranges
                .map(|range| {
                    self.reads.fetch_add(1, Ordering::Relaxed);
                    // Let other readers reach the shared entry while it loads.
                    std::thread::sleep(Duration::from_millis(20));
                    let n = self.body.len() as u64;
                    Ok(self
                        .body
                        .slice(range.start(n) as usize..range.end(n) as usize))
                })
                .collect();
            Ok(Some(Box::new(parts.into_iter())))
        }

        fn size_key(&self, _: &StoreKey) -> Result<Option<u64>, StorageError> {
            Ok(Some(self.body.len() as u64))
        }

        fn supports_get_partial(&self) -> bool {
            true
        }
    }

    #[test]
    fn concurrent_readers_share_index_fetch_but_keep_independent_counters() {
        let source = Arc::new(CountingStore {
            body: Bytes::from(vec![7; 68]),
            reads: AtomicUsize::new(0),
        });
        let cache = Arc::new(ShardIndexCache::default());
        let start = Arc::new(Barrier::new(8));
        let key = StoreKey::new("a/c/0").unwrap();
        let counters = std::thread::scope(|scope| {
            let threads: Vec<_> = (0..8)
                .map(|_| {
                    let (cache, start, source, key) =
                        (cache.clone(), start.clone(), source.clone(), key.clone());
                    scope.spawn(move || {
                        let counter = Arc::new(AtomicU64::new(0));
                        let store = IndexCachedStore {
                            inner: Arc::new(MeteredStore {
                                inner: source,
                                bytes_read: counter.clone(),
                            }),
                            cache,
                            index_range: IndexRange::End(36),
                        };
                        start.wait();
                        assert_eq!(
                            store
                                .get_partial(&key, ByteRange::Suffix(36))
                                .unwrap()
                                .unwrap()
                                .len(),
                            36
                        );
                        assert_eq!(
                            store
                                .get_partial(&key, ByteRange::FromStart(0, Some(16)))
                                .unwrap()
                                .unwrap()
                                .len(),
                            16
                        );
                        drop(store);
                        // Shared cache entries must not retain any query counter.
                        assert_eq!(Arc::strong_count(&counter), 1);
                        counter.load(Ordering::Relaxed)
                    })
                })
                .collect();
            threads
                .into_iter()
                .map(|thread| thread.join().unwrap())
                .collect::<Vec<_>>()
        });
        let mut counters = counters;
        counters.sort_unstable();
        assert_eq!(counters, vec![16, 16, 16, 16, 16, 16, 16, 52]);
        assert_eq!(source.reads.load(Ordering::Relaxed), 9);
    }

    #[test]
    fn cache_obeys_byte_and_entry_limits_and_evicts_least_recently_used() {
        // Independently exercise the byte budget and the entry budget.
        for (entries, bytes) in [(10, 8), (2, 100)] {
            let cache = ShardIndexCache::new(entries, bytes);
            let loads = AtomicUsize::new(0);
            let read = |key: &str| {
                cache
                    .get_or_load(&StoreKey::new(key).unwrap(), IndexRange::End(4), || {
                        loads.fetch_add(1, Ordering::Relaxed);
                        Ok(Some(Bytes::from_static(b"1234")))
                    })
                    .unwrap()
                    .unwrap()
            };
            read("a");
            read("b");
            read("a"); // a is most recently used
            read("c"); // evicts b
            read("a"); // still cached
            assert_eq!(loads.load(Ordering::Relaxed), 3);
            read("b");
            assert_eq!(loads.load(Ordering::Relaxed), 4);
            let state = cache.state.lock().unwrap();
            assert_eq!(state.entries.len(), 2);
            assert_eq!(state.bytes, 8);
        }
    }

    #[test]
    fn oversized_indexes_bypass_cache() {
        let cache = ShardIndexCache::new(2, 3);
        let loads = AtomicUsize::new(0);
        for _ in 0..2 {
            cache
                .get_or_load(&StoreKey::new("a").unwrap(), IndexRange::End(4), || {
                    loads.fetch_add(1, Ordering::Relaxed);
                    Ok(Some(Bytes::from_static(b"1234")))
                })
                .unwrap();
        }
        assert_eq!(loads.load(Ordering::Relaxed), 2);
        assert!(cache.state.lock().unwrap().entries.is_empty());
    }

    #[test]
    fn pending_index_does_not_block_unrelated_reads_or_exceed_budget() {
        let cache = ShardIndexCache::new(1, 4);
        let (entered, started) = std::sync::mpsc::channel();
        let (release, wait) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            let loading = scope.spawn(|| {
                cache
                    .get_or_load(
                        &StoreKey::new("a").unwrap(),
                        IndexRange::End(4),
                        move || {
                            entered.send(()).unwrap();
                            wait.recv_timeout(Duration::from_secs(5)).unwrap();
                            Ok(Some(Bytes::from_static(b"aaaa")))
                        },
                    )
                    .unwrap()
            });
            started.recv_timeout(Duration::from_secs(5)).unwrap();
            let other = cache
                .get_or_load(&StoreKey::new("b").unwrap(), IndexRange::End(4), || {
                    Ok(Some(Bytes::from_static(b"bbbb")))
                })
                .unwrap()
                .unwrap();
            assert_eq!(&other[..], b"bbbb");
            {
                let state = cache.state.lock().unwrap();
                assert_eq!(state.entries.len(), 1);
                assert_eq!(state.bytes, 4);
            }
            release.send(()).unwrap();
            assert_eq!(&loading.join().unwrap().unwrap()[..], b"aaaa");
        });
    }

    #[test]
    fn failed_missing_and_short_reads_are_retried() {
        for failure in [
            Err(std::io::Error::other("temporary failure").into()),
            Ok(None),
            Ok(Some(Bytes::from_static(b"12"))),
        ] {
            let cache = ShardIndexCache::default();
            let key = StoreKey::new("a").unwrap();
            let _ = cache.get_or_load(&key, IndexRange::End(4), || failure);
            assert!(cache.state.lock().unwrap().entries.is_empty());
            assert_eq!(cache.state.lock().unwrap().bytes, 0);
            let bytes = cache
                .get_or_load(&key, IndexRange::End(4), || {
                    Ok(Some(Bytes::from_static(b"1234")))
                })
                .unwrap()
                .unwrap();
            assert_eq!(&bytes[..], b"1234");
            cache
                .get_or_load(&key, IndexRange::End(4), || panic!("must be cached"))
                .unwrap();
        }
    }

    #[test]
    fn cache_retains_only_index_bytes_from_full_object_response() {
        let cache = ShardIndexCache::default();
        let body = Bytes::from(vec![7; 1024 * 1024]);
        let index = body.slice(body.len() - 4..);
        let cached = cache
            .get_or_load(&StoreKey::new("a").unwrap(), IndexRange::End(4), || {
                Ok(Some(index.clone()))
            })
            .unwrap()
            .unwrap();
        assert_eq!(cached, index);
        assert_ne!(cached.as_ptr(), index.as_ptr());
        assert_eq!(cache.state.lock().unwrap().bytes, 4);
    }
}
