# Querying AnnData stores

[AnnData](https://anndata.readthedocs.io/) is the file format of the scverse
single-cell ecosystem (scanpy, cellxgene). `AnnData.write_zarr()` writes a
Zarr store that you can query with `read_zarr`, one array at a time. This page
shows how to read cell and gene annotations (`obs`, `var`), embeddings
(`obsm`), and the sparse expression matrix (`X`) with SQL.

The extension does not decode AnnData's encodings. It does not assemble data
frames, map categorical codes to labels, or expand sparse matrices. You do these
steps in SQL, and this page gives the queries. Some of the queries are easy to
get wrong without an error, so read the notes on nulls and empty rows.

The examples use the small test store in this repository. Run
`make generate_fixtures` to create it. The store has 20 cells and 8 genes.

## How AnnData lays out a store

AnnData writes each part of the object as a Zarr group:

| Path | Contents |
|------|----------|
| `obs/<column>` | One cell annotation. Numeric columns are arrays. String and categorical columns are groups (see below). |
| `obs/_index` | The cell names. |
| `var/<column>`, `var/_index` | The same, for genes. |
| `obsm/<name>` | A dense array with one row per cell, for example a UMAP embedding. |
| `X` | The expression matrix. A sparse matrix is the group `X/data`, `X/indices`, `X/indptr`. |

AnnData does not write dimension names on its arrays. For that reason, you must
read each array on its own with `array_path=`. The dimensions of the array then
get the names `dim_0`, `dim_1`, and so on, and the array's values are in the
column `value`. A query on the whole store fails:

```sql
SELECT * FROM read_zarr('test/fixtures/anndata/pbmc_like.zarr');
-- Error: array '...' has no dimension_names or _ARRAY_DIMENSIONS; read it on its
-- own with array_path='...', which names its dimensions dim_0, dim_1, ...
```

The extension refuses to guess here on purpose. If it gave every array the
names `dim_0`, `dim_1`, then two unrelated arrays of the same length would join
row by row as if they shared an axis.

## List the arrays

`read_zarr_metadata` lists every array in the store. The `array_path_dims`
column shows the dimension names that `read_zarr(..., array_path := name)` will
use. The `dims` column shows only the names that the array itself declares,
which for AnnData is always `[]`.

```sql
SELECT name, dtype, shape, array_path_dims
FROM read_zarr_metadata('test/fixtures/anndata/pbmc_like.zarr')
ORDER BY name;
```

If zarrs cannot open an array because of its data type or codec, that array has
`role = 'unsupported'`, and `attrs` holds the error message. The other arrays
are still listed.

## Read numeric columns and embeddings

A numeric `obs` or `var` column is a 1-D array. `dim_0` is the cell (or gene)
position:

```sql
SELECT dim_0 AS cell, value AS n_genes
FROM read_zarr('test/fixtures/anndata/pbmc_like.zarr', array_path := 'obs/n_genes');
```

An `obsm` entry is a 2-D array. `dim_0` is the cell and `dim_1` is the
component. To get one row per cell, pivot the components:

```sql
SELECT dim_0 AS cell,
       MAX(value) FILTER (WHERE dim_1 = 0) AS umap_1,
       MAX(value) FILTER (WHERE dim_1 = 1) AS umap_2
FROM read_zarr('test/fixtures/anndata/pbmc_like.zarr', array_path := 'obsm/X_umap')
GROUP BY cell
ORDER BY cell;
```

## Read string columns

AnnData stores a string column in one of two ways. Both keep missing values
outside the array that holds the data, so you must apply them yourself.

### Nullable string arrays

A column such as `var/gene_symbol` is a group with two arrays: `values` holds
the strings and `mask` is `true` where the value is missing. If you read
`values` alone, a missing value reads as the empty string `''`, and nothing in
the result tells you that it was missing. Join the mask to get `NULL`:

```sql
SELECT v.dim_0 AS gene,
       CASE WHEN m.value THEN NULL ELSE v.value END AS gene_symbol
FROM read_zarr('test/fixtures/anndata/pbmc_like.zarr', array_path := 'var/gene_symbol/values') v
JOIN read_zarr('test/fixtures/anndata/pbmc_like.zarr', array_path := 'var/gene_symbol/mask') m
  USING (dim_0)
ORDER BY gene;
```

The cell and gene names (`obs/_index`, `var/_index`) use the same layout in
this store. Older AnnData versions write a column with no missing values as a
plain string array with no `mask`. Look at the names from
`read_zarr_metadata` to see which layout a column has.

### Categorical columns

A categorical column such as `obs/cell_type` is a group with two arrays:
`codes` holds one integer per cell, and `categories` holds the labels. Code `n`
is the label at position `n` of `categories`. A missing value has code `-1`.

Use a `LEFT JOIN`. An inner join drops every cell whose value is missing:

```sql
SELECT codes.dim_0 AS cell, cat.value AS cell_type
FROM read_zarr('test/fixtures/anndata/pbmc_like.zarr', array_path := 'obs/cell_type/codes') codes
LEFT JOIN read_zarr('test/fixtures/anndata/pbmc_like.zarr', array_path := 'obs/cell_type/categories') cat
  ON codes.value = cat.dim_0
ORDER BY cell;
```

## Read the sparse expression matrix

This section applies to a matrix stored as `csr_matrix` (compressed sparse
rows). Look at `encoding-type` in the attributes of the `X` group
(`X/zarr.json`, or `X/.zattrs` for Zarr v2) to find which format you have. For
a `csc_matrix`, `indptr` indexes genes and `indices` holds cell positions, so
you must swap the cell and gene roles in the queries below. The extension does
not read `encoding-type` for you.

A CSR matrix is three 1-D arrays:

- `X/data` holds the nonzero values, row by row.
- `X/indices` holds the gene (column) of each value in `X/data`.
- `X/indptr` has one entry per cell plus one. The values of cell `r` are at
  positions `indptr[r]` up to, but not including, `indptr[r + 1]`.

### Load the arrays into tables first

A join between several `read_zarr` calls can scan the store again for each row
group. One user reported about 50 s for such a join, against 0.25 s when each
array was loaded into a table first ([issue #40](https://github.com/xqlsystems/duckdb-zarr/issues/40)).
Load the three arrays before you join them:

```sql
CREATE TEMP TABLE x_data AS
  SELECT dim_0 AS pos, value FROM read_zarr('test/fixtures/anndata/pbmc_like.zarr', array_path := 'X/data');
CREATE TEMP TABLE x_indices AS
  SELECT dim_0 AS pos, value AS gene FROM read_zarr('test/fixtures/anndata/pbmc_like.zarr', array_path := 'X/indices');
CREATE TEMP TABLE x_indptr AS
  SELECT dim_0 AS cell, value AS start_pos FROM read_zarr('test/fixtures/anndata/pbmc_like.zarr', array_path := 'X/indptr');
```

### Expand to one row per nonzero value

The result has the columns `cell`, `gene`, and `value`. Zeros are not in the
result, so the matrix is never made dense.

```sql
CREATE TEMP TABLE x_long AS
WITH cell_start AS (
  -- A cell with no nonzero values has the same start_pos as the next cell.
  -- Keep only the last cell for each start_pos.
  SELECT start_pos, MAX(cell) AS cell FROM x_indptr GROUP BY start_pos
)
SELECT c.cell, i.gene, d.value
FROM x_data d
JOIN x_indices i USING (pos)
ASOF JOIN cell_start c ON d.pos >= c.start_pos;
```

Do not remove the `GROUP BY` step. A cell with no nonzero values repeats the
start position of the next cell in `indptr`. Without the `GROUP BY`, the
`ASOF JOIN` chooses between the two cells arbitrarily and can give values to
the empty cell. Filtered single-cell data often has empty cells, and the query
still returns the correct number of rows, so this mistake is hard to see.

This query gives the same result and is easier to check by eye, but it is
slower:

```sql
WITH cell_range AS (
  SELECT cell, start_pos, LEAD(start_pos) OVER (ORDER BY cell) AS end_pos
  FROM x_indptr
)
SELECT r.cell, i.gene, d.value
FROM x_data d
JOIN x_indices i USING (pos)
JOIN cell_range r ON d.pos >= r.start_pos AND d.pos < r.end_pos;
```

Both queries use time and memory in proportion to the number of nonzero
values. On a synthetic matrix with 2.1 million nonzero values (about the size
of the scanpy `pbmc3k` data set), in in-memory tables on 4 threads, the
`ASOF JOIN` query took 0.15 s and about 170 MB, and the range query took
0.4 s to 0.6 s and about 390 MB. These times do not include reading the store.

### Example: mean expression per cell type

This query joins the expanded matrix to the cell types and gene symbols. It
gives the mean expression of each gene in each cell type, which is the input
for a scanpy-style dot plot. Cells with no value for a gene count as zero. A
gene with no nonzero value in a cell type is not in the result.

```sql
WITH cell_type AS (
  SELECT codes.dim_0 AS cell, cat.value AS cell_type
  FROM read_zarr('test/fixtures/anndata/pbmc_like.zarr', array_path := 'obs/cell_type/codes') codes
  LEFT JOIN read_zarr('test/fixtures/anndata/pbmc_like.zarr', array_path := 'obs/cell_type/categories') cat
    ON codes.value = cat.dim_0
), gene AS (
  SELECT v.dim_0 AS gene, CASE WHEN m.value THEN NULL ELSE v.value END AS symbol
  FROM read_zarr('test/fixtures/anndata/pbmc_like.zarr', array_path := 'var/gene_symbol/values') v
  JOIN read_zarr('test/fixtures/anndata/pbmc_like.zarr', array_path := 'var/gene_symbol/mask') m
    USING (dim_0)
), cells_per_type AS (
  SELECT cell_type, COUNT(*) AS n_cells FROM cell_type GROUP BY cell_type
)
SELECT ct.cell_type, g.symbol,
       SUM(x.value) / ANY_VALUE(n.n_cells) AS mean_expression
FROM x_long x
JOIN cell_type ct USING (cell)
JOIN gene g USING (gene)
JOIN cells_per_type n ON n.cell_type IS NOT DISTINCT FROM ct.cell_type
GROUP BY ct.cell_type, g.symbol
ORDER BY ct.cell_type, g.symbol;
```

## Limits

- Each `read_zarr` call reads one array. Joins between arrays are yours to
  write.
- The extension does not read AnnData's `encoding-type` attributes. It does not
  know whether a group is a data frame, a categorical, or a CSR or CSC matrix.
- Dense `X`, `layers`, `obsp`, and `varp` read the same way as `obsm`: a 2-D
  array with `dim_0` and `dim_1`.
- Remote stores need consolidated metadata for `read_zarr_metadata`. Without
  it, you can still read an array by its `array_path=` if you know the path.
