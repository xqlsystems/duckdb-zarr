//! What a table function call selects in a store (design decision 8).
//!
//! `read_zarr`, `read_zarr_tables` and `read_zarr_metadata` take the same two
//! selectors: `array_path :=` for one array and `group_path :=` for one node
//! (Zarr group), each with a quoted alias (`"array" :=`, `"group" :=`; `GROUP`
//! is a DuckDB keyword, so the quotes are required). At most one may be given.

use duckdb::core::{LogicalTypeHandle, LogicalTypeId};
use duckdb::vtab::BindInfo;

use crate::zarr_reader::tree;

/// The part of a store a table function call reads or lists.
#[derive(Debug, Clone, PartialEq)]
pub enum Selection {
    /// Neither selector: `read_zarr` reads the root node, and the catalog
    /// functions list every node.
    Store,
    /// `group_path :=`: one node, as a normalized group path (`""` is the
    /// root). Its arrays are read as tables.
    Node(String),
    /// `array_path :=`: one array, by store-relative path, read on its own.
    Array(String),
}

impl Selection {
    pub fn from_bind(bind: &BindInfo) -> Result<Self, Box<dyn std::error::Error>> {
        let array = parameter(bind, "array_path", "array")?;
        let group = parameter(bind, "group_path", "group")?;
        match (array, group) {
            (Some(_), Some(_)) => Err("use either group_path= or array_path=, not both; \
                                       array_path= is relative to the store root"
                .into()),
            (Some(array), None) => Ok(Self::Array(array.trim().trim_matches('/').to_string())),
            (None, Some(group)) => Ok(Self::Node(tree::normalize_group(&group))),
            (None, None) => Ok(Self::Store),
        }
    }

    /// The named parameters that [`Selection::from_bind`] reads.
    pub fn named_parameters() -> Vec<(String, LogicalTypeHandle)> {
        ["array_path", "array", "group_path", "group"]
            .into_iter()
            .map(|name| (name.to_string(), LogicalTypeId::Varchar.into()))
            .collect()
    }
}

/// The value of the named parameter `name` or of its alias, never both.
fn parameter(
    bind: &BindInfo,
    name: &str,
    alias: &str,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    match (
        bind.get_named_parameter(name),
        bind.get_named_parameter(alias),
    ) {
        (Some(_), Some(_)) => Err(format!("use either {name}= or \"{alias}\"=, not both").into()),
        (value, alias_value) => Ok(value.or(alias_value).map(|v| v.to_string())),
    }
}
