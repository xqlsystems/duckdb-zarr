//! `read_zarr_views(store, database)`: the SQL that mounts a store's tables as
//! views, one statement per row (decision 8, Mounting).
//!
//! The C API cannot run DDL from inside a table function, so this function
//! returns the statements and the caller runs them in order, stopping on error:
//! an `ATTACH` of an in-memory database named `database`, a `CREATE SCHEMA` per
//! non-root node, and a `CREATE VIEW` per table that `read_zarr_tables` gives a name.
//! `view_names := MAP {...}` renames views, like xarray-sql's `table_names`.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicUsize, Ordering};

use duckdb::core::{Inserter, LogicalTypeHandle, LogicalTypeId};
use duckdb::vtab::{BindInfo, InitInfo, TableFunctionInfo, VTab, Value};

use crate::read_zarr_tables::catalog_rows;
use crate::selection::Selection;
use crate::zarr_reader::meta::raw_bind_info;
use crate::zarr_reader::tree;

#[derive(Debug, Clone)]
struct Statement {
    schema_name: Option<String>,
    view_name: Option<String>,
    sql: String,
}

pub struct ReadZarrViewsBind {
    statements: Vec<Statement>,
}

unsafe impl Send for ReadZarrViewsBind {}
unsafe impl Sync for ReadZarrViewsBind {}

pub struct ReadZarrViewsInit {
    /// The first statement the next call emits; a store can have more tables
    /// than one output chunk holds.
    next: AtomicUsize,
}

unsafe impl Send for ReadZarrViewsInit {}
unsafe impl Sync for ReadZarrViewsInit {}

pub struct ReadZarrViewsVTab;

impl VTab for ReadZarrViewsVTab {
    type BindData = ReadZarrViewsBind;
    type InitData = ReadZarrViewsInit;

    fn bind(bind: &BindInfo) -> Result<Self::BindData, Box<dyn std::error::Error>> {
        bind.add_result_column("schema_name", LogicalTypeId::Varchar.into());
        bind.add_result_column("view_name", LogicalTypeId::Varchar.into());
        bind.add_result_column("sql", LogicalTypeId::Varchar.into());

        let store_path = bind.get_parameter(0).to_string();
        let database = bind.get_parameter(1).to_string();
        if database.is_empty() {
            return Err("read_zarr_views needs a database name, like 'era5'".into());
        }
        let mut renames = unsafe { view_names(bind)? };

        let db = quote_identifier(&database);
        let mut statements = vec![Statement {
            schema_name: None,
            view_name: None,
            sql: format!("ATTACH ':memory:' AS {db};"),
        }];
        // Views per schema, both keyed by tree::fold_case, to catch two views
        // that DuckDB would see as one. Schema names fold too: groups `Sim`
        // and `sim` are one DuckDB schema.
        let mut taken: HashMap<String, HashMap<String, String>> = HashMap::new();
        let rows = catalog_rows(bind, &store_path, Selection::Store)?;
        let mut paths = Vec::new();
        for row in rows {
            let Some(table_name) = row.table_name else {
                continue;
            };
            let node = row.group_path.trim_start_matches('/');
            let path = table_path(node, &table_name);
            let view_name = renames.remove(&path).unwrap_or_else(|| table_name.clone());
            if view_name.is_empty() {
                return Err(format!("view_names gives table '{path}' an empty name").into());
            }
            let views = taken.entry(tree::fold_case(&row.schema_name)).or_default();
            if views.is_empty() && !node.is_empty() {
                statements.push(Statement {
                    schema_name: Some(row.schema_name.clone()),
                    view_name: None,
                    sql: format!(
                        "CREATE SCHEMA IF NOT EXISTS {db}.{};",
                        quote_identifier(&row.schema_name)
                    ),
                });
            }
            if let Some(other) = views.insert(tree::fold_case(&view_name), path.clone()) {
                return Err(format!(
                    "tables '{other}' and '{path}' would both be view '{view_name}' in schema \
                     '{}'; rename one with view_names := MAP {{'{path}': '...'}}",
                    row.schema_name
                )
                .into());
            }
            let group_arg = if node.is_empty() {
                String::new()
            } else {
                format!("group_path := {}, ", quote_string(node))
            };
            statements.push(Statement {
                sql: format!(
                    "CREATE OR REPLACE VIEW {db}.{}.{} AS SELECT * FROM read_zarr({}, \
                     {group_arg}table_name := {});",
                    quote_identifier(&row.schema_name),
                    quote_identifier(&view_name),
                    quote_string(&store_path),
                    quote_string(&table_name),
                ),
                schema_name: Some(row.schema_name),
                view_name: Some(view_name),
            });
            paths.push(path);
        }
        if let Some(unknown) = renames.into_keys().next() {
            return Err(format!(
                "view_names names table '{unknown}', which the store does not have; its tables \
                 are [{}] (a table outside the root is named by its group path, like \
                 'group/x_y')",
                paths.join(", ")
            )
            .into());
        }

        Ok(ReadZarrViewsBind { statements })
    }

    fn init(_: &InitInfo) -> Result<Self::InitData, Box<dyn std::error::Error>> {
        Ok(ReadZarrViewsInit {
            next: AtomicUsize::new(0),
        })
    }

    fn func(
        func: &TableFunctionInfo<Self>,
        output: &mut duckdb::core::DataChunkHandle,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let bind = func.get_bind_data();
        let init = func.get_init_data();

        let vector_size = unsafe { duckdb::ffi::duckdb_vector_size() as usize };
        let start = init.next.fetch_add(vector_size, Ordering::Relaxed);
        if start >= bind.statements.len() {
            output.set_len(0);
            return Ok(());
        }
        let end = (start + vector_size).min(bind.statements.len());

        let mut v_schema = output.flat_vector(0);
        let mut v_view = output.flat_vector(1);
        let v_sql = output.flat_vector(2);
        for (i, statement) in bind.statements[start..end].iter().enumerate() {
            match &statement.schema_name {
                Some(name) => v_schema.insert(i, name.as_str()),
                None => v_schema.set_null(i),
            }
            match &statement.view_name {
                Some(name) => v_view.insert(i, name.as_str()),
                None => v_view.set_null(i),
            }
            v_sql.insert(i, statement.sql.as_str());
        }
        output.set_len(end - start);
        Ok(())
    }

    fn parameters() -> Option<Vec<LogicalTypeHandle>> {
        Some(vec![
            LogicalTypeId::Varchar.into(),
            LogicalTypeId::Varchar.into(),
        ])
    }

    fn named_parameters() -> Option<Vec<(String, LogicalTypeHandle)>> {
        let varchar = || LogicalTypeHandle::from(LogicalTypeId::Varchar);
        Some(vec![(
            "view_names".to_string(),
            LogicalTypeHandle::map(&varchar(), &varchar()),
        )])
    }
}

/// How `view_names :=` names a table: its table name in the root, else its
/// group path and table name, like `simulation/fine/x_y`.
fn table_path(node: &str, table_name: &str) -> String {
    if node.is_empty() {
        table_name.to_string()
    } else {
        format!("{node}/{table_name}")
    }
}

/// The `view_names := MAP {...}` parameter, keyed by table path with any
/// leading `/` removed. duckdb-rs has no MAP accessor, so this reads the
/// value through the C API.
///
/// # Safety
/// Must be called from within a DuckDB table-function bind callback.
unsafe fn view_names(
    bind: &BindInfo,
) -> Result<BTreeMap<String, String>, Box<dyn std::error::Error>> {
    use duckdb::ffi::{
        duckdb_bind_get_named_parameter, duckdb_get_map_key, duckdb_get_map_size,
        duckdb_get_map_value,
    };

    let name = std::ffi::CString::new("view_names")?;
    let raw = duckdb_bind_get_named_parameter(raw_bind_info(bind), name.as_ptr());
    if raw.is_null() {
        return Ok(BTreeMap::new());
    }
    // Owns `raw`, and destroys it on return.
    let map = Value::from(raw);
    if map.is_null() {
        return Ok(BTreeMap::new());
    }
    let mut names = BTreeMap::new();
    for i in 0..duckdb_get_map_size(raw) {
        let key = Value::from(duckdb_get_map_key(raw, i));
        let value = Value::from(duckdb_get_map_value(raw, i));
        if key.is_null() || value.is_null() {
            return Err("view_names cannot have NULL keys or values".into());
        }
        let key = key.to_string().trim_start_matches('/').to_string();
        names.insert(key, value.to_string());
    }
    Ok(names)
}

/// `name` as a double-quoted SQL identifier.
fn quote_identifier(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// `text` as a single-quoted SQL string literal.
fn quote_string(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_names_and_strings() {
        assert_eq!(quote_identifier("simulation/fine"), "\"simulation/fine\"");
        assert_eq!(quote_identifier("a\"b"), "\"a\"\"b\"");
        assert_eq!(quote_string("it's.zarr"), "'it''s.zarr'");
    }

    #[test]
    fn table_paths() {
        assert_eq!(table_path("", "x"), "x");
        assert_eq!(table_path("simulation/fine", "x_y"), "simulation/fine/x_y");
    }
}
