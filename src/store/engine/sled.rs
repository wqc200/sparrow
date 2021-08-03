use std::sync::{Arc, Mutex};

use sled::{Config, IVec};
use sled::{Db, Iter};
use parquet::data_type::AsBytes;

use arrow::array::{StringArray, Array};
use datafusion::datasource::TableProvider;
use datafusion::logical_plan::Expr;

use crate::core::global_context::GlobalContext;
use crate::datafusion_impl::datasource::rocksdb::RocksdbTable;
use crate::meta::meta_util;
use crate::mysql::error::{MysqlResult, MysqlError};

use super::engine_util::Engine;
use crate::core::session_context::SessionContext;
use sqlparser::ast::ObjectName;
use datafusion::scalar::ScalarValue;
use crate::meta::def::TableDef;

pub struct Sled {
    core_context: Arc<Mutex<GlobalContext>>,
    table_name: ObjectName,
    table_schema: TableDef,
}

impl Sled {
    pub fn new(
        core_context: Arc<Mutex<GlobalContext>>,
        table_name: ObjectName,
        table_schema: TableDef,
    ) -> Self {
        Self {
            core_context,
            table_name,
            table_schema,
        }
    }
}

impl Engine for Sled {
    fn table_provider(&self) -> Arc<dyn TableProvider> {
        let provider = RocksdbTable::try_new(self.core_context.clone(), self.table_schema.clone(), "/tmp/rocksdb/", self.table_name.clone()).unwrap();
        Arc::new(provider)
    }

    fn insert(&self, column_name: Vec<String>, column_value: Vec<Vec<ScalarValue>>) -> MysqlResult<u64> {
        Ok(0)
    }

    fn add_rows(&self, column_name: Vec<String>, column_value: Vec<Vec<Expr>>) -> MysqlResult<u64> {
        Ok(0)
    }

    fn delete(&self, rowid_array: &StringArray) -> MysqlResult<u64> {
        Ok(0)
    }
}

#[derive(Clone)]
pub struct SledOperator {
    dbpath: String,
    sled_db: Db,
}

impl SledOperator {
    pub fn new(
        dbpath: &str,
    ) -> SledOperator {
        let dbpath = String::from(dbpath);

        let config = sled::Config::new().temporary(false).path(dbpath.clone());
        let sled_db = config.open().unwrap();

        Self {
            dbpath,
            sled_db,
        }
    }

    pub fn write(&mut self, k: Vec<u8>, v: Vec<u8>) {
        self.sled_db.insert(k, v);
    }

    pub fn read(&mut self, k: Vec<u8>) -> Option<Vec<u8>> {
        let a = self.sled_db.get(k).unwrap();
        match a {
            Some(b) => {
                Some(b.to_vec())
            }
            _ => {
                None
            }
        }
    }
}