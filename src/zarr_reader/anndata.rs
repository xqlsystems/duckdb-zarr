//! AnnData stores read as an xarray `DataTree` (design decision 9).
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

use super::meta::ZarrStore;
use super::tree::{basename, display_group, join, node_of, self_and_ancestors};
use super::types::{SparseMatrix, VarEncoding};

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

/// The groups of an AnnData store that matter for reading it.
#[derive(Debug, Default)]
pub struct Layout {
    /// The `encoding-type` of each group, keyed by path (`""` is the root).
    pub encodings: HashMap<String, String>,
    /// For each `dataframe` group, the name of its index column (its `_index`
    /// attribute; usually `_index`).
    pub indexes: HashMap<String, String>,
}

impl Layout {
    pub fn encoding_of(&self, group: &str) -> Option<String> {
        self.encodings.get(group).cloned()
    }

    /// The node whose tables hold the array at `path`: the node of the variable
    /// it belongs to, which is the outermost encoded group around it
    /// (categorical, nullable, sparse) or else the array itself.
    pub fn node_of_array(&self, path: &str) -> String {
        let variable = self_and_ancestors(node_of(path))
            .into_iter()
            .rev()
            .find(|g| {
                !g.is_empty()
                    && self
                        .encoding_of(g)
                        .is_some_and(|e| is_variable_encoding(&e))
            })
            .unwrap_or_else(|| path.to_string());
        logical_node(node_of(&variable), |g| self.encoding_of(g))
    }
}

/// In an AnnData store, `group_path=` naming the `obs`, `var` or `raw/var`
/// data frame is an error that points at the group its columns belong to.
pub fn check_not_folded(
    store_path: &str,
    layout: Option<&Layout>,
    node: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let Some(layout) = layout else {
        return Ok(());
    };
    let parent = logical_node(node, |g| layout.encoding_of(g));
    if parent == node {
        return Ok(());
    }
    let axis = if node == "raw/var" { RAW_VAR } else { node };
    Err(format!(
        "'{store_path}': in an AnnData store, the columns of '{}' are in the group '{}': \
         read_zarr('{store_path}', group_path := '{parent}', dims := ['{axis}'])",
        display_group(node),
        display_group(&parent)
    )
    .into())
}

/// The layout of an AnnData store, or `None` for a store whose root is not an
/// AnnData object, so that other stores pay for no extra metadata reads.
pub fn layout(store: &ZarrStore, array_names: &[String]) -> Option<Layout> {
    if read_encoding_type(store, "")?.as_str() != "anndata" {
        return None;
    }
    let mut layout = Layout::default();
    layout
        .encodings
        .insert(String::new(), "anndata".to_string());
    for name in array_names.iter().filter(|name| !is_unstructured(name)) {
        for group in self_and_ancestors(node_of(name)) {
            if group.is_empty() || layout.encodings.contains_key(&group) {
                continue;
            }
            let attrs = read_group_attrs(store, &group).unwrap_or_default();
            let encoding = attr_str(&attrs, "encoding-type").unwrap_or_default();
            if encoding == "dataframe" {
                if let Some(index) = attr_str(&attrs, "_index") {
                    layout.indexes.insert(group.clone(), index);
                }
            }
            layout.encodings.insert(group, encoding);
        }
    }
    Some(layout)
}

/// The node an element in `physical_node` belongs to. The columns of the
/// `obs` and `var` data frames belong to the root, beside `X`, and those of
/// `raw/var` to `raw`, beside `raw/X`. These data frames annotate the axes of
/// the matrix next to them; as nodes of their own they would give tables named
/// `obs.obs` and `var.var`. Every other group is its own node.
pub fn logical_node(physical_node: &str, encoding_of: impl Fn(&str) -> Option<String>) -> String {
    let folded = matches!(physical_node, "obs" | "var" | "raw/var")
        && encoding_of(physical_node).as_deref() == Some("dataframe");
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

/// Whether `path` is in `uns`, AnnData's unstructured metadata. It has no axes,
/// so none of it is read as tables, and it is never opened during
/// enumeration: it often holds arrays zarrs cannot open (record arrays,
/// awkward arrays). `array_path=` still reads its arrays.
pub fn is_unstructured(path: &str) -> bool {
    path == "uns" || path.starts_with("uns/")
}

/// Whether `encoding` is a group encoding that holds one variable.
pub fn is_variable_encoding(encoding: &str) -> bool {
    VARIABLE_ENCODINGS.contains(&encoding)
}

/// The variable stored in the group at `path` with the given encoding.
pub fn variable_encoding(
    store: &ZarrStore,
    path: &str,
    encoding: &str,
) -> Result<Option<VarEncoding>, Box<dyn std::error::Error>> {
    Ok(Some(match encoding {
        "categorical" => VarEncoding::Categorical {
            codes: join(path, "codes"),
            categories: join(path, "categories"),
        },
        "nullable-integer" | "nullable-boolean" | "nullable-string-array" => {
            VarEncoding::Nullable {
                values: join(path, "values"),
                mask: join(path, "mask"),
            }
        }
        "csr_matrix" | "csc_matrix" => {
            let group = zarrs::group::Group::open(store.clone(), &format!("/{path}"))?;
            let shape = group
                .attributes()
                .get("shape")
                .and_then(serde_json::Value::as_array)
                .and_then(|dims| dims.iter().map(serde_json::Value::as_u64).collect())
                .ok_or_else(|| format!("sparse matrix '{path}' has no valid `shape` attribute"))?;
            VarEncoding::Sparse(SparseMatrix {
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

/// Dimension names for the AnnData element at `path` with `ndim` dimensions,
/// or `None` if the element has no fixed axes (`uns`, anything inside an
/// encoded group, or a store that is not AnnData). `encoding_of` gives a
/// group's `encoding-type`.
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
    let is = |group: &str, encoding: &str| encoding_of(group).as_deref() == Some(encoding);
    if !is("", "anndata") {
        return None;
    }
    // Every group between the root and the element must be a container. This
    // rules out the arrays inside an encoded variable and unknown encodings
    // such as awkward arrays.
    let node = node_of(path);
    for group in self_and_ancestors(node) {
        if !group.is_empty()
            && !encoding_of(&group).is_some_and(|e| CONTAINER_ENCODINGS.contains(&e.as_str()))
        {
            return None;
        }
    }

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
}
