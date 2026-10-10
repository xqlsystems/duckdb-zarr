# Querying AnnData stores

[AnnData](https://anndata.readthedocs.io/) is the file format of the scverse
single-cell ecosystem (scanpy, cellxgene). This page shows how to query a store
that `AnnData.write_zarr()` wrote: cell and gene annotations (`obs`, `var`),
the expression matrix (`X`), `layers`, embeddings (`obsm`) and pairwise
matrices (`obsp`, `varp`).

The examples use the small test store in this repository. Run
`make generate_fixtures` to create it. The store has 20 cells, named `cell_0`
to `cell_19`, and 8 genes, named `gene_0` to `gene_7`.

## How a store maps to tables

The extension reads an AnnData store as a tree of tables. Each Zarr group is
a DuckDB schema, and each set of variables with the same dimensions in a group
is one table. The root group is the schema `main`. The design is in
[docs/design.md](design.md), decisions 8 and 9.

| Element | Dimensions | Table |
|---|---|---|
| `X` | `obs`, `var` | `main.obs_var` |
| `obs` columns | `obs` | `main.obs` |
| `var` columns | `var` | `main.var` |
| `layers/*` | `obs`, `var` | `layers.obs_var` |
| `obsm/<k>` | `obs`, `<k>_component` | `obsm.obs_<k>_component` |
| `varm/<k>` | `var`, `<k>_component` | `varm.var_<k>_component` |
| `obsp/*` | `obs_i`, `obs_j` | `obsp.obs_i_obs_j` |
| `varp/*` | `var_i`, `var_j` | `varp.var_i_var_j` |
| `raw/X` | `obs`, `raw_var` | `raw.obs_raw_var` |
| `raw/var` columns | `raw_var` | `raw.raw_var` |

The `obs` column holds the cell name (`obs_names` in AnnData) and the `var`
column holds the gene name (`var_names`). Every table has these names, so you
can filter and join any table by name. Both axes of `obsp` show cell names, and
both axes of `varp` show gene names. The `<k>_component` column of an `obsm`
table is the component number, from 0.

If a data frame has a column with the same name as its axis, for example an
`obs` column called `obs`, that column keeps its path in the store as its
name, `"obs/obs"`. Otherwise it would have the same name as the column of cell
names.

The extension decodes AnnData's encodings:

- A categorical column with string categories reads as a DuckDB `ENUM` of its
  categories. A categorical with other categories (for example integers)
  reads as the type of its categories. A missing value is `NULL`.
- A nullable column (`nullable-integer`, `nullable-boolean`,
  `nullable-string-array`) reads as its values, with `NULL` where the mask is
  set.
- A sparse matrix (`csr_matrix` or `csc_matrix`) reads as one row for each
  stored entry. See [Sparse matrices omit zeros](#sparse-matrices-omit-zeros).

The extension does not read `uns`. Read its arrays one at a time with
`array_path=` (see [Read one array](#read-one-array)).

## List the tables

`read_zarr_groups` gives one row for each table. `group_path` is the Zarr
group, `schema_name` is the DuckDB schema for that group, and `table_name` is
the table name.

```sql
SELECT group_path, schema_name, table_name, dims
FROM read_zarr_groups('test/fixtures/anndata/pbmc_like.zarr');
```

```text
/        main    obs                   ["obs"]
/        main    obs_var               ["obs","var"]
/        main    var                   ["var"]
/layers  layers  obs_var               ["obs","var"]
/obsm    obsm    obs_X_umap_component  ["obs","X_umap_component"]
/obsp    obsp    obs_i_obs_j           ["obs_i","obs_j"]
/varp    varp    var_i_var_j           ["var_i","var_j"]
```

## Read a table

`read_zarr` reads the root group. The root has three tables, so use
`dims :=` to select one. Use `group_path :=` to read another group.

```sql
-- Cell annotations: one row for each cell
SELECT obs, cell_type, donor, n_genes
FROM read_zarr('test/fixtures/anndata/pbmc_like.zarr', dims := ['obs']);

-- Gene annotations: one row for each gene
SELECT var, gene_symbol, mean_expr
FROM read_zarr('test/fixtures/anndata/pbmc_like.zarr', dims := ['var']);

-- The expression matrix: one row for each stored entry
SELECT obs, var, X
FROM read_zarr('test/fixtures/anndata/pbmc_like.zarr', dims := ['obs', 'var']);

-- Two layers of one cell, by name
SELECT var, spliced, unspliced
FROM read_zarr('test/fixtures/anndata/pbmc_like.zarr', group_path := 'layers')
WHERE obs = 'cell_0';
```

In this store, `cell_4` has no cell type, so `cell_type` is `NULL` for
`cell_4`. `gene_6` has no symbol, so `gene_symbol` is `NULL` for `gene_6`.

`"group" :=` is the same parameter as `group_path :=`. `GROUP` is a keyword in
DuckDB, so the quotes are necessary.

## Mount the store as a database

Views give each table a stable name, so queries do not repeat `read_zarr`.
Attach an in-memory database, then create one schema for each group and one
view for each table. The names come from `read_zarr_groups`.

```sql
ATTACH ':memory:' AS pbmc;
CREATE VIEW pbmc.main.obs AS
  SELECT * FROM read_zarr('test/fixtures/anndata/pbmc_like.zarr', group_path := '', dims := ['obs']);
CREATE VIEW pbmc.main.var AS
  SELECT * FROM read_zarr('test/fixtures/anndata/pbmc_like.zarr', group_path := '', dims := ['var']);
CREATE VIEW pbmc.main.obs_var AS
  SELECT * FROM read_zarr('test/fixtures/anndata/pbmc_like.zarr', group_path := '', dims := ['obs', 'var']);
CREATE SCHEMA pbmc.layers;
CREATE VIEW pbmc.layers.obs_var AS
  SELECT * FROM read_zarr('test/fixtures/anndata/pbmc_like.zarr', group_path := 'layers', dims := ['obs', 'var']);
```

A table in `main` needs no schema name: `pbmc.obs` is `pbmc.main.obs`. The
examples in the rest of this page use these views.

The views read the store again for each query. If you run many queries,
`CREATE TABLE ... AS SELECT` copies the data into DuckDB once.
[docs/design.md](design.md), decision 8, has a query that writes the
statements for every row of `read_zarr_groups`.

## Sparse matrices omit zeros

AnnData usually stores `X` as a sparse matrix. A table of sparse matrices has a
row only where a matrix stores a value. A missing row means the value is 0.
This store has 27 stored entries in a matrix of 160 cells, so the `X` table has
27 rows.

For this reason, `AVG(X)` over the `X` table is a mean over the stored entries
only. To get a mean over cells, start from `obs` and use a `LEFT JOIN`. Each
cell then has one row, and `COALESCE` gives 0 to the cells that have no entry:

```sql
-- Mean expression of Cd4 in each cell type
SELECT o.cell_type, AVG(COALESCE(x.X, 0)) AS mean_cd4
FROM pbmc.obs o
LEFT JOIN (
  SELECT x.obs, x.X
  FROM pbmc.obs_var x JOIN pbmc.var v USING (var)
  WHERE v.gene_symbol = 'Cd4'
) x USING (obs)
GROUP BY o.cell_type;
```

```text
B cell   17.666667
NK cell   9.333333
T cell   23.142857
NULL      0.000000
```

When the gene names are the gene symbols, as in most scanpy data sets, the
inner query is not necessary: `LEFT JOIN pbmc.obs_var x ON x.obs = o.obs AND
x.var = 'CD4'`.

If a group has two or more sparse matrices with the same dimensions, they share
one table. The table has a row for each position that at least one matrix
stores. A matrix that has no value at that position reads as 0 in that row. In
this store, `layers/spliced` stores 53 entries and `layers/unspliced` stores
80. The `layers.obs_var` table has 106 rows, because 27 positions are in both.

If a group has a sparse matrix and a dense array with the same dimensions,
the table has a row for each cell. The sparse matrix reads as 0 where it stores
no value. In this store, `varp` has a dense `corr` and a sparse `corr_csc`, so
the `varp.var_i_var_j` table has 64 rows.

## Embeddings and pairwise matrices

An `obsm` table has one row for each cell and component. Use `FILTER` or
`PIVOT` to get one column for each component:

```sql
-- UMAP coordinates of each cell
SELECT u.obs,
       MAX(u.X_umap) FILTER (WHERE u.X_umap_component = 0) AS umap_1,
       MAX(u.X_umap) FILTER (WHERE u.X_umap_component = 1) AS umap_2
FROM read_zarr('test/fixtures/anndata/pbmc_like.zarr', group_path := 'obsm') u
GROUP BY u.obs;
```

An `obsp` table has one row for each stored pair of cells. For a neighbor
graph, the neighbors of one cell are:

```sql
SELECT obs_j AS neighbor, connectivities
FROM read_zarr('test/fixtures/anndata/pbmc_like.zarr', group_path := 'obsp')
WHERE obs_i = 'cell_3';
```

## Cell names must be unique

Joins on `obs` and `var` compare names. If two cells have the same name, a
join matches each row of one cell to both cells, and the result has extra
rows. anndata warns about duplicate names when it loads such a store. Make the
names unique before you write the store, for example with
`adata.obs_names_make_unique()` and `adata.var_names_make_unique()`.

## Read one array

`array_path :=` reads one array and ignores the group structure. Use it for
`uns`, or to look at the arrays that hold an encoded column.

An AnnData element gets the same dimension names as in its table, but one
array has no index to take names from, so its dimension columns hold integer
positions. For example, `array_path := 'obs/n_genes'` gives the columns `obs`
(0 to 19) and `value`. An array inside an encoded group has no AnnData
dimensions. Its dimensions are `dim_0`, `dim_1`, and so on. For example,
`array_path := 'obs/cell_type/codes'` gives the integer codes in the columns
`dim_0` and `value`, and the categories are not applied.

`read_zarr_metadata` lists every array. The `array_path_dims` column gives the
dimension names that `array_path :=` uses for each array. With `group_path :=`,
it lists the arrays of that group's tables: `group_path := '/'` gives the
arrays of `X` and of the `obs` and `var` columns.

```sql
SELECT name, dtype, shape, array_path_dims
FROM read_zarr_metadata('test/fixtures/anndata/pbmc_like.zarr')
ORDER BY name;
```

`array_path :=` also selects an encoded column on its own, decoded: a sparse
matrix, a categorical or a nullable column. Like one array, its dimension
columns hold integer positions. For example, `array_path := 'X'` gives the
stored entries of a sparse `X` as `obs`, `var` and `value`, and
`read_zarr_metadata(store, array_path := 'X')` lists the arrays it is made of.
`group_path :=` cannot name an encoded column, because it is not a group of
tables.

## Limits

- The extension uses this layout only when the root group has
  `encoding-type: anndata`. An old store that has no `encoding-type`
  attributes reads only with `array_path :=`.
- The extension does not read `uns`, awkward arrays, or MuData stores.
- A sparse matrix with duplicate entries for one position gives one row with
  the last value. scipy adds duplicate entries.
- Sparse matrices with the same dimensions and no dense array beside them
  share a table only if all are CSR or all are CSC. A table that mixes the two
  has no `table_name` in `read_zarr_groups`; read each matrix with
  `array_path :=`.
- If two columns of one data frame differ only in case, or one has the name
  of its axis, they keep their paths as names (`"obs/obs"`), because DuckDB
  column names ignore case. A path that still collides gets a number
  (`"obs/Obs_1"`).
- Remote stores need consolidated metadata to list arrays. Without it, only
  `array_path :=` works.
