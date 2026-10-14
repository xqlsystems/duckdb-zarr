//! Execution-local source byte accounting and typed, URL-free source errors.

use std::error::Error;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use vgi::diagnostics::{source_error, source_io_error};
use vgi_rpc::{errors::Code, RpcError};
use zarr_core::meta::ZarrStore;
use zarrs::storage::{
    byte_range::ByteRangeIterator, Bytes, MaybeBytesIterator, ReadableStorageTraits, StorageError,
    StoreKey,
};

pub fn read_error(mut error: &(dyn Error + 'static)) -> RpcError {
    loop {
        // zarrs' transparent errors forward source(), which can skip the
        // payload we need. Unwrap these explicitly to preserve HTTP status.
        if let Some(array) = error.downcast_ref::<zarrs::array::ArrayError>() {
            match array {
                zarrs::array::ArrayError::StorageError(storage) => {
                    error = storage;
                    continue;
                }
                zarrs::array::ArrayError::CodecError(codec) => {
                    error = codec;
                    continue;
                }
                _ => {}
            }
        }
        if let Some(zarrs::array::ArrayCreateError::StorageError(storage)) =
            error.downcast_ref::<zarrs::array::ArrayCreateError>()
        {
            error = storage;
            continue;
        }
        if let Some(zarrs::array::CodecError::StorageError(storage)) =
            error.downcast_ref::<zarrs::array::CodecError>()
        {
            error = storage;
            continue;
        }
        if let Some(rpc) = error.downcast_ref::<RpcError>() {
            return rpc.clone();
        }
        if let Some(StorageError::IOError(io)) = error.downcast_ref::<StorageError>() {
            error = io.as_ref();
            continue;
        }
        if let Some(io) = error.downcast_ref::<std::io::Error>() {
            if let Some(rpc) = io.get_ref().and_then(|e| e.downcast_ref::<RpcError>()) {
                return rpc.clone();
            }
            return source_io_error("read", "Zarr store", io);
        }
        match error.source() {
            Some(cause) => error = cause,
            None => return source_error(Code::DataLoss, "decode", "Zarr store"),
        }
    }
}

/// Keep the RPC status through zarrs' cloneable storage error chain.
pub fn storage_error(error: RpcError) -> StorageError {
    std::io::Error::other(error).into()
}

/// Filesystem byte reads. HTTP accounts for the entire received body in its
/// transport, including a server's full response when it ignores Range.
pub struct MeteredStore {
    pub inner: ZarrStore,
    pub bytes_read: Arc<AtomicU64>,
}

impl ReadableStorageTraits for MeteredStore {
    fn get(&self, key: &StoreKey) -> Result<Option<Bytes>, StorageError> {
        let bytes = self.inner.get(key)?;
        if let Some(bytes) = &bytes {
            self.bytes_read
                .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        }
        Ok(bytes)
    }

    fn get_partial_many<'a>(
        &'a self,
        key: &StoreKey,
        ranges: ByteRangeIterator<'a>,
    ) -> Result<MaybeBytesIterator<'a>, StorageError> {
        Ok(self.inner.get_partial_many(key, ranges)?.map(|parts| {
            Box::new(parts.inspect(|part| {
                if let Ok(bytes) = part {
                    self.bytes_read
                        .fetch_add(bytes.len() as u64, Ordering::Relaxed);
                }
            })) as _
        }))
    }

    fn size_key(&self, key: &StoreKey) -> Result<Option<u64>, StorageError> {
        self.inner.size_key(key)
    }

    fn supports_get_partial(&self) -> bool {
        self.inner.supports_get_partial()
    }
}
