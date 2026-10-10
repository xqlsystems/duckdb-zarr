//! Nested Zarr groups as a tree of tables (design decision 8).
//!
//! Each Zarr group is a node, as in `xarray.DataTree`. A table is one dimension
//! group inside one node: arrays in different nodes never share a table, even
//! when their dimension names match. A node's tables use coordinate arrays from
//! the node itself or, failing that, from its nearest ancestor that has one.
//! Tables are named the way xarray-sql names them (`"_".join(dims)`), and each
//! node maps to one DuckDB schema (the root is `main`).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use super::anndata;
use super::meta::{
    cf_auxiliary_vars, declared_dims, first_chunk_shape, is_unsupported_array_error, open_array,
    raw_array_layout, ArrayFacts, ZarrStore,
};
use super::types::{DimGroup, VarEncoding};

/// One array in a node, with the metadata that grouping needs.
#[derive(Debug, Clone)]
pub struct NodeVar {
    /// Store-relative path, for example `simulation/fine/foo`.
    pub path: String,
    /// The node (group path) the variable belongs to: the group that holds it,
    /// except for AnnData's `obs`, `var` and `raw/var` columns, which belong
    /// to the parent group (see [`anndata::logical_node`]).
    pub node: String,
    /// Dimension names the store records, or `None` when it records none.
    pub dims: Option<Vec<String>>,
    pub shape: Vec<u64>,
    pub chunk_shape: Vec<u64>,
    pub attrs: serde_json::Map<String, serde_json::Value>,
    /// Why zarrs cannot open the array (unsupported data type or codec), if it
    /// cannot. Its shape and names then come from the raw metadata document,
    /// so that the table it belongs to can say it is missing.
    pub unreadable: Option<String>,
    /// How a variable stored as a group of arrays is decoded; `None` for a
    /// plain array. A sparse matrix's `chunk_shape` is its shape.
    pub encoding: Option<VarEncoding>,
}

/// The arrays of the nodes that were loaded, keyed by store-relative path.
#[derive(Debug, Default)]
pub struct StoreTree {
    pub vars: BTreeMap<String, NodeVar>,
    /// Coordinates by `(node, dim)`: the array that holds the values, and its
    /// length. A dimension coordinate (a 1-D array named after its only
    /// dimension) is registered in its node; so is an AnnData data frame's
    /// index, under its axis (`obs`, `var`).
    pub coords: HashMap<(String, String), (String, u64)>,
    /// Whether the store is an AnnData object (decision 9).
    pub is_anndata: bool,
    /// Arrays zarrs cannot open and whose metadata document cannot be read
    /// either. They are left out of every table; `read_zarr_metadata` lists
    /// them.
    pub unsupported: Vec<String>,
}

/// The node selected by a table function's `group_path=` parameter, or by its
/// quoted alias `"group"=` (xarray's name; `GROUP` is a DuckDB keyword, so the
/// quotes are required).
pub fn group_param(
    bind: &duckdb::vtab::BindInfo,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let path = bind.get_named_parameter("group_path");
    let alias = bind.get_named_parameter("group");
    if path.is_some() && alias.is_some() {
        return Err("use either group_path= or \"group\"=, not both".into());
    }
    Ok(path
        .or(alias)
        .map(|value| normalize_group(&value.to_string())))
}

/// The named parameters that [`group_param`] reads.
pub fn group_named_parameters() -> Vec<(String, duckdb::core::LogicalTypeHandle)> {
    use duckdb::core::LogicalTypeId;
    vec![
        ("group_path".to_string(), LogicalTypeId::Varchar.into()),
        ("group".to_string(), LogicalTypeId::Varchar.into()),
    ]
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
    /// ([`NodeVar::unreadable`]), or in `unsupported` if even its metadata
    /// document cannot be read; any other open error fails.
    pub fn load(
        store: &ZarrStore,
        array_names: &[String],
        nodes: Option<&HashSet<String>>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        // In an AnnData store, name each element's axes and read encoded
        // groups (categoricals, nullable columns, sparse matrices) as one
        // variable each (decision 9). Other stores skip this entirely.
        let layout = anndata::layout(store, array_names);
        let encodings = layout.as_ref().map(|l| &l.encodings);
        let encoding_of = |group: &str| layout.as_ref().and_then(|l| l.encoding_of(group));
        let logical = |path: &str| anndata::logical_node(node_of(path), encoding_of);
        let wanted = |path: &str| nodes.is_none_or(|nodes| nodes.contains(&logical(path)));
        let is_variable_group = |group: &str| {
            !group.is_empty()
                && encoding_of(group).is_some_and(|e| anndata::is_variable_encoding(&e))
        };
        // The arrays of an encoded variable belong to it, not to its group.
        let inside_variable = |path: &str| {
            self_and_ancestors(node_of(path))
                .iter()
                .any(|g| is_variable_group(g))
        };

        let mut tree = StoreTree {
            is_anndata: layout.is_some(),
            ..Default::default()
        };
        for name in array_names {
            if !wanted(name)
                || inside_variable(name)
                || (encodings.is_some() && anndata::is_unstructured(name))
            {
                continue;
            }
            let arr = match open_array(store, name) {
                Ok(arr) => arr,
                Err(err) if is_unsupported_array_error(err.as_ref()) => {
                    match raw_array_layout(store, name) {
                        Some((shape, dims)) => {
                            tree.vars.insert(
                                name.clone(),
                                NodeVar {
                                    path: name.clone(),
                                    node: logical(name),
                                    dims: dims.or_else(|| {
                                        anndata::axis_names(name, shape.len(), encoding_of)
                                    }),
                                    chunk_shape: shape.clone(),
                                    shape,
                                    attrs: Default::default(),
                                    unreadable: Some(err.to_string()),
                                    encoding: None,
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
            tree.vars.insert(
                name.clone(),
                NodeVar {
                    path: name.clone(),
                    node: logical(name),
                    dims,
                    shape,
                    chunk_shape,
                    attrs: arr.attributes().clone(),
                    unreadable: None,
                    encoding: None,
                },
            );
        }

        let Some(group_encodings) = encodings else {
            tree.register_dim_coords();
            return Ok(tree);
        };
        for (group, encoding) in group_encodings {
            if !is_variable_group(group) || !wanted(group) || inside_variable(group) {
                continue;
            }
            let Some(var_encoding) = anndata::variable_encoding(store, group, encoding)? else {
                continue;
            };
            // Shape and chunks come from the array that holds the values.
            let (shape, chunk_shape) = match &var_encoding {
                VarEncoding::Categorical { codes: values, .. }
                | VarEncoding::Nullable { values, .. } => match open_array(store, values) {
                    Ok(arr) => (arr.shape().to_vec(), first_chunk_shape(&arr)?),
                    Err(err) if is_unsupported_array_error(err.as_ref()) => {
                        tree.unsupported.push(group.clone());
                        continue;
                    }
                    Err(err) => return Err(err),
                },
                VarEncoding::Sparse(matrix) => (matrix.shape.clone(), matrix.shape.clone()),
            };
            let dims = anndata::axis_names(group, shape.len(), encoding_of);
            tree.vars.insert(
                group.clone(),
                NodeVar {
                    path: group.clone(),
                    node: logical(group),
                    dims,
                    shape,
                    chunk_shape,
                    attrs: Default::default(),
                    encoding: Some(var_encoding),
                    unreadable: None,
                },
            );
        }
        tree.register_dim_coords();
        if let Some(layout) = &layout {
            tree.register_anndata_indexes(layout);
        }
        Ok(tree)
    }

    /// Register every dimension coordinate in its node.
    fn register_dim_coords(&mut self) {
        for var in self.vars.values().filter(|v| is_dim_coord(v)) {
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
            let Some(var) = self.vars.get(&path) else {
                continue;
            };
            // A string array, or a nullable string array whose values are
            // never missing in an index.
            let values = match &var.encoding {
                None => path.clone(),
                Some(VarEncoding::Nullable { values, .. }) => values.clone(),
                Some(_) => continue,
            };
            let Some([dim]) = var.dims.as_deref() else {
                continue;
            };
            let key = (var.node.clone(), dim.clone());
            let len = var.shape[0];
            self.vars.remove(&path);
            self.coords.insert(key, (values, len));
        }
        let aliased: Vec<_> = self
            .coords
            .iter()
            .flat_map(|((node, dim), coord)| {
                anndata::COORDINATE_ALIASES
                    .iter()
                    .filter(move |(_, of)| of == dim)
                    .map(move |(alias, _)| ((node.clone(), alias.to_string()), coord.clone()))
            })
            .collect();
        self.coords.extend(aliased);
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
        let nodes: BTreeSet<&str> = self.vars.values().map(|v| v.node.as_str()).collect();
        nodes.into_iter().map(str::to_string).collect()
    }

    fn vars_in<'a>(&'a self, node: &'a str) -> impl Iterator<Item = &'a NodeVar> + 'a {
        self.vars.values().filter(move |v| v.node == node)
    }

    /// The tables of one node: arrays grouped by `(dims, shape)`, sorted by
    /// dims. Arrays in one table may be chunked differently; the table's
    /// `chunk_shape` is the largest chunk length in each dimension, the grid
    /// `read_zarr` plans work units on (decision 6). Two entries with the same
    /// dims mean arrays that share dimension names but not a shape; `read_zarr`
    /// cannot read those dims as one table. A node that is not aligned with its ancestors is reported in
    /// [`NodeGroups::misaligned`] rather than as an error, so that
    /// `read_zarr_groups` can still list the rest of the store.
    pub fn dim_groups(&self, node: &str) -> NodeGroups {
        let mut misaligned = self.check_alignment(node).err();
        let facts: Vec<ArrayFacts> = self
            .vars_in(node)
            .map(|v| (v.path.as_str(), &v.attrs, v.shape.as_slice()))
            .collect();
        let (aux_coords, bounds) = cf_auxiliary_vars(&facts);

        type GroupKey = (Vec<String>, Vec<u64>);
        let mut groups: HashMap<GroupKey, DimGroup> = HashMap::new();
        let mut unnamed = Vec::new();
        // Sparse matrices go last, so that each can join a dense table with the
        // same dims and shape (see below).
        let mut vars: Vec<&NodeVar> = self.vars_in(node).collect();
        vars.sort_by_key(|var| is_sparse(var));
        for var in vars {
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
            if let Some(group) = groups.get_mut(&key) {
                add_to_group(group, var);
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
            let mut group = DimGroup {
                dims: dims.clone(),
                shape: var.shape.clone(),
                chunk_shape: Vec::new(),
                data_var_names: Vec::new(),
                coords,
                encodings: HashMap::new(),
                unreadable: Vec::new(),
            };
            add_to_group(&mut group, var);
            groups.insert(key, group);
        }
        // A table of unreadable arrays only has no chunk grid to plan on.
        for group in groups.values_mut() {
            if group.chunk_shape.is_empty() {
                group.chunk_shape = group.shape.clone();
            }
        }

        // The plan grid is the largest chunk length per dim over the dense
        // arrays. A sparse matrix has no chunk grid of its own on this shape:
        // next to dense arrays it is densified per plan chunk, and on its own
        // it is read in blocks of entries.
        for group in groups.values_mut() {
            let dense_chunks = group
                .data_var_names
                .iter()
                .filter(|path| !matches!(group.encodings.get(*path), Some(VarEncoding::Sparse(_))))
                .map(|path| &self.vars[path].chunk_shape);
            let mut plan: Option<Vec<u64>> = None;
            for chunks in dense_chunks {
                plan = Some(match plan {
                    None => chunks.clone(),
                    Some(p) => p.iter().zip(chunks).map(|(&a, &b)| a.max(b)).collect(),
                });
            }
            if let Some(plan) = plan {
                group.chunk_shape = plan;
            }
        }

        let mut groups: Vec<DimGroup> = groups.into_values().collect();
        groups.sort_by(|a, b| {
            a.dims
                .cmp(&b.dims)
                .then_with(|| a.shape.cmp(&b.shape))
                .then_with(|| a.data_var_names.cmp(&b.data_var_names))
        });
        NodeGroups {
            groups,
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
        for var in self.vars_in(parent) {
            let Some(dims) = &var.dims else { continue };
            for (dim, &len) in dims.iter().zip(&var.shape) {
                parent_sizes.entry(dim).or_insert((len, &var.path));
            }
        }
        for var in self.vars_in(node) {
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
pub struct NodeGroups {
    pub groups: Vec<DimGroup>,
    pub unnamed: Vec<String>,
    /// Why the node is not aligned with its ancestors, if it is not. `read_zarr`
    /// fails with this message; `read_zarr_groups` gives the node's rows no table
    /// name.
    pub misaligned: Option<String>,
}

/// Add `var` to its table: as a data variable whose chunks widen the plan
/// grid (decision 6), or, if zarrs cannot open it, to the table's unreadable
/// arrays.
fn add_to_group(group: &mut DimGroup, var: &NodeVar) {
    if let Some(err) = &var.unreadable {
        group.unreadable.push((var.path.clone(), err.clone()));
        return;
    }
    group.data_var_names.push(var.path.clone());
    if let Some(encoding) = &var.encoding {
        group.encodings.insert(var.path.clone(), encoding.clone());
    }
    if group.chunk_shape.is_empty() {
        group.chunk_shape = var.chunk_shape.clone();
    } else {
        for (plan, &c) in group.chunk_shape.iter_mut().zip(&var.chunk_shape) {
            *plan = (*plan).max(c);
        }
    }
}

fn is_sparse(var: &NodeVar) -> bool {
    matches!(var.encoding, Some(VarEncoding::Sparse(_)))
}

/// A dimension coordinate: a 1-D array whose only dimension has its name.
fn is_dim_coord(var: &NodeVar) -> bool {
    var.encoding.is_none()
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
        "'{store_path}': group '{}' not found; run read_zarr_groups('{store_path}') to list \
         the groups",
        display_group(node)
    )
    .into())
}

/// Table names for one node's dim groups, in order. A name is `None` where the
/// node has arrays of different shapes for the same dims, which `read_zarr`
/// cannot read as one table. It is also `None` for every group whose name equals
/// another's after case folding, because DuckDB identifiers ignore case.
/// xarray-sql raises on such a collision (`xarray_sql.df.resolve_table_names`);
/// here the rest of the node keeps its names, and `dims :=` still reads it.
pub fn table_names(groups: &[DimGroup]) -> Vec<Option<String>> {
    let mut shapes: HashMap<&[String], usize> = HashMap::new();
    for group in groups {
        *shapes.entry(group.dims.as_slice()).or_default() += 1;
    }
    let names: Vec<Option<String>> = groups
        .iter()
        .map(|group| (shapes[group.dims.as_slice()] == 1).then(|| default_table_name(&group.dims)))
        .collect();
    let mut folded: HashMap<String, usize> = HashMap::new();
    for name in names.iter().flatten() {
        *folded.entry(name.to_lowercase()).or_default() += 1;
    }
    names
        .into_iter()
        .map(|name| name.filter(|n| folded[&n.to_lowercase()] == 1))
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

        let group = |dims: &[&str], len: u64| DimGroup {
            dims: s(dims),
            shape: vec![len; dims.len()],
            chunk_shape: vec![len; dims.len()],
            data_var_names: vec![],
            coords: vec![],
            encodings: Default::default(),
            unreadable: vec![],
        };
        let names = table_names(&[group(&["x"], 2), group(&["y"], 3), group(&["y"], 4)]);
        assert_eq!(names, vec![Some("x".to_string()), None, None]);

        // A case collision leaves only the colliding groups unnamed.
        let names = table_names(&[group(&["X"], 2), group(&["x"], 3), group(&["y"], 4)]);
        assert_eq!(names, vec![None, None, Some("y".to_string())]);
    }
}
