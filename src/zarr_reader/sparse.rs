//! Reading compressed sparse matrices (AnnData `csr_matrix` / `csc_matrix`).
//!
//! A table of sparse matrices alone has one row per stored entry: the union of
//! the entries of its matrices, with `0` for a matrix that stores nothing at a
//! row another matrix stores. A sparse matrix in a table with dense arrays is
//! scattered into each dense chunk instead, since that table has a row for
//! every cell anyway. In both cases a cell a matrix does not store reads as 0,
//! which is its value.

use super::meta::{open_array, parse_dtype, read_index_range, ZarrArray, ZarrStore};
use super::types::{SparseMatrix, ZarrDtype};

/// Minor indices of a run of stored entries, and their values if requested.
type Entries = (Vec<u64>, Option<Vec<u8>>);

/// The decoded stored entries of one range of a sparse matrix's major axis.
#[derive(Debug)]
pub struct MajorRange {
    m0: u64,
    m1: u64,
    indices: Vec<u64>,
    data: Vec<u8>,
}

/// One sparse matrix, opened at bind time with its whole `indptr` in memory.
pub struct SparseInput {
    pub matrix: SparseMatrix,
    pub dtype: ZarrDtype,
    data: ZarrArray,
    indices: ZarrArray,
    indptr: Vec<u64>,
}

impl SparseInput {
    pub fn open(
        store: &ZarrStore,
        name: &str,
        matrix: &SparseMatrix,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let data = open_array(store, &matrix.data)?;
        let indices = open_array(store, &matrix.indices)?;
        let indptr_arr = open_array(store, &matrix.indptr)?;
        let dtype = parse_dtype(&data, &matrix.data)?;
        if matrix.shape.len() != 2 || matrix.major_axis > 1 {
            return Err(format!("sparse matrix '{name}' is not 2-D").into());
        }
        let n_major = matrix.shape[matrix.major_axis];
        if indptr_arr.shape() != [n_major + 1] {
            return Err(format!(
                "sparse matrix '{name}': indptr has shape {:?}, expected [{}]",
                indptr_arr.shape(),
                n_major + 1
            )
            .into());
        }
        let indptr = read_index_range(&indptr_arr, &matrix.indptr, 0, n_major + 1)?;
        let nnz = data.shape().first().copied().unwrap_or(0);
        if indptr.windows(2).any(|w| w[0] > w[1])
            || indptr.last().copied() != Some(nnz)
            || indices.shape() != data.shape()
        {
            return Err(
                format!("sparse matrix '{name}': indptr, indices and data do not agree").into(),
            );
        }
        Ok(Self {
            matrix: matrix.clone(),
            dtype,
            data,
            indices,
            indptr,
        })
    }

    fn elem_size(&self) -> usize {
        self.dtype
            .byte_size()
            .expect("sparse data has a fixed-width dtype")
    }

    /// Stored entries along the major axis `m0..m1`: their minor indices and,
    /// when `with_data`, their values as native-endian bytes.
    fn entries(
        &self,
        m0: u64,
        m1: u64,
        with_data: bool,
    ) -> Result<Entries, Box<dyn std::error::Error>> {
        let (start, end) = (self.indptr[m0 as usize], self.indptr[m1 as usize]);
        let indices = read_index_range(&self.indices, &self.matrix.indices, start, end)?;
        let n_minor = self.matrix.shape[1 - self.matrix.major_axis];
        if indices.iter().any(|&i| i >= n_minor) {
            return Err(format!("'{}' holds an index out of range", self.matrix.indices).into());
        }
        let data = if with_data && start < end {
            let subset =
                zarrs::array::ArraySubset::new_with_ranges(std::slice::from_ref(&(start..end)));
            let bytes = self
                .data
                .retrieve_array_subset::<zarrs::array::ArrayBytes<'static>>(&subset)?
                .into_fixed()
                .map_err(|_| format!("'{}' has a variable-length dtype", self.matrix.data))?
                .into_owned();
            Some(bytes)
        } else {
            with_data.then(Vec::new)
        };
        Ok((indices, data))
    }

    /// The stored entries of the major range `m0..m1`, decoded once so that
    /// every dense chunk across that range can be cut from them
    /// ([`Self::dense_chunk`]).
    pub fn major_range(&self, m0: u64, m1: u64) -> Result<MajorRange, Box<dyn std::error::Error>> {
        let (indices, data) = self.entries(m0, m1, true)?;
        Ok(MajorRange {
            m0,
            m1,
            indices,
            data: data.expect("requested data"),
        })
    }

    /// One dense chunk of the matrix: `chunk_shape` elements laid out like
    /// `retrieve_chunk` (padded past the edge), 0 where nothing is stored.
    /// `range` must cover the chunk's major range ([`Self::major_range`]).
    pub fn dense_chunk(&self, range: &MajorRange, origin: &[u64], chunk_shape: &[u64]) -> Vec<u8> {
        let size = self.elem_size();
        let mut out = vec![0u8; (chunk_shape[0] * chunk_shape[1]) as usize * size];
        let major = self.matrix.major_axis;
        let minor = 1 - major;
        let m0 = origin[major];
        debug_assert_eq!(m0, range.m0, "major range does not start at the chunk");
        let (lo, hi) = (origin[minor], origin[minor] + chunk_shape[minor]);
        let base = self.indptr[range.m0 as usize];
        for m in range.m0..range.m1 {
            let (start, end) = (self.indptr[m as usize], self.indptr[m as usize + 1]);
            for pos in (start - base)..(end - base) {
                let j = range.indices[pos as usize];
                if j < lo || j >= hi {
                    continue;
                }
                let mut local = [0u64; 2];
                local[major] = m - m0;
                local[minor] = j - lo;
                let flat = (local[0] * chunk_shape[1] + local[1]) as usize;
                let src = pos as usize * size;
                out[flat * size..(flat + 1) * size].copy_from_slice(&range.data[src..src + size]);
            }
        }
        out
    }
}

/// Split the major axis into ranges of about `target` stored entries each
/// (summed over the matrices), at least one major index per range. Every
/// matrix in a table has the same shape and major axis.
pub fn plan_blocks(inputs: &[&SparseInput], target: u64) -> Vec<(u64, u64)> {
    let Some(first) = inputs.first() else {
        return Vec::new();
    };
    let n_major = first.matrix.shape[first.matrix.major_axis];
    let nnz_before = |m: u64| -> u64 { inputs.iter().map(|i| i.indptr[m as usize]).sum() };
    let mut blocks = Vec::new();
    let mut start = 0u64;
    while start < n_major {
        let base = nnz_before(start);
        let mut end = start + 1;
        while end < n_major && nnz_before(end + 1) - base <= target {
            end += 1;
        }
        blocks.push((start, end));
        start = end;
    }
    blocks
}

/// The rows of one block of a sparse table.
#[derive(Debug, Default)]
pub struct SparseBlock {
    pub major: Vec<u64>,
    pub minor: Vec<u64>,
    /// One buffer per matrix, `None` for a matrix whose column is not
    /// projected: native-endian values, one per row, 0 where the matrix stores
    /// nothing.
    pub values: Vec<Option<Vec<u8>>>,
}

impl SparseBlock {
    pub fn len(&self) -> usize {
        self.major.len()
    }
}

/// Decode the major range `m0..m1` of a sparse table: one row per position
/// that at least one matrix stores, ordered by major then minor index.
/// `projected[v]` says whether matrix `v`'s values are needed.
pub fn decode_block(
    inputs: &[&SparseInput],
    (m0, m1): (u64, u64),
    projected: &[bool],
) -> Result<SparseBlock, Box<dyn std::error::Error>> {
    let mut decoded = Vec::with_capacity(inputs.len());
    for (input, &with_data) in inputs.iter().zip(projected) {
        decoded.push(input.entries(m0, m1, with_data)?);
    }
    let mut block = SparseBlock {
        values: projected.iter().map(|&p| p.then(Vec::new)).collect(),
        ..Default::default()
    };
    let mut row: Vec<(u64, usize, usize)> = Vec::new();
    for m in m0..m1 {
        // (minor index, matrix, entry position) for every stored entry in m.
        row.clear();
        for (v, input) in inputs.iter().enumerate() {
            let base = input.indptr[m0 as usize];
            let (start, end) = (input.indptr[m as usize], input.indptr[m as usize + 1]);
            for pos in (start - base)..(end - base) {
                row.push((decoded[v].0[pos as usize], v, pos as usize));
            }
        }
        row.sort_unstable();
        let mut i = 0;
        while i < row.len() {
            let minor = row[i].0;
            block.major.push(m);
            block.minor.push(minor);
            for (v, values) in block.values.iter_mut().enumerate() {
                if let Some(values) = values {
                    values.resize(values.len() + inputs[v].elem_size(), 0);
                }
            }
            while i < row.len() && row[i].0 == minor {
                let (_, v, pos) = row[i];
                if let (Some(values), Some(data)) = (&mut block.values[v], &decoded[v].1) {
                    let size = inputs[v].elem_size();
                    let dst = values.len() - size;
                    values[dst..].copy_from_slice(&data[pos * size..(pos + 1) * size]);
                }
                i += 1;
            }
        }
    }
    Ok(block)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use zarrs::storage::store::MemoryStore;
    use zarrs::storage::{Bytes, StoreKey, WritableStorageTraits};

    use super::*;

    /// Write a 1-D Zarr v3 array of `values` (one chunk, little-endian).
    fn put(store: &MemoryStore, name: &str, data_type: &str, bytes: Vec<u8>, len: usize) {
        let doc = serde_json::json!({
            "zarr_format": 3, "node_type": "array", "shape": [len], "data_type": data_type,
            "chunk_grid": {"name": "regular", "configuration": {"chunk_shape": [len.max(1)]}},
            "chunk_key_encoding": {"name": "default", "configuration": {"separator": "/"}},
            "fill_value": 0,
            "codecs": [{"name": "bytes", "configuration": {"endian": "little"}}],
            "attributes": {}
        });
        store
            .set(
                &StoreKey::new(format!("{name}/zarr.json")).unwrap(),
                Bytes::from(serde_json::to_vec(&doc).unwrap()),
            )
            .unwrap();
        if len > 0 {
            store
                .set(
                    &StoreKey::new(format!("{name}/c/0")).unwrap(),
                    Bytes::from(bytes),
                )
                .unwrap();
        }
    }

    /// A CSR matrix from (row, col, value) triples, sorted by row then col.
    fn csr(
        store: &MemoryStore,
        name: &str,
        shape: [u64; 2],
        entries: &[(u64, u64, f32)],
    ) -> SparseMatrix {
        let mut indptr = vec![0i64; shape[0] as usize + 1];
        for &(r, _, _) in entries {
            indptr[r as usize + 1] += 1;
        }
        for i in 1..indptr.len() {
            indptr[i] += indptr[i - 1];
        }
        let le = |v: Vec<Vec<u8>>| v.concat();
        put(
            store,
            &format!("{name}/data"),
            "float32",
            le(entries.iter().map(|e| e.2.to_le_bytes().to_vec()).collect()),
            entries.len(),
        );
        put(
            store,
            &format!("{name}/indices"),
            "int32",
            le(entries
                .iter()
                .map(|e| (e.1 as i32).to_le_bytes().to_vec())
                .collect()),
            entries.len(),
        );
        put(
            store,
            &format!("{name}/indptr"),
            "int64",
            le(indptr.iter().map(|v| v.to_le_bytes().to_vec()).collect()),
            indptr.len(),
        );
        SparseMatrix {
            major_axis: 0,
            data: format!("{name}/data"),
            indices: format!("{name}/indices"),
            indptr: format!("{name}/indptr"),
            shape: shape.to_vec(),
        }
    }

    fn floats(bytes: &[u8]) -> Vec<f32> {
        let (chunks, _) = bytes.as_chunks::<4>();
        chunks.iter().map(|b| f32::from_ne_bytes(*b)).collect()
    }

    #[test]
    fn union_of_two_matrices_across_blocks() {
        let inner = MemoryStore::new();
        let a = csr(
            &inner,
            "a",
            [4, 5],
            &[(0, 0, 1.), (0, 3, 2.), (2, 4, 3.), (3, 1, 4.), (3, 2, 5.)],
        );
        let b = csr(
            &inner,
            "b",
            [4, 5],
            &[(0, 3, 10.), (1, 0, 20.), (3, 2, 30.), (3, 4, 40.)],
        );
        let store: ZarrStore = Arc::new(inner);
        let a = SparseInput::open(&store, "a", &a).unwrap();
        let b = SparseInput::open(&store, "b", &b).unwrap();
        let inputs = [&a, &b];

        // A target of one entry puts every row in its own block.
        let blocks = plan_blocks(&inputs, 1);
        assert_eq!(blocks, vec![(0, 1), (1, 2), (2, 3), (3, 4)]);
        assert_eq!(plan_blocks(&inputs, 100), vec![(0, 4)]);

        let mut rows = Vec::new();
        for block in blocks {
            let decoded = decode_block(&inputs, block, &[true, true]).unwrap();
            let (va, vb) = (
                floats(decoded.values[0].as_ref().unwrap()),
                floats(decoded.values[1].as_ref().unwrap()),
            );
            for i in 0..decoded.len() {
                rows.push((decoded.major[i], decoded.minor[i], va[i], vb[i]));
            }
        }
        assert_eq!(
            rows,
            vec![
                (0, 0, 1., 0.),
                (0, 3, 2., 10.),
                (1, 0, 0., 20.),
                (2, 4, 3., 0.),
                (3, 1, 4., 0.),
                (3, 2, 5., 30.),
                (3, 4, 0., 40.),
            ]
        );

        // An unprojected matrix still contributes its rows, but no values.
        let decoded = decode_block(&inputs, (0, 4), &[true, false]).unwrap();
        assert_eq!(decoded.len(), 7);
        assert!(decoded.values[1].is_none());
    }

    #[test]
    fn dense_chunk_scatters_one_chunk() {
        let inner = MemoryStore::new();
        let a = csr(
            &inner,
            "a",
            [4, 5],
            &[(0, 0, 1.), (0, 3, 2.), (2, 4, 3.), (3, 1, 4.), (3, 2, 5.)],
        );
        let store: ZarrStore = Arc::new(inner);
        let a = SparseInput::open(&store, "a", &a).unwrap();
        // Rows 2..4, columns 3..6 (column 5 is padding past the edge).
        let rows = a.major_range(2, 4).unwrap();
        let chunk = a.dense_chunk(&rows, &[2, 3], &[2, 3]);
        assert_eq!(floats(&chunk), vec![0., 3., 0., 0., 0., 0.]);
        // One decoded range serves every chunk across it.
        let rows = a.major_range(0, 2).unwrap();
        let chunk = a.dense_chunk(&rows, &[0, 0], &[2, 3]);
        assert_eq!(floats(&chunk), vec![1., 0., 0., 0., 0., 0.]);
        let chunk = a.dense_chunk(&rows, &[0, 3], &[2, 3]);
        assert_eq!(floats(&chunk), vec![2., 0., 0., 0., 0., 0.]);
    }

    #[test]
    fn indptr_that_disagrees_with_data_is_an_error() {
        let inner = MemoryStore::new();
        let mut a = csr(&inner, "a", [2, 2], &[(0, 0, 1.)]);
        a.shape = vec![3, 2];
        let store: ZarrStore = Arc::new(inner);
        let err = SparseInput::open(&store, "a", &a)
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("indptr has shape"), "{err}");
    }
}
