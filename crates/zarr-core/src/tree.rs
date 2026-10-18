//! Nested Zarr groups as a tree of tables (design decision 8).
//!
//! Each Zarr group is a node, as in `xarray.DataTree`. A table is the set of a
//! node's variables that share dimension names and shape: variables in
//! different nodes never share a table, even when their dimension names match. A node's tables use coordinate arrays from
//! the node itself or, failing that, from its nearest ancestor that has one.
//! Tables are named the way xarray-sql names them (`"_".join(dims)`), and each
//! node maps to one DuckDB schema (the root is `main`).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use super::anndata;
use super::meta::{
    cf_auxiliary_vars, declared_dims, first_chunk_shape, is_unsupported_array_error, open_array,
    raw_array_layout, ArrayFacts, ZarrStore,
};
use super::types::{Storage, Table, Variable};

/// The variables of the nodes that were loaded, keyed by store-relative path.
#[derive(Debug, Default)]
pub struct StoreTree {
    pub variables: BTreeMap<String, Variable>,
    /// Coordinates by `(node, dim)`: the array that holds the values, and its
    /// length. A dimension coordinate (a 1-D array named after its only
    /// dimension) is registered in its node; so is an AnnData data frame's
    /// index, under its axis (`obs`, `var`).
    pub coords: HashMap<(String, String), (String, u64)>,
    /// The layout of the store's AnnData objects (decision 9); `None` for a
    /// store that has none.
    pub layout: Option<anndata::Layout>,
    /// Arrays zarrs cannot open and whose metadata document cannot be read
    /// either. They are left out of every table; `read_zarr_metadata` lists
    /// them.
    pub unsupported: Vec<String>,
}

/// Normalize a user-supplied group path: `'/a/b/'` and `'a/b'` are the same
/// node, and `''` or `'/'` is the root.
pub fn normalize_group(group: &str) -> String {
    group.trim().trim_matches('/').to_string()
}

/// The node (group path) that holds `path`; `""` for the root.
pub fn node_of(path: &str) -> &str {
    path.rsplit_once('/').map(|(node, _)| node).unwrap_or("")
}

/// `path` relative to its node.
pub fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// The store-relative path of `name` inside `node`.
pub fn join(node: &str, name: &str) -> String {
    if node.is_empty() {
        name.to_string()
    } else {
        format!("{node}/{name}")
    }
}

/// `node` followed by its ancestors up to the root: `a/b` → `[a/b, a, ""]`.
pub fn self_and_ancestors(node: &str) -> Vec<String> {
    let mut chain = vec![node.to_string()];
    let mut current = node;
    while !current.is_empty() {
        current = node_of(current);
        chain.push(current.to_string());
    }
    chain
}

/// The node path as `xarray.DataTree` writes it: `/` for the root, `/a/b` below.
pub fn display_group(node: &str) -> String {
    format!("/{node}")
}

/// The DuckDB schema a node maps to: `main` for the root, else the node path.
pub fn schema_name(node: &str) -> String {
    if node.is_empty() {
        "main".to_string()
    } else {
        node.to_string()
    }
}

/// A name as DuckDB compares identifiers: ASCII letters ignore case, other
/// characters do not (`Abc` and `abc` are one view, `Ärger` and `ärger` two).
pub fn fold_case(name: &str) -> String {
    name.to_ascii_lowercase()
}

/// xarray-sql's default table name (`xarray_sql.df.default_table_name`): the
/// dimension names joined by `_` in dimension order, or `scalar` for none.
pub fn default_table_name(dims: &[String]) -> String {
    if dims.is_empty() {
        "scalar".to_string()
    } else {
        dims.join("_")
    }
}

impl StoreTree {
    /// Open the arrays in `nodes` (all arrays when `None`) and record their
    /// metadata. An array zarrs cannot open is recorded with its error
    /// ([`Storage::Unreadable`]), or in `unsupported` if even its metadata
    /// document cannot be read; any other open error fails.
    pub fn load(
        store: &ZarrStore,
        array_names: &[String],
        nodes: Option<&HashSet<String>>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        // In an AnnData object, name each element's axes and read encoded
        // groups (categoricals, nullable columns, sparse matrices) as one
        // variable each (decision 9). Stores without one skip this entirely.
        let layout = anndata::layout(store, array_names);
        let encoding_of = |group: &str| layout.as_ref().and_then(|l| l.encoding_of(group));
        // Without an AnnData layout, an element belongs to the group that holds
        // it and nothing is a data frame column or part of an encoded group.
        let logical = |path: &str| match &layout {
            Some(layout) => layout.node_of(path),
            None => node_of(path).to_string(),
        };
        let in_dataframe = |path: &str| layout.as_ref().is_some_and(|l| l.is_frame_column(path));
        // The arrays of an encoded variable belong to it, not to its group.
        let inside_variable = |path: &str| {
            layout
                .as_ref()
                .is_some_and(|l| l.variable_of(node_of(path)).is_some())
        };
        let wanted = |path: &str| nodes.is_none_or(|nodes| nodes.contains(&logical(path)));

        let mut tree = StoreTree::default();
        for name in array_names {
            if !wanted(name)
                || inside_variable(name)
                || layout.as_ref().is_some_and(|l| l.is_unstructured(name))
            {
                continue;
            }
            let arr = match open_array(store, name) {
                Ok(arr) => arr,
                Err(err) if is_unsupported_array_error(err.as_ref()) => {
                    match raw_array_layout(store, name) {
                        Some((shape, dims)) => {
                            tree.variables.insert(
                                name.clone(),
                                Variable {
                                    path: name.clone(),
                                    node: logical(name),
                                    dims: dims.or_else(|| {
                                        anndata::axis_names(name, shape.len(), encoding_of)
                                    }),
                                    chunk_shape: shape.clone(),
                                    shape,
                                    attrs: Default::default(),
                                    storage: Storage::Unreadable {
                                        error: err.to_string(),
                                    },
                                    in_dataframe: in_dataframe(name),
                                },
                            );
                        }
                        None => tree.unsupported.push(name.clone()),
                    }
                    continue;
                }
                Err(err) => return Err(err),
            };
            let shape = arr.shape().to_vec();
            let chunk_shape = first_chunk_shape(&arr)?;
            let dims = declared_dims(store, &arr, name)
                .or_else(|| anndata::axis_names(name, shape.len(), encoding_of));
            tree.variables.insert(
                name.clone(),
                Variable {
                    path: name.clone(),
                    node: logical(name),
                    dims,
                    shape,
                    chunk_shape,
                    attrs: arr.attributes().clone(),
                    storage: Storage::Array,
                    in_dataframe: in_dataframe(name),
                },
            );
        }

        let Some(anndata_layout) = layout.as_ref() else {
            tree.register_dim_coords();
            return Ok(tree);
        };
        for (group, encoding) in &anndata_layout.encodings {
            if !anndata_layout.is_variable(group) || !wanted(group) || inside_variable(group) {
                continue;
            }
            match anndata::encoded_variable(store, anndata_layout, group, encoding) {
                Ok(Some(variable)) => {
                    tree.variables.insert(group.clone(), variable);
                }
                Ok(None) => {}
                Err(err) if is_unsupported_array_error(err.as_ref()) => {
                    tree.unsupported.push(group.clone());
                }
                Err(err) => return Err(err),
            }
        }
        tree.register_dim_coords();
        if let Some(layout) = &layout {
            tree.register_anndata_indexes(layout);
        }
        tree.layout = layout;
        Ok(tree)
    }

    /// Register every dimension coordinate in its node.
    fn register_dim_coords(&mut self) {
        for var in self.variables.values().filter(|v| is_dim_coord(v)) {
            self.coords.insert(
                (var.node.clone(), basename(&var.path).to_string()),
                (var.path.clone(), var.shape[0]),
            );
        }
    }

    /// Make each AnnData data frame's index the coordinate of its axis, as
    /// `obs_names` and `var_names` are, instead of a column called `_index`.
    /// The axes of `obsp` and `varp` get the same names.
    fn register_anndata_indexes(&mut self, layout: &anndata::Layout) {
        for (frame, index) in &layout.indexes {
            let path = join(frame, index);
            let Some(var) = self.variables.get(&path) else {
                continue;
            };
            // A string array, or a nullable string array whose values are
            // never missing in an index.
            let values = match &var.storage {
                Storage::Array => path.clone(),
                Storage::Nullable { values, .. } => values.clone(),
                _ => continue,
            };
            let Some([dim]) = var.dims.as_deref() else {
                continue;
            };
            let key = (var.node.clone(), dim.clone());
            let len = var.shape[0];
            self.variables.remove(&path);
            let coord = (values, len);
            // Only AnnData indexes have pairwise-axis aliases. Aliasing every
            // coordinate in a mixed store would overwrite plain-Zarr obs_i /
            // var_i coordinates with unrelated obs / var values.
            for &(alias, of) in anndata::COORDINATE_ALIASES {
                if of == key.1.as_str() {
                    self.coords
                        .insert((key.0.clone(), alias.to_string()), coord.clone());
                }
            }
            self.coords.insert(key, coord);
        }
    }

    /// Load `node` and its ancestors, which is everything a read of `node` needs.
    pub fn load_for_node(
        store: &ZarrStore,
        array_names: &[String],
        node: &str,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let nodes: HashSet<String> = self_and_ancestors(node).into_iter().collect();
        Self::load(store, array_names, Some(&nodes))
    }

    /// Every node that directly holds at least one array, sorted.
    pub fn nodes(&self) -> Vec<String> {
        let nodes: BTreeSet<&str> = self.variables.values().map(|v| v.node.as_str()).collect();
        nodes.into_iter().map(str::to_string).collect()
    }

    fn variables_in<'a>(&'a self, node: &'a str) -> impl Iterator<Item = &'a Variable> + 'a {
        self.variables.values().filter(move |v| v.node == node)
    }

    /// The tables of one node: its variables grouped by `(dims, shape)`,
    /// sorted by dims. Variables in one table may be chunked differently; the
    /// table's `chunk_shape` is the largest chunk length in each dimension,
    /// the grid `read_zarr` plans work units on (decision 6). Two tables with
    /// the same dims hold variables that share dimension names but not a
    /// shape; `read_zarr` cannot read those dims as one table. A node that is
    /// not aligned with its ancestors is reported in
    /// [`NodeTables::misaligned`] rather than as an error, so that
    /// `read_zarr_tables` can still list the rest of the store.
    pub fn tables(&self, node: &str) -> NodeTables {
        let mut misaligned = self.check_alignment(node).err();
        let facts: Vec<ArrayFacts> = self
            .variables_in(node)
            .map(|v| (v.path.as_str(), &v.attrs, v.shape.as_slice()))
            .collect();
        // CF rules do not apply to AnnData, which writes no CF attributes: an
        // obsm entry named `spatial_bounds` with two components is data.
        let in_anndata = self
            .layout
            .as_ref()
            .is_some_and(|l| l.object_of(node).is_some());
        let (aux_coords, bounds) = if in_anndata {
            Default::default()
        } else {
            cf_auxiliary_vars(&facts)
        };

        type TableKey = (Vec<String>, Vec<u64>);
        let mut tables: HashMap<TableKey, Table> = HashMap::new();
        let mut unnamed = Vec::new();
        for var in self.variables_in(node) {
            if var.shape.is_empty() || bounds.contains(&var.path) || aux_coords.contains(&var.path)
            {
                continue;
            }
            let Some(dims) = &var.dims else {
                unnamed.push(var.path.clone());
                continue;
            };
            if is_dim_coord(var) {
                continue;
            }
            // A table's rows are the union of the cells its variables store: a
            // dense array stores every cell and a sparse matrix only its
            // entries. So a sparse matrix in a table with a dense array is read
            // densely there; sparse matrices on their own make a table of
            // stored entries. Chunk shapes may differ (decision 6).
            let key = (dims.clone(), var.shape.clone());
            if let Some(table) = tables.get_mut(&key) {
                table.variables.push(var.clone());
                continue;
            }
            let mut coords = Vec::new();
            for (dim, &len) in dims.iter().zip(&var.shape) {
                match self.find_coord(node, dim, len) {
                    Ok(Some(coord)) => coords.push((dim.clone(), coord)),
                    Ok(None) => {}
                    Err(msg) => {
                        misaligned.get_or_insert(msg);
                    }
                }
            }
            tables.insert(
                key,
                Table {
                    dims: dims.clone(),
                    shape: var.shape.clone(),
                    chunk_shape: Vec::new(),
                    coords,
                    variables: vec![var.clone()],
                },
            );
        }

        let mut tables: Vec<Table> = tables.into_values().collect();
        for table in &mut tables {
            table.chunk_shape = plan_grid(table);
            // An AnnData data frame lists its columns in `column-order`.
            if let Some(layout) = &self.layout {
                table
                    .variables
                    .sort_by_cached_key(|v| layout.column_rank(&v.path));
            }
        }
        tables.sort_by(|a, b| {
            a.dims
                .cmp(&b.dims)
                .then_with(|| a.shape.cmp(&b.shape))
                .then_with(|| a.variable_paths().cmp(&b.variable_paths()))
        });
        NodeTables {
            tables,
            unnamed,
            misaligned,
        }
    }

    /// The coordinate array for `dim` as seen from `node`: the node's own, else
    /// the nearest ancestor's, as `DataTree` inherits coordinates. An inherited
    /// coordinate whose length differs from `len` is an error, as in xarray.
    fn find_coord(&self, node: &str, dim: &str, len: u64) -> Result<Option<String>, String> {
        for ancestor in self_and_ancestors(node) {
            let Some((path, coord_len)) = self.coords.get(&(ancestor, dim.to_string())) else {
                continue;
            };
            if *coord_len != len {
                return Err(format!(
                    "group '{}' is not aligned with its ancestors: dimension '{dim}' has \
                     length {len}, but coordinate '{path}' has length {coord_len}",
                    display_group(node),
                ));
            }
            return Ok(Some(path.clone()));
        }
        Ok(None)
    }

    /// Check that `node` shares each dimension length with its parent, as
    /// `DataTree` requires. Only arrays with dimension names count.
    fn check_alignment(&self, node: &str) -> Result<(), String> {
        if node.is_empty() {
            return Ok(());
        }
        let parent = node_of(node);
        let mut parent_sizes: HashMap<&str, (u64, &str)> = HashMap::new();
        for var in self.variables_in(parent) {
            let Some(dims) = &var.dims else { continue };
            for (dim, &len) in dims.iter().zip(&var.shape) {
                parent_sizes.entry(dim).or_insert((len, &var.path));
            }
        }
        for var in self.variables_in(node) {
            let Some(dims) = &var.dims else { continue };
            for (dim, &len) in dims.iter().zip(&var.shape) {
                if let Some(&(parent_len, parent_var)) = parent_sizes.get(dim.as_str()) {
                    if parent_len != len {
                        return Err(format!(
                            "group '{}' is not aligned with its parent '{}': dimension \
                             '{dim}' has length {len} in '{}' but {parent_len} in '{parent_var}'",
                            display_group(node),
                            display_group(parent),
                            var.path
                        ));
                    }
                }
            }
        }
        Ok(())
    }
}

/// The tables of one node, plus the arrays left out because they declare no
/// dimension names.
#[derive(Debug)]
pub struct NodeTables {
    pub tables: Vec<Table>,
    pub unnamed: Vec<String>,
    /// Why the node is not aligned with its ancestors, if it is not. `read_zarr`
    /// fails with this message; `read_zarr_tables` gives the node's rows no table
    /// name.
    pub misaligned: Option<String>,
}

/// The grid work units follow (decision 6): the largest chunk length in each
/// dimension over the variables read chunk by chunk. A sparse matrix has no
/// chunk grid on the table's shape: beside dense arrays it is densified per
/// plan chunk, and on its own it is read in blocks of entries. Without such
/// variables (a table of sparse matrices, or of unreadable arrays) the grid
/// is the shape.
fn plan_grid(table: &Table) -> Vec<u64> {
    table
        .variables
        .iter()
        .filter(|v| !matches!(v.storage, Storage::Sparse(_) | Storage::Unreadable { .. }))
        .map(|v| v.chunk_shape.clone())
        .reduce(|a, b| a.iter().zip(&b).map(|(&x, &y)| x.max(y)).collect())
        .unwrap_or_else(|| table.shape.clone())
}

/// A dimension coordinate: a 1-D array whose only dimension has its name.
/// An AnnData data frame's columns never are (see [`Variable::in_dataframe`]).
fn is_dim_coord(var: &Variable) -> bool {
    matches!(var.storage, Storage::Array)
        && !var.in_dataframe
        && var.shape.len() == 1
        && var
            .dims
            .as_ref()
            .is_some_and(|dims| dims.len() == 1 && dims[0] == basename(&var.path))
}

/// Fail unless some array lies in or below group `node` (the root always
/// exists), so that a mistyped `group_path=` is an error, not an empty result.
pub fn ensure_group_exists(
    store_path: &str,
    array_names: &[String],
    node: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let prefix = format!("{node}/");
    if node.is_empty() || array_names.iter().any(|name| name.starts_with(&prefix)) {
        return Ok(());
    }
    Err(format!(
        "'{store_path}': group '{}' not found; run read_zarr_tables('{store_path}') to list \
         the groups",
        display_group(node)
    )
    .into())
}

/// Table names for one node's tables, in order. A name is `None` where the
/// node has arrays of different shapes for the same dims, which `read_zarr`
/// cannot read as one table. It is also `None` for every group whose name equals
/// another's after case folding, because DuckDB identifiers ignore case.
/// xarray-sql raises on such a collision (`xarray_sql.df.resolve_table_names`);
/// here the rest of the node keeps its names, and `dims :=` still reads it.
pub fn table_names(tables: &[Table]) -> Vec<Option<String>> {
    let mut shapes: HashMap<&[String], usize> = HashMap::new();
    for table in tables {
        *shapes.entry(table.dims.as_slice()).or_default() += 1;
    }
    let names: Vec<Option<String>> = tables
        .iter()
        .map(|table| (shapes[table.dims.as_slice()] == 1).then(|| default_table_name(&table.dims)))
        .collect();
    let mut folded: HashMap<String, usize> = HashMap::new();
    for name in names.iter().flatten() {
        *folded.entry(fold_case(name)).or_default() += 1;
    }
    names
        .into_iter()
        .map(|name| name.filter(|n| folded[&fold_case(n)] == 1))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_helpers() {
        assert_eq!(normalize_group(" /a/b/ "), "a/b");
        assert_eq!(normalize_group("/"), "");
        assert_eq!(node_of("a/b/c"), "a/b");
        assert_eq!(node_of("c"), "");
        assert_eq!(join("", "x"), "x");
        assert_eq!(join("a", "x"), "a/x");
        assert_eq!(self_and_ancestors("a/b"), vec!["a/b", "a", ""]);
        assert_eq!(self_and_ancestors(""), vec![""]);
        assert_eq!(display_group(""), "/");
        assert_eq!(schema_name(""), "main");
        assert_eq!(schema_name("simulation/fine"), "simulation/fine");
    }

    #[test]
    fn table_names_follow_xarray_sql() {
        let s = |v: &[&str]| v.iter().map(|d| d.to_string()).collect::<Vec<_>>();
        assert_eq!(
            default_table_name(&s(&["time", "lat", "lon"])),
            "time_lat_lon"
        );
        assert_eq!(default_table_name(&[]), "scalar");

        let table = |dims: &[&str], len: u64| Table {
            dims: s(dims),
            shape: vec![len; dims.len()],
            chunk_shape: vec![len; dims.len()],
            coords: vec![],
            variables: vec![],
        };
        let names = table_names(&[table(&["x"], 2), table(&["y"], 3), table(&["y"], 4)]);
        assert_eq!(names, vec![Some("x".to_string()), None, None]);

        // A case collision leaves only the colliding groups unnamed.
        let names = table_names(&[table(&["X"], 2), table(&["x"], 3), table(&["y"], 4)]);
        assert_eq!(names, vec![None, None, Some("y".to_string())]);
    }
}
