//! Nested Zarr groups as a tree of tables (design decision 8).
//!
//! Each Zarr group is a node, as in `xarray.DataTree`. A table is one dimension
//! group inside one node: arrays in different nodes never share a table, even
//! when their dimension names match. A node's tables use coordinate arrays from
//! the node itself or, failing that, from its nearest ancestor that has one.
//! Tables are named the way xarray-sql names them (`"_".join(dims)`), and each
//! node maps to one DuckDB schema (the root is `main`).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use super::meta::{
    cf_auxiliary_vars, declared_dims, first_chunk_shape, is_unsupported_array_error, open_array,
    raw_array_layout, ArrayFacts, ZarrStore,
};
use super::types::DimGroup;

/// One array in a node, with the metadata that grouping needs.
#[derive(Debug, Clone)]
pub struct NodeVar {
    /// Store-relative path, for example `simulation/fine/foo`.
    pub path: String,
    /// Dimension names the store records, or `None` when it records none.
    pub dims: Option<Vec<String>>,
    pub shape: Vec<u64>,
    pub chunk_shape: Vec<u64>,
    pub attrs: serde_json::Map<String, serde_json::Value>,
    /// Why zarrs cannot open the array (unsupported data type or codec), if it
    /// cannot. Its shape and names then come from the raw metadata document,
    /// so that the table it belongs to can say it is missing.
    pub unreadable: Option<String>,
}

/// The arrays of the nodes that were loaded, keyed by store-relative path.
#[derive(Debug, Default)]
pub struct StoreTree {
    pub vars: BTreeMap<String, NodeVar>,
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
        let mut tree = StoreTree::default();
        for name in array_names {
            if nodes.is_some_and(|nodes| !nodes.contains(node_of(name))) {
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
                                    dims,
                                    chunk_shape: shape.clone(),
                                    shape,
                                    attrs: Default::default(),
                                    unreadable: Some(err.to_string()),
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
            tree.vars.insert(
                name.clone(),
                NodeVar {
                    path: name.clone(),
                    dims: declared_dims(store, &arr, name),
                    shape,
                    chunk_shape,
                    attrs: arr.attributes().clone(),
                    unreadable: None,
                },
            );
        }
        Ok(tree)
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
        let nodes: BTreeSet<&str> = self.vars.keys().map(|p| node_of(p)).collect();
        nodes.into_iter().map(str::to_string).collect()
    }

    fn vars_in<'a>(&'a self, node: &'a str) -> impl Iterator<Item = &'a NodeVar> + 'a {
        self.vars.values().filter(move |v| node_of(&v.path) == node)
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
        for var in self.vars_in(node) {
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
            let key = (dims.clone(), var.shape.clone());
            if let Some(group) = groups.get_mut(&key) {
                add_to_group(group, var);
                continue;
            }
            let mut coord_var_names = Vec::new();
            for (dim, &len) in dims.iter().zip(&var.shape) {
                match self.find_coord(node, dim, len) {
                    Ok(Some(coord)) => coord_var_names.push(coord),
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
                coord_var_names,
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

    /// The coordinate array for `dim` as seen from `node`: `<node>/<dim>` if it
    /// is a 1-D array along `dim`, else the same lookup in each ancestor, nearest
    /// first, as `DataTree` inherits coordinates. An inherited coordinate whose
    /// length differs from `len` is an error, as in xarray.
    fn find_coord(&self, node: &str, dim: &str, len: u64) -> Result<Option<String>, String> {
        for ancestor in self_and_ancestors(node) {
            let candidate = join(&ancestor, dim);
            let Some(var) = self.vars.get(&candidate) else {
                continue;
            };
            if !is_dim_coord(var) {
                continue;
            }
            if var.shape[0] != len {
                return Err(format!(
                    "group '{}' is not aligned with its ancestors: dimension '{dim}' has \
                     length {len}, but coordinate '{candidate}' has length {}",
                    display_group(node),
                    var.shape[0]
                ));
            }
            return Ok(Some(candidate));
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
    if group.chunk_shape.is_empty() {
        group.chunk_shape = var.chunk_shape.clone();
    } else {
        for (plan, &c) in group.chunk_shape.iter_mut().zip(&var.chunk_shape) {
            *plan = (*plan).max(c);
        }
    }
}

/// A dimension coordinate: a 1-D array whose only dimension has its name.
fn is_dim_coord(var: &NodeVar) -> bool {
    var.shape.len() == 1
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
            coord_var_names: vec![],
            unreadable: vec![],
        };
        let names = table_names(&[group(&["x"], 2), group(&["y"], 3), group(&["y"], 4)]);
        assert_eq!(names, vec![Some("x".to_string()), None, None]);

        // A case collision leaves only the colliding groups unnamed.
        let names = table_names(&[group(&["X"], 2), group(&["x"], 3), group(&["y"], 4)]);
        assert_eq!(names, vec![None, None, Some("y".to_string())]);
    }
}
