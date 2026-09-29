//! SPIKE, not for merge: `CALL zarr_attach(store, alias)` mounts a store as an
//! in-memory database with one view per dimension group (design.md decision 8).
//!
//! The C API has no way to run SQL on the calling connection, so DDL goes through
//! a second connection that is opened when the extension loads and kept in the
//! table function's extra info. The spike checks whether that is safe.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use duckdb::core::{Inserter, LogicalTypeId};
use duckdb::ffi;
use duckdb::vtab::{BindInfo, InitInfo, TableFunctionInfo, VTab};

use crate::zarr_reader::meta::{
    discover_dim_groups, extract_file_system, list_array_names, open_store,
};

/// A raw connection shared by every call of `zarr_attach`. Queries on it are
/// serialized by the mutex.
pub struct SideConnection(ffi::duckdb_connection);
unsafe impl Send for SideConnection {}

#[derive(Clone)]
pub struct AttachExtra(pub Arc<Mutex<SideConnection>>);

impl AttachExtra {
    /// # Safety
    /// `db` must be the handle from the extension's `get_database`, used during load.
    pub unsafe fn connect(db: ffi::duckdb_database) -> Result<Self, Box<dyn std::error::Error>> {
        let mut con: ffi::duckdb_connection = std::ptr::null_mut();
        if unsafe { ffi::duckdb_connect(db, &mut con) } != ffi::DuckDBSuccess {
            return Err("zarr_attach: duckdb_connect failed".into());
        }
        Ok(Self(Arc::new(Mutex::new(SideConnection(con)))))
    }
}

fn run(con: &SideConnection, sql: &str) -> Result<(), Box<dyn std::error::Error>> {
    let c_sql = std::ffi::CString::new(sql)?;
    let mut result: ffi::duckdb_result = unsafe { std::mem::zeroed() };
    let state = unsafe { ffi::duckdb_query(con.0, c_sql.as_ptr(), &mut result) };
    let err = if state != ffi::DuckDBSuccess {
        let msg = unsafe { std::ffi::CStr::from_ptr(ffi::duckdb_result_error(&mut result)) };
        Some(format!("zarr_attach: {sql}: {}", msg.to_string_lossy()))
    } else {
        None
    };
    unsafe { ffi::duckdb_destroy_result(&mut result) };
    err.map_or(Ok(()), |e| Err(e.into()))
}

fn quote_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

fn quote_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

pub struct ZarrAttachBind {
    views: Vec<String>,
}
pub struct ZarrAttachInit {
    done: AtomicBool,
}

pub struct ZarrAttachVTab;

impl VTab for ZarrAttachVTab {
    type BindData = ZarrAttachBind;
    type InitData = ZarrAttachInit;

    fn bind(bind: &BindInfo) -> Result<Self::BindData, Box<dyn std::error::Error>> {
        bind.add_result_column("view", LogicalTypeId::Varchar.into());
        let store_path = bind.get_parameter(0).to_string();
        let alias = bind.get_parameter(1).to_string();
        let extra = unsafe { &*bind.get_extra_info::<AttachExtra>() };

        let fs = unsafe { extract_file_system(bind) };
        let store = open_store(&store_path, Some(fs))?;
        let names = list_array_names(&store_path, &store)?;
        let groups = discover_dim_groups(&store, &names)?;

        let db = quote_ident(&alias);
        let mut ddl = vec![format!("ATTACH ':memory:' AS {db}")];
        let mut views = Vec::new();
        for g in &groups {
            let table = if g.dims.is_empty() { "scalar".to_string() } else { g.dims.join("_") };
            let dims_sql = g.dims.iter().map(|d| quote_literal(d)).collect::<Vec<_>>().join(", ");
            let qualified = format!("{db}.main.{}", quote_ident(&table));
            ddl.push(format!(
                "CREATE VIEW {qualified} AS SELECT * FROM read_zarr({}, dims := [{dims_sql}])",
                quote_literal(&store_path)
            ));
            views.push(format!("{alias}.main.{table}"));
        }
        // A nested-path schema, to check quoting of '/' in schema names.
        ddl.push(format!("CREATE SCHEMA {db}.{}", quote_ident("demo/nested")));
        ddl.push(format!(
            "CREATE VIEW {db}.{}.{} AS SELECT 42 AS answer",
            quote_ident("demo/nested"),
            quote_ident("scalar")
        ));
        views.push(format!("{alias}.\"demo/nested\".scalar"));

        let con = extra.0.lock().map_err(|_| "zarr_attach: connection lock poisoned")?;
        for stmt in &ddl {
            run(&con, stmt)?;
        }
        Ok(ZarrAttachBind { views })
    }

    fn init(_: &InitInfo) -> Result<Self::InitData, Box<dyn std::error::Error>> {
        Ok(ZarrAttachInit { done: AtomicBool::new(false) })
    }

    fn func(
        func: &TableFunctionInfo<Self>,
        output: &mut duckdb::core::DataChunkHandle,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let bind = func.get_bind_data();
        if func.get_init_data().done.swap(true, Ordering::Relaxed) {
            output.set_len(0);
            return Ok(());
        }
        let v = output.flat_vector(0);
        for (i, name) in bind.views.iter().enumerate() {
            v.insert(i, name.as_str());
        }
        output.set_len(bind.views.len());
        Ok(())
    }

    fn parameters() -> Option<Vec<duckdb::core::LogicalTypeHandle>> {
        Some(vec![LogicalTypeId::Varchar.into(), LogicalTypeId::Varchar.into()])
    }
}
