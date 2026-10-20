//! AnnData stores read as an xarray `DataTree` (design decision 9).
//!
//! An AnnData object is a group with `encoding-type: anndata`: the root of an
//! AnnData store, or a group inside another store, such as a SpatialData
//! table at `tables/<name>`. Its elements are found by their paths relative to
//! that group. Groups outside every AnnData object read as plain Zarr.
//!
//! AnnData writes no dimension names, and stores some variables as a group of
//! arrays: a categorical is `codes` plus `categories`, a nullable column is
//! `values` plus `mask`, and a sparse matrix is `data`, `indices` and `indptr`
//! (<https://anndata.readthedocs.io/en/stable/fileformat-prose.html>). This
//! module supplies what the generic tree needs to read such a store:
//! dimension names for each AnnData element, the encoded groups to read as
//! single variables, the `obs` and `var` data frames moved into the root node,
//! and each data frame's index as the coordinate of its dimension. Everything
//! else (one schema per group, tables named by dims, inherited coordinates) is
//! decision 8 unchanged.

use std::collections::HashMap;

use super::meta::{first_chunk_shape, open_array, synthesize_dim_names, ZarrStore};
use super::tree::{basename, display_group, join, node_of, self_and_ancestors};
use super::types::{SparseMatrix, Storage, Table, Variable};

/// The dimension of AnnData's rows (cells): `n_obs` long.
pub const OBS: &str = "obs";
/// The dimension of AnnData's columns (genes): `n_vars` long.
pub const VAR: &str = "var";
/// The columns of `raw`, which may differ from `var`.
pub const RAW_VAR: &str = "raw_var";

/// Group encodings that hold one variable.
const VARIABLE_ENCODINGS: &[&str] = &[
    "categorical",
    "nullable-integer",
    "nullable-boolean",
    "nullable-string-array",
    "csr_matrix",
    "csc_matrix",
];

/// Group encodings whose children are AnnData elements that get dimension names.
const CONTAINER_ENCODINGS: &[&str] = &["anndata", "raw", "dict", "dataframe"];

/// The attributes of the group at `path` (`""` is the root), or `None` if
/// there is no such group.
fn read_group_attrs(
    store: &ZarrStore,
    path: &str,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    let group = zarrs::group::Group::open(store.clone(), &format!("/{path}")).ok()?;
    Some(group.attributes().clone())
}

fn attr_str(attrs: &serde_json::Map<String, serde_json::Value>, key: &str) -> Option<String> {
    attrs.get(key)?.as_str().map(str::to_string)
}

/// The `encoding-type` attribute of the group at `path` (`""` is the root), or
/// `None` if there is no such group or it has no encoding.
pub fn read_encoding_type(store: &ZarrStore, path: &str) -> Option<String> {
    attr_str(&read_group_attrs(store, path)?, "encoding-type")
}

/// The groups of a store's AnnData objects that matter for reading them.
#[derive(Debug, Default)]
pub struct Layout {
    /// The `encoding-type` of each AnnData object's group and of the groups
    /// inside it, keyed by path (`""` is the root). Groups outside every
    /// AnnData object are not listed.
    pub encodings: HashMap<String, String>,
    /// For each `dataframe` group, the name of its index column (its `_index`
    /// attribute; usually `_index`).
    pub indexes: HashMap<String, String>,
    /// For each `dataframe` group, its columns in order (its `column-order`
    /// attribute), so that tables list them as `adata.obs` does.
    pub column_orders: HashMap<String, Vec<String>>,
    /// For each `csr_matrix` / `csc_matrix` group, its `shape` attribute, kept
    /// so that the group's metadata is read once.
    pub sparse_shapes: HashMap<String, Option<Vec<u64>>>,
}

impl Layout {
    pub fn encoding_of(&self, group: &str) -> Option<String> {
        self.encodings.get(group).cloned()
    }

    /// The AnnData object that `node` is, or lies in: the nearest group at or
    /// above it with `encoding-type: anndata`. `None` outside every AnnData
    /// object.
    pub fn object_of(&self, node: &str) -> Option<String> {
        object_of(node, |g| self.encoding_of(g))
    }

    /// Whether `path` is in an AnnData object's `uns`, its unstructured
    /// metadata. It has no axes, so none of it is read as tables, and it is
    /// never opened during enumeration: it often holds arrays zarrs cannot
    /// open (record arrays, awkward arrays). `array_path=` still reads its
    /// arrays.
    pub fn is_unstructured(&self, path: &str) -> bool {
        self.object_of(node_of(path))
            .is_some_and(|object| is_unstructured(relative(path, &object)))
    }

    /// Where a variable sorts among the columns of its table: by its data
    /// frame's `column-order`, then by path for anything the order does not
    /// list (which keeps other variables in path order).
    pub fn column_rank(&self, path: &str) -> (usize, String) {
        let rank = self
            .column_orders
            .get(node_of(path))
            .and_then(|order| order.iter().position(|c| c == basename(path)));
        (rank.unwrap_or(usize::MAX), path.to_string())
    }

    /// Whether the group at `path` holds one encoded variable (a categorical,
    /// a nullable column, a sparse matrix).
    pub fn is_variable(&self, path: &str) -> bool {
        !path.is_empty()
            && self
                .encoding_of(path)
                .is_some_and(|e| is_variable_encoding(&e))
    }

    /// The encoded variable that `path` is part of: `path` itself if it is an
    /// encoded group, else the outermost encoded group above it.
    pub fn variable_of(&self, path: &str) -> Option<String> {
        self_and_ancestors(path)
            .into_iter()
            .rev()
            .find(|g| self.is_variable(g))
    }

    /// The node whose tables hold the element at `path` (an array or an
    /// encoded group): the group that holds it, with the `obs`, `var` and
    /// `raw/var` data frames folded into their parent ([`logical_node`]).
    pub fn node_of(&self, path: &str) -> String {
        logical_node(node_of(path), |g| self.encoding_of(g))
    }

    /// Whether the element at `path` is a column of a data frame.
    pub fn is_frame_column(&self, path: &str) -> bool {
        self.encoding_of(node_of(path)).as_deref() == Some("dataframe")
    }

    /// The node whose tables hold the array at `path`: the node of the variable
    /// it belongs to, which is the encoded group around it or else the array
    /// itself.
    pub fn node_of_array(&self, path: &str) -> String {
        self.node_of(&self.variable_of(path).unwrap_or_else(|| path.to_string()))
    }
}

/// In an AnnData store, `group_path=` must name a node of tables. Naming the
/// `obs`, `var` or `raw/var` data frame is an error that points at the group
/// its columns belong to, and naming an encoded variable (a sparse matrix, a
/// categorical) or a group inside one is an error that points at its table and
/// at `array_path=`.
pub fn check_group_path(
    store_path: &str,
    layout: Option<&Layout>,
    node: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let Some(layout) = layout else {
        return Ok(());
    };
    if let Some(variable) = layout.variable_of(node) {
        let parent = layout.node_of(&variable);
        return Err(format!(
            "'{store_path}': in an AnnData store, '{}' is part of the variable '{variable}', \
             not a group of tables. It is a column of a table in group '{}'; read it alone \
             with array_path := '{variable}'",
            display_group(node),
            display_group(&parent)
        )
        .into());
    }
    let parent = logical_node(node, |g| layout.encoding_of(g));
    if parent == node {
        return Ok(());
    }
    let axis = if basename(node) == VAR && basename(&parent) == "raw" {
        RAW_VAR
    } else {
        basename(node)
    };
    Err(format!(
        "'{store_path}': in an AnnData store, the columns of '{}' are in the group '{}': \
         read_zarr('{store_path}', group_path := '{parent}', table_name := '{axis}')",
        display_group(node),
        display_group(&parent)
    )
    .into())
}

/// The layout of a store's AnnData objects, or `None` for a store that has
/// none. A store whose root is an AnnData object costs one metadata read more
/// than another store; otherwise each group that holds arrays is read once to
/// find AnnData objects inside the store (a SpatialData table), and the groups
/// inside each object once more.
pub fn layout(store: &ZarrStore, array_names: &[String]) -> Option<Layout> {
    let objects = find_objects(store, array_names);
    if objects.is_empty() {
        return None;
    }
    let mut layout = Layout::default();
    for object in objects {
        layout.encodings.insert(object, "anndata".to_string());
    }
    for name in array_names {
        let Some(object) = layout.object_of(node_of(name)) else {
            continue;
        };
        if layout.is_unstructured(name) {
            continue;
        }
        for group in self_and_ancestors(node_of(name)) {
            if group == object {
                break;
            }
            if layout.encodings.contains_key(&group) {
                continue;
            }
            let attrs = read_group_attrs(store, &group).unwrap_or_default();
            let encoding = attr_str(&attrs, "encoding-type").unwrap_or_default();
            if encoding == "csr_matrix" || encoding == "csc_matrix" {
                let shape = attrs
                    .get("shape")
                    .and_then(serde_json::Value::as_array)
                    .and_then(|dims| dims.iter().map(serde_json::Value::as_u64).collect());
                layout.sparse_shapes.insert(group.clone(), shape);
            }
            if encoding == "dataframe" {
                if let Some(index) = attr_str(&attrs, "_index") {
                    layout.indexes.insert(group.clone(), index);
                }
                if let Some(serde_json::Value::Array(columns)) = attrs.get("column-order") {
                    let columns = columns
                        .iter()
                        .filter_map(|c| c.as_str().map(str::to_string))
                        .collect();
                    layout.column_orders.insert(group.clone(), columns);
                }
            }
            layout.encodings.insert(group, encoding);
        }
    }
    Some(layout)
}

/// The groups with `encoding-type: anndata` that hold arrays: the root alone
/// if it is one, else the outermost such groups. Groups inside an AnnData
/// object are not searched, so its `uns` is never read.
fn find_objects(store: &ZarrStore, array_names: &[String]) -> Vec<String> {
    let is_object = |group: &str| read_encoding_type(store, group).as_deref() == Some("anndata");
    if is_object("") {
        return vec![String::new()];
    }
    let mut groups: Vec<String> = array_names
        .iter()
        .flat_map(|name| self_and_ancestors(node_of(name)))
        .filter(|group| !group.is_empty())
        .collect();
    // Parents before children, so that an object's groups are skipped.
    groups.sort_by_key(|group| (group.matches('/').count(), group.clone()));
    groups.dedup();
    let mut objects: Vec<String> = Vec::new();
    for group in groups {
        let inside = objects
            .iter()
            .any(|object| group.starts_with(&format!("{object}/")));
        if !inside && is_object(&group) {
            objects.push(group);
        }
    }
    objects
}

/// The AnnData object that `node` is, or lies in: the nearest group at or
/// above it whose `encoding-type` (from `encoding_of`) is `anndata`.
pub fn object_of(node: &str, encoding_of: impl Fn(&str) -> Option<String>) -> Option<String> {
    self_and_ancestors(node)
        .into_iter()
        .find(|group| encoding_of(group).as_deref() == Some("anndata"))
}

/// `path` relative to the group `object` that contains it: `tables/t/obs` in
/// `tables/t` is `obs`, and the object itself is `""`.
fn relative<'a>(path: &'a str, object: &str) -> &'a str {
    if object.is_empty() {
        return path;
    }
    match path.strip_prefix(object) {
        Some("") => "",
        Some(rest) => rest.strip_prefix('/').unwrap_or(path),
        None => path,
    }
}

/// The node an element in `physical_node` belongs to. The columns of an
/// AnnData object's `obs` and `var` data frames belong to the object's own
/// node, beside `X`, and those of `raw/var` to `raw`, beside `raw/X`. These
/// data frames annotate the axes of the matrix next to them; as nodes of their
/// own they would give tables named `obs.obs` and `var.var`. Every other group
/// is its own node.
pub fn logical_node(physical_node: &str, encoding_of: impl Fn(&str) -> Option<String>) -> String {
    let in_object = object_of(node_of(physical_node), &encoding_of).is_some_and(|object| {
        matches!(relative(physical_node, &object), "obs" | "var" | "raw/var")
    });
    let folded = in_object && encoding_of(physical_node).as_deref() == Some("dataframe");
    if folded {
        node_of(physical_node).to_string()
    } else {
        physical_node.to_string()
    }
}

/// The row and column dimensions of `obsp` (cell by cell) and `varp`.
pub const OBS_I: &str = "obs_i";
pub const OBS_J: &str = "obs_j";
pub const VAR_I: &str = "var_i";
pub const VAR_J: &str = "var_j";

/// Dimensions that take their coordinate from another dimension: both axes of
/// `obsp` show cell names, and both axes of `varp` gene names.
pub const COORDINATE_ALIASES: &[(&str, &str)] =
    &[(OBS_I, OBS), (OBS_J, OBS), (VAR_I, VAR), (VAR_J, VAR)];

/// Whether `path`, relative to its AnnData object, is in `uns` (see
/// [`Layout::is_unstructured`]).
fn is_unstructured(path: &str) -> bool {
    path == "uns" || path.starts_with("uns/")
}

/// Whether `encoding` is a group encoding that holds one variable.
pub fn is_variable_encoding(encoding: &str) -> bool {
    VARIABLE_ENCODINGS.contains(&encoding)
}

/// How the variable in the group at `path`, with the given `encoding-type`,
/// is stored, or `None` if the encoding does not hold one variable.
pub fn variable_storage(
    layout: &Layout,
    path: &str,
    encoding: &str,
) -> Result<Option<Storage>, Box<dyn std::error::Error>> {
    Ok(Some(match encoding {
        "categorical" => Storage::Categorical {
            codes: join(path, "codes"),
            categories: join(path, "categories"),
        },
        "nullable-integer" | "nullable-boolean" | "nullable-string-array" => Storage::Nullable {
            values: join(path, "values"),
            mask: join(path, "mask"),
        },
        "csr_matrix" | "csc_matrix" => {
            let shape = layout
                .sparse_shapes
                .get(path)
                .cloned()
                .flatten()
                .ok_or_else(|| format!("sparse matrix '{path}' has no valid `shape` attribute"))?;
            Storage::Sparse(SparseMatrix {
                major_axis: if encoding == "csr_matrix" { 0 } else { 1 },
                data: join(path, "data"),
                indices: join(path, "indices"),
                indptr: join(path, "indptr"),
                shape,
            })
        }
        _ => return Ok(None),
    }))
}

/// The variable that the AnnData encoded group at `path` holds (a sparse
/// matrix, a categorical or a nullable column), or `None` if `encoding` is not
/// one of those. Shape and chunks come from the array that holds the values; a
/// sparse matrix is read by ranges of its major axis, so its chunk shape is
/// its shape. An error opening the values array is returned as is, for the
/// caller to tell an unsupported array from a failure.
pub fn encoded_variable(
    store: &ZarrStore,
    layout: &Layout,
    path: &str,
    encoding: &str,
) -> Result<Option<Variable>, Box<dyn std::error::Error>> {
    let Some(storage) = variable_storage(layout, path, encoding)? else {
        return Ok(None);
    };
    let (shape, chunk_shape) = match &storage {
        Storage::Sparse(matrix) => (matrix.shape.clone(), matrix.shape.clone()),
        _ => {
            let values = storage
                .values_array(path)
                .expect("an AnnData group variable");
            let arr = open_array(store, values)?;
            (arr.shape().to_vec(), first_chunk_shape(&arr)?)
        }
    };
    Ok(Some(Variable {
        path: path.to_string(),
        node: layout.node_of(path),
        dims: axis_names(path, shape.len(), |g| layout.encoding_of(g)),
        shape,
        chunk_shape,
        attrs: Default::default(),
        storage,
        in_dataframe: layout.is_frame_column(path),
    }))
}

/// The one-variable table that `array_path=` reads for an AnnData encoded
/// variable (a sparse matrix, a categorical or a nullable column), or `None` if
/// `path` is not one, or lies inside another. Its dimensions are named as in
/// its table but read as integer positions, as for any one array read alone:
/// one variable has no index to take names from.
pub fn variable_table(
    store: &ZarrStore,
    layout: &Layout,
    path: &str,
) -> Result<Option<Table>, Box<dyn std::error::Error>> {
    if layout.variable_of(path).as_deref() != Some(path) {
        return Ok(None);
    }
    let encoding = layout.encoding_of(path).unwrap_or_default();
    let Some(mut variable) = encoded_variable(store, layout, path, &encoding)? else {
        return Ok(None);
    };
    let dims = variable
        .dims
        .clone()
        .unwrap_or_else(|| synthesize_dim_names(variable.shape.len()));
    variable.dims = Some(dims.clone());
    Ok(Some(Table {
        dims,
        shape: variable.shape.clone(),
        chunk_shape: variable.chunk_shape.clone(),
        coords: Vec::new(),
        variables: vec![variable],
    }))
}

/// Dimension names for the AnnData element at `path` with `ndim` dimensions,
/// or `None` if the element has no fixed axes (`uns`, anything inside an
/// encoded group, or an array outside every AnnData object). `encoding_of`
/// gives a group's `encoding-type`. Element paths are relative to the
/// AnnData object's group: in a SpatialData table at `tables/t`, `X` is
/// `tables/t/X`.
///
/// | Element | Dimensions |
/// |---|---|
/// | `X`, `layers/*` | `obs`, `var` |
/// | `obs/*`, `obsm/<df>/*` | `obs` |
/// | `var/*`, `varm/<df>/*` | `var` |
/// | `obsm/<k>` | `obs`, `<k>_component` |
/// | `varm/<k>` | `var`, `<k>_component` |
/// | `obsp/*` | `obs_i`, `obs_j` |
/// | `varp/*` | `var_i`, `var_j` |
/// | `raw/X` | `obs`, `raw_var` |
/// | `raw/var/*` | `raw_var` |
/// | `raw/varm/<k>` | `raw_var`, `<k>_component` |
pub fn axis_names(
    path: &str,
    ndim: usize,
    encoding_of: impl Fn(&str) -> Option<String>,
) -> Option<Vec<String>> {
    let node = node_of(path);
    let object = object_of(node, &encoding_of)?;
    // Every group between the object and the element must be a container.
    // This rules out the arrays inside an encoded variable and unknown
    // encodings such as awkward arrays.
    for group in self_and_ancestors(node) {
        if group == object {
            break;
        }
        if !encoding_of(&group).is_some_and(|e| CONTAINER_ENCODINGS.contains(&e.as_str())) {
            return None;
        }
    }
    // Groups named relative to the object.
    let is = |group: &str, encoding: &str| {
        encoding_of(&join(&object, group)).as_deref() == Some(encoding)
    };
    let node = relative(node, &object);

    let name = basename(path);
    // An obsm/varm entry's other axes are its components: `X_umap` is
    // (obs, X_umap_component); a 3-D entry has `<k>_component_1`, `_2`.
    let with_dims = |axis: &str| {
        let mut dims = vec![axis.to_string()];
        match ndim {
            2 => dims.push(format!("{name}_component")),
            _ => dims.extend((1..ndim).map(|i| format!("{name}_component_{i}"))),
        }
        dims
    };
    let parts: Vec<&str> = if node.is_empty() {
        Vec::new()
    } else {
        node.split('/').collect()
    };
    let s = |v: &[&str]| v.iter().map(|d| d.to_string()).collect::<Vec<_>>();
    let dims = match (parts.as_slice(), ndim) {
        ([], 2) if name == "X" => s(&[OBS, VAR]),
        (["layers"], 2) => s(&[OBS, VAR]),
        (["obs"], 1) if is("obs", "dataframe") => s(&[OBS]),
        (["var"], 1) if is("var", "dataframe") => s(&[VAR]),
        (["obsm"], 1..) => with_dims(OBS),
        (["varm"], 1..) => with_dims(VAR),
        (["obsm", df], 1) if is(&join("obsm", df), "dataframe") => s(&[OBS]),
        (["varm", df], 1) if is(&join("varm", df), "dataframe") => s(&[VAR]),
        (["obsp"], 2) => s(&[OBS_I, OBS_J]),
        (["varp"], 2) => s(&[VAR_I, VAR_J]),
        (["raw"], 2) if name == "X" => s(&[OBS, RAW_VAR]),
        (["raw", "var"], 1) if is("raw/var", "dataframe") => s(&[RAW_VAR]),
        (["raw", "varm"], 1..) => with_dims(RAW_VAR),
        _ => return None,
    };
    Some(dims)
}

/// [`axis_names`] for one array read with `array_path=`, reading the group
/// encodings it needs from the store.
pub fn axis_names_for_array(store: &ZarrStore, path: &str, ndim: usize) -> Option<Vec<String>> {
    axis_names(path, ndim, |group| read_encoding_type(store, group))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn groups(groups: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = groups
            .iter()
            .map(|(g, e)| (g.to_string(), e.to_string()))
            .collect();
        move |g: &str| map.get(g).cloned()
    }

    #[test]
    fn axis_names_follow_the_anndata_layout() {
        let enc = groups(&[
            ("", "anndata"),
            ("obs", "dataframe"),
            ("var", "dataframe"),
            ("obsm", "dict"),
            ("obsm/df", "dataframe"),
            ("obsp", "dict"),
            ("layers", "dict"),
            ("obs/cell_type", "categorical"),
            ("obsm/awk", "awkward-array"),
            ("uns", "dict"),
            ("raw", "raw"),
            ("raw/var", "dataframe"),
        ]);
        let names = |path: &str, ndim: usize| axis_names(path, ndim, &enc);
        assert_eq!(names("X", 2).unwrap(), ["obs", "var"]);
        assert_eq!(names("layers/counts", 2).unwrap(), ["obs", "var"]);
        assert_eq!(names("obs/n_genes", 1).unwrap(), ["obs"]);
        assert_eq!(names("var/_index", 1).unwrap(), ["var"]);
        assert_eq!(
            names("obsm/X_umap", 2).unwrap(),
            ["obs", "X_umap_component"]
        );
        assert_eq!(
            names("obsm/X_3d", 3).unwrap(),
            ["obs", "X_3d_component_1", "X_3d_component_2"]
        );
        assert_eq!(names("obsm/df/a", 1).unwrap(), ["obs"]);
        assert_eq!(names("obsp/distances", 2).unwrap(), ["obs_i", "obs_j"]);
        assert_eq!(names("raw/X", 2).unwrap(), ["obs", "raw_var"]);
        assert_eq!(names("raw/var/_index", 1).unwrap(), ["raw_var"]);

        // The arrays inside an encoded variable, unknown encodings, uns, and a
        // rank that does not fit get no names.
        assert_eq!(names("obs/cell_type/codes", 1), None);
        assert_eq!(names("obsm/awk/node0", 1), None);
        assert_eq!(names("uns/thing", 1), None);
        assert_eq!(names("X", 1), None);
        assert_eq!(names("obs/n_genes", 2), None);

        assert_eq!(logical_node("obs", &enc), "");
        assert_eq!(logical_node("raw/var", &enc), "raw");
        assert_eq!(logical_node("obsm/df", &enc), "obsm/df");
        assert_eq!(logical_node("layers", &enc), "layers");

        // Not an AnnData store.
        assert_eq!(axis_names("X", 2, groups(&[])), None);
    }

    #[test]
    fn an_anndata_object_inside_a_store_reads_like_one_at_the_root() {
        // A SpatialData table: the AnnData object is the group tables/t.
        let enc = groups(&[
            ("tables/t", "anndata"),
            ("tables/t/obs", "dataframe"),
            ("tables/t/obsm", "dict"),
            ("tables/t/raw", "raw"),
            ("tables/t/raw/var", "dataframe"),
        ]);
        let names = |path: &str, ndim: usize| axis_names(path, ndim, &enc);
        assert_eq!(names("tables/t/X", 2).unwrap(), ["obs", "var"]);
        assert_eq!(names("tables/t/obs/region", 1).unwrap(), ["obs"]);
        assert_eq!(
            names("tables/t/obsm/spatial", 2).unwrap(),
            ["obs", "spatial_component"]
        );
        assert_eq!(names("tables/t/raw/X", 2).unwrap(), ["obs", "raw_var"]);
        // Outside the object, nothing is AnnData.
        assert_eq!(names("tables/X", 2), None);
        assert_eq!(names("X", 2), None);
        assert_eq!(names("labels/obs/x", 1), None);

        assert_eq!(logical_node("tables/t/obs", &enc), "tables/t");
        assert_eq!(logical_node("tables/t/raw/var", &enc), "tables/t/raw");
        assert_eq!(logical_node("tables/t/obsm", &enc), "tables/t/obsm");
        assert_eq!(logical_node("tables/t", &enc), "tables/t");

        assert_eq!(relative("tables/t/obs", "tables/t"), "obs");
        assert_eq!(relative("tables/t", "tables/t"), "");
        assert_eq!(relative("obs", ""), "obs");
    }
}
