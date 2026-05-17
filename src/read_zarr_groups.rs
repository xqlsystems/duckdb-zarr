use std::sync::atomic::{AtomicBool, Ordering};

use duckdb::core::LogicalTypeId;
use duckdb::vtab::{BindInfo, InitInfo, TableFunctionInfo, VTab};

use crate::zarr_reader::meta::{extract_file_system, infer_dim_groups, list_array_names, open_store};

#[derive(Debug, Clone)]
struct GroupRow {
    dims: String,
    shape: String,
    chunk_shape: String,
    data_vars: String,
    coord_vars: String,
}

pub struct ReadZarrGroupsBind {
    rows: Vec<GroupRow>,
}

unsafe impl Send for ReadZarrGroupsBind {}
unsafe impl Sync for ReadZarrGroupsBind {}

pub struct ReadZarrGroupsInit {
    done: AtomicBool,
}

unsafe impl Send for ReadZarrGroupsInit {}
unsafe impl Sync for ReadZarrGroupsInit {}

pub struct ReadZarrGroupsVTab;

impl VTab for ReadZarrGroupsVTab {
    type BindData = ReadZarrGroupsBind;
    type InitData = ReadZarrGroupsInit;

    fn bind(bind: &BindInfo) -> Result<Self::BindData, Box<dyn std::error::Error>> {
        bind.add_result_column("dims", LogicalTypeId::Varchar.into());
        bind.add_result_column("shape", LogicalTypeId::Varchar.into());
        bind.add_result_column("chunk_shape", LogicalTypeId::Varchar.into());
        bind.add_result_column("data_vars", LogicalTypeId::Varchar.into());
        bind.add_result_column("coord_vars", LogicalTypeId::Varchar.into());

        let store_path = bind.get_parameter(0).to_string();
        let fs = unsafe { extract_file_system(bind) };
        let store = open_store(&store_path, Some(fs))?;
        let array_names = list_array_names(&store_path, &store)?;
        let (dim_groups, _) = infer_dim_groups(&store, &array_names)?;

        let rows = dim_groups
            .iter()
            .map(|g| GroupRow {
                dims: serde_json::to_string(&g.dims).unwrap_or_default(),
                shape: serde_json::to_string(&g.shape).unwrap_or_default(),
                chunk_shape: serde_json::to_string(&g.chunk_shape).unwrap_or_default(),
                data_vars: serde_json::to_string(&g.data_var_names).unwrap_or_default(),
                coord_vars: serde_json::to_string(&g.coord_var_names).unwrap_or_default(),
            })
            .collect();

        Ok(ReadZarrGroupsBind { rows })
    }

    fn init(_: &InitInfo) -> Result<Self::InitData, Box<dyn std::error::Error>> {
        Ok(ReadZarrGroupsInit {
            done: AtomicBool::new(false),
        })
    }

    fn func(
        func: &TableFunctionInfo<Self>,
        output: &mut duckdb::core::DataChunkHandle,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let bind = func.get_bind_data();
        let init = func.get_init_data();

        if init.done.swap(true, Ordering::Relaxed) {
            output.set_len(0);
            return Ok(());
        }

        let n = bind.rows.len();
        if n == 0 {
            output.set_len(0);
            return Ok(());
        }

        let v_dims = output.flat_vector(0);
        let v_shape = output.flat_vector(1);
        let v_cshape = output.flat_vector(2);
        let v_dvars = output.flat_vector(3);
        let v_cvars = output.flat_vector(4);

        for (i, row) in bind.rows.iter().enumerate() {
            use duckdb::core::Inserter;
            v_dims.insert(i, row.dims.as_str());
            v_shape.insert(i, row.shape.as_str());
            v_cshape.insert(i, row.chunk_shape.as_str());
            v_dvars.insert(i, row.data_vars.as_str());
            v_cvars.insert(i, row.coord_vars.as_str());
        }

        output.set_len(n);
        Ok(())
    }

    fn parameters() -> Option<Vec<duckdb::core::LogicalTypeHandle>> {
        Some(vec![LogicalTypeId::Varchar.into()])
    }
}
