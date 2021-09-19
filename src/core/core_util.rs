use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::Mutex;

use arrow::array::{Array, as_primitive_array, as_string_array};
use arrow::array::{
    ArrayData,
    BinaryArray,
    Float32Array,
    Float64Array,
    Int16Array,
    Int32Array,
    Int64Array,
    Int8Array,
    StringArray,
    UInt16Array,
    UInt32Array,
    UInt64Array,
    UInt8Array,
};
use arrow::buffer::Buffer;
use arrow::compute::cast;
use arrow::datatypes::{DataType, Field, Schema, ToByteSlice};
use arrow::datatypes::DataType::UInt8;
use arrow::record_batch::{RecordBatch, RecordBatchReader};
use datafusion::catalog::catalog::{CatalogProvider, MemoryCatalogProvider};
use datafusion::catalog::schema::{MemorySchemaProvider, SchemaProvider};
use datafusion::datasource::{CsvFile, CsvReadOptions, MemTable, TableProvider};
use datafusion::error::{DataFusionError, Result};
use datafusion::execution::context::ExecutionContext;
use datafusion::logical_plan::{col, Expr, LogicalPlan, PlanType};
use datafusion::logical_plan::{DFField, DFSchema, DFSchemaRef};
use datafusion::physical_plan::math_expressions;
use datafusion::scalar::ScalarValue;
use datafusion::sql::parser::{DFParser, FileType};
use datafusion::sql::planner::{ContextProvider, SqlToRel};
use parquet::data_type::AsBytes;
use sqlparser::ast::{
    Assignment, BinaryOperator, Expr as SQLExpr, Join, JoinConstraint, JoinOperator,
    Query, Select, SelectItem, SetExpr, TableFactor, TableWithJoins, UnaryOperator, Value,
};
use sqlparser::ast::Ident;
use sqlparser::ast::ObjectName;

use crate::core::global_context::GlobalContext;
use crate::core::session_context::SessionContext;
use crate::datafusion_impl::catalog::information_schema::CatalogWithInformationSchemaProvider;
use crate::datafusion_impl::datasource::rocksdb::RocksdbTable;
use crate::meta::{meta_const, meta_util};
use crate::mysql::error::{MysqlError, MysqlResult};

use crate::test;
use crate::util;
use crate::meta::initial::{read_all_table, read_information_schema_schemata};
use crate::store::engine::engine_util::TableEngineFactory;
use crate::meta::meta_util::read_all_schema;

pub fn register_catalog(global_context: Arc<Mutex<GlobalContext>>, execution_context: &mut ExecutionContext, catalog_name: &str) {
    let state = execution_context.state.lock().unwrap();
    let catalog_provider = Arc::new(MemoryCatalogProvider::new());
    let catalog = Arc::new(CatalogWithInformationSchemaProvider::new(
        global_context,
        state.catalog_list.clone(),
        catalog_provider,
    ));
    state.catalog_list.register_catalog(catalog_name.to_string(), catalog);
}

pub fn register_schema(execution_context: &mut ExecutionContext, catalog_name: &str, schema_name: &str) {
    let schema_provider = Arc::new(MemorySchemaProvider::new());
    let catalog_provider = get_catalog_provider(execution_context, catalog_name);
    let catalog_provider = catalog_provider.as_any()
        .downcast_ref::<CatalogWithInformationSchemaProvider>()
        .expect("Catalog provider was a CatalogWithInformationSchemaProvider");
    catalog_provider.register_schema(schema_name, schema_provider);
}

pub fn register_table(execution_context: &mut ExecutionContext, catalog_name: &str, schema_name: &str, table_name: &str, table_provider: Arc<dyn TableProvider>) {
    let schema_provider = get_schema_provider(execution_context, catalog_name, schema_name);
    schema_provider.register_table(table_name.to_string(), table_provider);
}

pub fn get_catalog_provider(execution_context: &mut ExecutionContext, catalog_name: &str) -> Arc<dyn CatalogProvider> {
    let state = execution_context.state.lock().unwrap();
    let catalog_provider = state.catalog_list.catalog(catalog_name).unwrap();
    catalog_provider
}

pub fn get_schema_provider(execution_context: &mut ExecutionContext, catalog_name: &str, schema_name: &str) -> Arc<dyn SchemaProvider> {
    let catalog_provider = get_catalog_provider(execution_context, catalog_name);
    let schema_provider = catalog_provider.schema(schema_name).unwrap();
    schema_provider
}

pub fn register_all_table(global_context: Arc<Mutex<GlobalContext>>, datafusion_context: &mut ExecutionContext) -> MysqlResult<()> {
    let mut catalog_map = HashMap::new();

    let schema_map = read_all_schema(global_context.clone()).unwrap();
    for (_, schema) in schema_map.iter() {
        catalog_map
            .entry(meta_const::CATALOG_NAME.to_string()).or_insert(HashMap::new())
            .entry(schema.schema_option.schema_name.clone()).or_insert(HashMap::new());
    }

    let table_map = read_all_table(global_context.clone()).unwrap();
    for (_, table) in table_map.iter() {
        catalog_map
            .entry(meta_const::CATALOG_NAME.to_string()).or_insert(HashMap::new())
            .entry(table.option.schema_name.clone()).or_insert(HashMap::new())
            .entry(table.option.table_name.clone()).or_insert(table.clone());
    }

    for (catalog_name, schema_map) in catalog_map.iter() {
        register_catalog(global_context.clone(), datafusion_context, catalog_name.as_str());

        for (schema_name, table_map) in schema_map.iter() {
            register_schema(datafusion_context, catalog_name.as_str(), schema_name.as_str());

            for (table_name, table) in table_map.iter() {
                let full_table_name = table.option.full_table_name.clone();
                let engine = TableEngineFactory::try_new_with_table_name(global_context.clone(), full_table_name.clone());
                let table_provider = match engine {
                    Ok(engine) => engine.table_provider(),
                    Err(mysql_error) => return Err(mysql_error),
                };

                register_table(datafusion_context, catalog_name.as_str(), schema_name.as_str(), table_name.as_str(), table_provider);
            }
        }
    }

    Ok(())
}

pub fn convert_record_to_scalar_value(record_batch: RecordBatch) -> Vec<Vec<ScalarValue>> {
    let mut rows: Vec<Vec<ScalarValue>> = Vec::new();

    let schema = record_batch.schema();
    for column_index in 0..record_batch.num_columns() {
        let field = schema.field(column_index);
        match field.data_type() {
            DataType::Utf8 => {
                let column: &StringArray = as_string_array(record_batch.column(column_index));

                for row_index in 0..record_batch.num_rows() {
                    let mut value = None;
                    if !column.is_null(row_index) {
                        value = Some(column.value(row_index).to_string());
                    }

                    if let Some(row) = rows.get_mut(row_index) {
                        row.insert(column_index, ScalarValue::Utf8(value));
                    } else {
                        let mut row = vec![];
                        row.insert(column_index, ScalarValue::Utf8(value));
                        rows.insert(row_index, row);
                    }
                }
            }
            DataType::Int32 => {
                let column: &Int32Array = as_primitive_array(record_batch.column(column_index));

                for row_index in 0..record_batch.num_rows() {
                    let mut value = None;
                    if !column.is_null(row_index) {
                        value = Some(column.value(row_index));
                    }

                    if let Some(row) = rows.get_mut(row_index) {
                        row.insert(column_index, ScalarValue::Int32(value));
                    } else {
                        let mut row = vec![];
                        row.insert(column_index, ScalarValue::Int32(value));
                        rows.insert(row_index, row);
                    }
                }
            }
            DataType::Int64 => {
                let column: &Int64Array = as_primitive_array(record_batch.column(column_index));

                for row_index in 0..record_batch.num_rows() {
                    let mut value = None;
                    if !column.is_null(row_index) {
                        value = Some(column.value(row_index));
                    }

                    if let Some(row) = rows.get_mut(row_index) {
                        row.insert(column_index, ScalarValue::Int64(value));
                    } else {
                        let mut row = vec![];
                        row.insert(column_index, ScalarValue::Int64(value));
                        rows.insert(row_index, row);
                    }
                }
            }
            DataType::UInt64 => {
                let column: &UInt64Array = as_primitive_array(record_batch.column(column_index));

                for row_index in 0..record_batch.num_rows() {
                    let mut value = None;
                    if !column.is_null(row_index) {
                        value = Some(column.value(row_index));
                    }

                    if let Some(row) = rows.get_mut(row_index) {
                        row.insert(column_index, ScalarValue::UInt64(value));
                    } else {
                        let mut row = vec![];
                        row.insert(column_index, ScalarValue::UInt64(value));
                        rows.insert(row_index, row);
                    }
                }
            }
            _ => {
                let message = format!("unsupported data type: {}", field.data_type().to_string());
                log::error!("{}", message);
                panic!(message)
            }
        }
    }

    rows
}

// pub fn query_to_plan<S: SchemaProvider>(query: &Query, query_planner: &SqlToRel<S>) -> Result<LogicalPlan> {
//     let plan = match &query.body {
//         SetExpr::Select(s) => query_planner.select_to_plan(s.as_ref()),
//         _ => Err(ExecutionError::NotImplemented(
//             format!("Query {} not implemented yet", query.body).to_owned(),
//         )),
//     }?;
//
//     let plan = query_planner.order_by(&plan, &query.order_by)?;
//
//     query_planner.limit(&plan, &query.limit)
// }

pub fn captured_name(current_db: Arc<Mutex<Option<String>>>) -> Option<String> {
    let captured_name = current_db.lock().expect("mutex poisoned");
    let db_name = match captured_name.as_ref() {
        Some(s) => {
            Some(s.clone())
        }
        None => None
    };
    db_name
}

pub fn projection_has_rowid(projection: Vec<SelectItem>) -> bool {
    let has_rowid = projection
        .iter()
        .any(|x| match x {
            SelectItem::UnnamedExpr(expr) => {
                match expr {
                    SQLExpr::Identifier(ident) => {
                        ident.to_string() == meta_const::COLUMN_ROWID.to_string()
                    }
                    _ => { false }
                }
            }
            SelectItem::ExprWithAlias { expr, .. } => {
                false
            }
            _ => {
                false
            }
        });
    has_rowid
}

pub fn remove_rowid_from_projection(plan: &LogicalPlan) -> LogicalPlan {
    match plan.clone() {
        LogicalPlan::Projection { expr, input, schema } => {
            let mut projected_expr = vec![];
            let mut dffields = vec![];

            for i in 0..expr.len() {
                match expr[i] {
                    Expr::Column(ref column) => {
                        if column.name.to_string() != meta_const::COLUMN_ROWID.to_string() {
                            projected_expr.push(expr[i].clone());
                            dffields.push(schema.field(i).clone());
                        }
                    }
                    _ => {
                        projected_expr.push(expr[i].clone());
                        dffields.push(schema.field(i).clone());
                    }
                }
            }

            LogicalPlan::Projection {
                expr: projected_expr,
                input: Arc::new(remove_rowid_from_projection(&input)),
                schema: Arc::new(DFSchema::new(dffields).unwrap()),
            }
        }
        LogicalPlan::Explain { verbose, plan, stringified_plans, schema } => {
            let plan = Arc::new(remove_rowid_from_projection(&plan));
            LogicalPlan::Explain {
                verbose,
                plan,
                stringified_plans,
                schema,
            }
        }
        LogicalPlan::Filter { predicate, input } => {
            LogicalPlan::Filter {
                predicate,
                input: Arc::new(remove_rowid_from_projection(&input)),
            }
        }
        LogicalPlan::TableScan {
            table_name,
            source,
            projection,
            projected_schema,
            filters,
            limit,
        } => {
            let mut dffields = vec![];
            for i in 0..projected_schema.fields().len() {
                let field = projected_schema.field(i).clone();
                if field.name() != &meta_const::COLUMN_ROWID.to_string() {
                    dffields.push(projected_schema.field(i).clone());
                }
            }

            LogicalPlan::TableScan {
                table_name,
                source,
                projection,
                projected_schema: Arc::new(DFSchema::new(dffields).unwrap()),
                filters,
                limit,
            }
        }
        LogicalPlan::Limit { n, input } => {
            LogicalPlan::Limit {
                n,
                input: Arc::new(remove_rowid_from_projection(&input)),
            }
        }
        _ => {
            plan.clone()
        }
    }
}

pub fn create_table_dual() -> Arc<dyn TableProvider> {
    let dual_schema = Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int32, false),
        Field::new("name", DataType::Utf8, false),
    ]));
    let batch = RecordBatch::try_new(
        dual_schema.clone(),
        vec![
            Arc::new(Int32Array::from(vec![Some(1)])),
            Arc::new(StringArray::from(vec!["a"])),
        ],
    ).unwrap();
    let provider = MemTable::try_new(dual_schema.clone(), vec![vec![batch.clone()]]).unwrap();
    Arc::new(provider)
}

// pub fn create_table_information_schema_tables() -> MemTable {
//     let schema = Arc::new(Schema::new(vec![
//         Field::new("TABLE_NAME", DataType::Utf8, false),
//         Field::new("ENGINE", DataType::Utf8, false),
//     ]));
//     let batch = RecordBatch::try_new(
//         schema.clone(),
//         vec![
//             Arc::new(StringArray::from(vec!["a"])),
//             Arc::new(StringArray::from(vec!["b"])),
//         ],
//     ).unwrap();
//     let provider = MemTable::new(schema.clone(), vec![vec![batch.clone()]]).unwrap();
// }

pub fn build_table_dual() -> TableWithJoins {
    let ident = Ident::new("dual");
    let idents = vec![ident];
    let object_name = ObjectName(idents);
    build_table_with_joins(object_name)
}

pub fn build_update_sqlselect(table_name: ObjectName, assignments: Vec<Assignment>, selection: Option<SQLExpr>) -> Select {
    let table_with_joins = build_table_with_joins(table_name);
    // projection
    let sql_expr = SQLExpr::Identifier(Ident { value: meta_const::COLUMN_ROWID.to_string(), quote_style: None });
    let mut projection = vec![SelectItem::UnnamedExpr(sql_expr)];
    for assignment in assignments.clone() {
        let select_item = SelectItem::UnnamedExpr(assignment.value.to_owned());
        projection.push(select_item);
    }

    let select = Select {
        distinct: false,
        top: None,
        projection,
        from: vec![table_with_joins],
        lateral_views: vec![],
        selection: selection.clone(),
        group_by: vec![],
        cluster_by: vec![],
        distribute_by: vec![],
        sort_by: vec![],
        having: None,
    };
    select
}

pub fn build_select_wildcard_sqlselect(full_table_name: ObjectName, selection: Option<SQLExpr>) -> Select {
    let table_with_joins = build_table_with_joins(full_table_name);
    let projection = vec![SelectItem::Wildcard];

    let select = Select {
        distinct: false,
        top: None,
        projection,
        from: vec![table_with_joins],
        lateral_views: vec![],
        selection: selection.clone(),
        group_by: vec![],
        cluster_by: vec![],
        distribute_by: vec![],
        sort_by: vec![],
        having: None,
    };
    select
}

pub fn build_select_rowid_sqlselect(table_name: ObjectName, selection: Option<SQLExpr>) -> Select {
    let table_with_joins = build_table_with_joins(table_name.clone());

    let sql_expr = SQLExpr::Identifier(Ident { value: meta_const::COLUMN_ROWID.to_string(), quote_style: None });
    let mut projection = vec![SelectItem::UnnamedExpr(sql_expr)];

    let select = Select {
        distinct: false,
        top: None,
        projection,
        from: vec![table_with_joins],
        lateral_views: vec![],
        selection,
        group_by: vec![],
        cluster_by: vec![],
        distribute_by: vec![],
        sort_by: vec![],
        having: None,
    };
    select
}

pub fn build_table_with_joins(table_name: ObjectName) -> TableWithJoins {
    let table_factor = TableFactor::Table {
        name: table_name,
        alias: None,
        args: vec![],
        with_hints: vec![],
    };
    let table_with_joins = TableWithJoins {
        relation: table_factor.clone(),
        joins: vec![],
    };
    table_with_joins
}

pub fn get_real_value(expr: Expr) -> Result<Option<String>> {
    match expr {
        Expr::Literal(scalar_value) => {
            match scalar_value {
                ScalarValue::Int32(limit) => {
                    if let Some(value) = limit {
                        Ok(Some(value.to_string()))
                    } else {
                        Ok(None)
                    }
                }
                ScalarValue::Int64(limit) => {
                    if let Some(value) = limit {
                        Ok(Some(value.to_string()))
                    } else {
                        Ok(None)
                    }
                }
                ScalarValue::UInt64(limit) => {
                    if let Some(value) = limit {
                        Ok(Some(value.to_string()))
                    } else {
                        Ok(None)
                    }
                }
                ScalarValue::Utf8(limit) => {
                    if let Some(value) = limit {
                        Ok(Some(value.to_string()))
                    } else {
                        Ok(None)
                    }
                }
                _ => {
                    let message = format!("Limit only supports non-negative integer literals, scalar_value: {:?}", scalar_value);
                    log::error!("{}", message);
                    Err(DataFusionError::Execution(message))
                }
            }
        }
        _ => {
            let message = format!("Limit only supports non-negative integer literals, expr: {:?}", expr);
            log::error!("{}", message);
            Err(DataFusionError::Execution(message))
        }
    }
}

pub fn convert_scalar_value(scalar_value: ScalarValue) -> MysqlResult<Option<String>> {
    match scalar_value {
        ScalarValue::Int32(limit) => {
            if let Some(value) = limit {
                Ok(Some(value.to_string()))
            } else {
                Ok(None)
            }
        }
        ScalarValue::Int64(limit) => {
            if let Some(value) = limit {
                Ok(Some(value.to_string()))
            } else {
                Ok(None)
            }
        }
        ScalarValue::UInt64(limit) => {
            if let Some(value) = limit {
                Ok(Some(value.to_string()))
            } else {
                Ok(None)
            }
        }
        ScalarValue::Utf8(limit) => {
            if let Some(value) = limit {
                Ok(Some(value.to_string()))
            } else {
                Ok(None)
            }
        }
        _ => {
            let message = format!("Limit only supports non-negative integer literals, scalar_value: {:?}", scalar_value);
            log::error!("{}", message);
            Err(MysqlError::new_global_error(1105, message.as_str()))
        }
    }
}

pub fn build_find_column_sqlwhere(catalog_name: &str, schema_name: &str, table_name: &str, column_name: &str) -> SQLExpr {
    let selection_catalog = SQLExpr::BinaryOp {
        left: Box::new(SQLExpr::Identifier(Ident::new(meta_const::COLUMN_INFORMATION_SCHEMA_TABLE_CATALOG))),
        op: BinaryOperator::Eq,
        right: Box::new(SQLExpr::Value(Value::SingleQuotedString(catalog_name.to_string()))),
    };
    let selection_schema = SQLExpr::BinaryOp {
        left: Box::new(SQLExpr::Identifier(Ident::new(meta_const::COLUMN_INFORMATION_SCHEMA_TABLE_SCHEMA))),
        op: BinaryOperator::Eq,
        right: Box::new(SQLExpr::Value(Value::SingleQuotedString(schema_name.to_string()))),
    };
    let selection_table = SQLExpr::BinaryOp {
        left: Box::new(SQLExpr::Identifier(Ident::new(meta_const::COLUMN_INFORMATION_SCHEMA_TABLE_NAME))),
        op: BinaryOperator::Eq,
        right: Box::new(SQLExpr::Value(Value::SingleQuotedString(table_name.to_string()))),
    };
    let selection_column = SQLExpr::BinaryOp {
        left: Box::new(SQLExpr::Identifier(Ident::new(meta_const::COLUMN_INFORMATION_SCHEMA_COLUMN_NAME))),
        op: BinaryOperator::Eq,
        right: Box::new(SQLExpr::Value(Value::SingleQuotedString(column_name.to_string()))),
    };
    let selection_catalog_and_schema = SQLExpr::BinaryOp {
        left: Box::new(selection_catalog),
        op: BinaryOperator::And,
        right: Box::new(selection_schema),
    };
    let selection_catalog_and_schema_and_table = SQLExpr::BinaryOp {
        left: Box::new(selection_catalog_and_schema),
        op: BinaryOperator::And,
        right: Box::new(selection_table),
    };
    let selection = SQLExpr::BinaryOp {
        left: Box::new(selection_catalog_and_schema_and_table),
        op: BinaryOperator::And,
        right: Box::new(selection_column),
    };
    selection
}

pub fn build_find_table_sqlwhere(catalog_name: &str, schema_name: &str, table_name: &str) -> SQLExpr {
    let selection_catalog = SQLExpr::BinaryOp {
        left: Box::new(SQLExpr::Identifier(Ident::new(meta_const::COLUMN_INFORMATION_SCHEMA_TABLE_CATALOG))),
        op: BinaryOperator::Eq,
        right: Box::new(SQLExpr::Value(Value::SingleQuotedString(catalog_name.to_string()))),
    };
    let selection_schema = SQLExpr::BinaryOp {
        left: Box::new(SQLExpr::Identifier(Ident::new(meta_const::COLUMN_INFORMATION_SCHEMA_TABLE_SCHEMA))),
        op: BinaryOperator::Eq,
        right: Box::new(SQLExpr::Value(Value::SingleQuotedString(schema_name.to_string()))),
    };
    let selection_table = SQLExpr::BinaryOp {
        left: Box::new(SQLExpr::Identifier(Ident::new(meta_const::COLUMN_INFORMATION_SCHEMA_TABLE_NAME))),
        op: BinaryOperator::Eq,
        right: Box::new(SQLExpr::Value(Value::SingleQuotedString(table_name.to_string()))),
    };
    let selection = SQLExpr::BinaryOp {
        left: Box::new(selection_catalog),
        op: BinaryOperator::And,
        right: Box::new(selection_schema),
    };
    let selection = SQLExpr::BinaryOp {
        left: Box::new(selection),
        op: BinaryOperator::And,
        right: Box::new(selection_table),
    };
    selection
}

pub fn build_find_column_ordinal_position_sqlwhere(catalog_name: &str, schema_name: &str, table_name: &str, ordinal_position: i32) -> SQLExpr {
    let selection_catalog = SQLExpr::BinaryOp {
        left: Box::new(SQLExpr::Identifier(Ident::new(meta_const::COLUMN_INFORMATION_SCHEMA_TABLE_CATALOG))),
        op: BinaryOperator::Eq,
        right: Box::new(SQLExpr::Value(Value::SingleQuotedString(catalog_name.to_string()))),
    };
    let selection_schema = SQLExpr::BinaryOp {
        left: Box::new(SQLExpr::Identifier(Ident::new(meta_const::COLUMN_INFORMATION_SCHEMA_TABLE_SCHEMA))),
        op: BinaryOperator::Eq,
        right: Box::new(SQLExpr::Value(Value::SingleQuotedString(schema_name.to_string()))),
    };
    let selection_table = SQLExpr::BinaryOp {
        left: Box::new(SQLExpr::Identifier(Ident::new(meta_const::COLUMN_INFORMATION_SCHEMA_TABLE_NAME))),
        op: BinaryOperator::Eq,
        right: Box::new(SQLExpr::Value(Value::SingleQuotedString(table_name.to_string()))),
    };
    let selection_column = SQLExpr::BinaryOp {
        left: Box::new(SQLExpr::Identifier(Ident::new(meta_const::COLUMN_INFORMATION_SCHEMA_ORDINAL_POSITION))),
        op: BinaryOperator::Gt,
        right: Box::new(SQLExpr::Value(Value::Number(ordinal_position.to_string(), false))),
    };
    let selection_catalog_and_schema = SQLExpr::BinaryOp {
        left: Box::new(selection_catalog),
        op: BinaryOperator::And,
        right: Box::new(selection_schema),
    };
    let selection_selection_catalog_and_schema_and_table = SQLExpr::BinaryOp {
        left: Box::new(selection_catalog_and_schema),
        op: BinaryOperator::And,
        right: Box::new(selection_table),
    };
    let selection = SQLExpr::BinaryOp {
        left: Box::new(selection_selection_catalog_and_schema_and_table),
        op: BinaryOperator::And,
        right: Box::new(selection_column),
    };
    selection
}

pub fn build_update_column_assignments() -> Vec<Assignment> {
    let value = SQLExpr::BinaryOp {
        left: Box::new(SQLExpr::Identifier(Ident::new(meta_const::COLUMN_INFORMATION_SCHEMA_ORDINAL_POSITION))),
        op: BinaryOperator::Plus,
        right: Box::new(SQLExpr::Value(Value::Number("1".to_string(), false))),
    };
    let assignment = Assignment {
        id: Ident::new(meta_const::COLUMN_INFORMATION_SCHEMA_ORDINAL_POSITION),
        value,
    };
    vec![assignment]
}
