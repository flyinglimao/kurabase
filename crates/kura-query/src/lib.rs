//! RLS-preserving immutable relational query execution.
//!
//! Only explicitly admitted query ASTs reach DataFusion. Every base relation is
//! copied through `Database::read_table` under the caller's fixed AuthContext;
//! joins, subqueries and views can therefore never recover filtered-out rows.
use datafusion::{
    arrow::{array::{Array, ArrayRef, BooleanArray, Float32Array, Float64Array, Int8Array, Int16Array,
        Int32Array, Int64Array, UInt8Array, UInt16Array, UInt32Array, UInt64Array, StringArray,
        LargeStringArray, StringViewArray}, datatypes::{DataType, Field, Schema}, record_batch::RecordBatch},
    common::{DataFusionError, ExprSchema, tree_node::{TreeNode, TreeNodeRecursion}},
    datasource::MemTable,
    execution::{context::{SessionContext, SQLOptions}, disk_manager::{DiskManagerBuilder, DiskManagerMode}, runtime_env::RuntimeEnvBuilder},
    prelude::SessionConfig,
    logical_expr::{Expr as LogicalExpr, ExprSchemable, LogicalPlan},
};
use kurasql::{AuthContext, Database, SqlError, Table};
use serde_json::{Map, Number, Value};
use sqlparser::{ast::{self, *}, dialect::PostgreSqlDialect, parser::Parser};
use std::{collections::BTreeSet, ops::ControlFlow, sync::Arc, time::Duration};

type Result<T> = std::result::Result<T, SqlError>;
const MAX_SQL_BYTES: usize = 256 * 1024;
const MAX_RESULT_ROWS: usize = 100_000;
const MAX_QUERY_NESTING: usize = 64;

fn error(code: &str, message: impl Into<String>) -> SqlError {
    SqlError { code: code.into(), message: message.into() }
}
fn unsupported(message: impl Into<String>) -> SqlError { error("UnsupportedFeature", message) }

fn backend_error(error_: DataFusionError) -> SqlError {
    match error_ {
        DataFusionError::Context(_, source) => backend_error(*source),
        DataFusionError::ResourcesExhausted(_) => error("ResourceLimit", "Query memory or execution resource limit exceeded"),
        DataFusionError::NotImplemented(_) => unsupported("This query form is not supported by the relational backend"),
        DataFusionError::Plan(_) | DataFusionError::SchemaError(..) => error("TypeError", "Invalid query columns, argument types, or relational expression"),
        DataFusionError::SQL(..) => unsupported("The query syntax has no supported backend translation"),
        DataFusionError::Execution(_) | DataFusionError::ArrowError(..) => error("TypeError", "Invalid expression operands, numeric overflow, or incompatible query arguments"),
        _ => error("ExecutionFailure", "Relational execution failed"),
    }
}

/// Execute one read-only relational query against a caller-owned immutable snapshot.
///
/// This function never mutates the database, caches privileged rows, accesses SQL
/// file sources, or refreshes materialized views. The gateway pins the enclosing
/// block snapshot and handles authentication before calling it.
pub async fn execute(db: &Database, sql: &str, context: &AuthContext) -> Result<Vec<Value>> {
    let (_, batches) = query_batches(db, sql, context).await?;
    batches_to_rows(&batches)
}

/// Build a typed derived table for a gateway-owned, per-auth materialized cache.
/// Column types come from the query schema even when no rows are returned.
pub async fn materialize(db: &Database, sql: &str, context: &AuthContext, name: &str) -> Result<Table> {
    let (schema, batches) = query_batches(db, sql, context).await?;
    let rows = batches_to_rows(&batches)?;
    let mut columns = Vec::new();
    for (index, field) in schema.fields().iter().enumerate() {
        let data_type = match field.data_type() {
            DataType::Int8 | DataType::Int16 => "smallint",
            DataType::Int32 => "integer",
            DataType::Int64 | DataType::UInt8 | DataType::UInt16 | DataType::UInt32 | DataType::UInt64 => "bigint",
            DataType::Boolean => "boolean",
            DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View if field.metadata().get("kurasql.logical_type").is_some_and(|kind| matches!(kind.as_str(), "json" | "jsonb")) => field.metadata()["kurasql.logical_type"].as_str(),
            DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View | DataType::Null => "text",
            other => return Err(unsupported(format!("Materialized result type {other} has no supported canonical table type"))),
        };
        columns.push(serde_json::from_value(serde_json::json!({
            "id": index as u64 + 1, "name": field.name(), "data_type": data_type,
            "nullable": field.is_nullable(), "default": null
        })).map_err(|_| error("ExecutionFailure", "Cannot construct materialized column metadata"))?);
    }
    let mut table = Table {
        id: 0, name: name.into(), columns, primary_key: Vec::new(), unique: Vec::new(), deferred_unique: BTreeSet::new(),
        foreign_keys: Vec::new(), checks: Vec::new(), rls_enabled: false, policies: Vec::new(),
        rows: Default::default(), next_row_id: rows.len() as u64 + 1,
    };
    for (index, row) in rows.into_iter().enumerate() {
        let values = row.as_object().ok_or_else(|| error("ExecutionFailure", "Expected result object"))?
            .iter().map(|(name, value)| (name.clone(), value.clone())).collect();
        table.rows.insert(index as u64 + 1, kurasql::Row { id: index as u64 + 1, version: 0, values });
    }
    // Fail closed if a backend unsigned value cannot fit the canonical signed type.
    table_batch(&table, &table.rows.values().map(|row| Value::Object(row.values.clone().into_iter().collect())).collect::<Vec<_>>())?;
    Ok(table)
}

async fn query_batches(db: &Database, sql: &str, context: &AuthContext) -> Result<(Arc<Schema>, Vec<RecordBatch>)> {
    let (query, references) = normalize(db, sql, context, &[])?;
    let runtime = RuntimeEnvBuilder::new()
        .with_memory_limit(128 * 1024 * 1024, 1.0)
        .with_disk_manager_builder(DiskManagerBuilder::default().with_mode(DiskManagerMode::Disabled))
        .build_arc().map_err(backend_error)?;
    let config = SessionConfig::new().with_target_partitions(1).with_information_schema(false);
    let engine = SessionContext::new_with_config_rt(config, runtime);
    for name in references {
        let table = db.table(&name)?;
        // This is the sole path by which canonical row values enter the backend.
        let rows = db.read_table(&name, context)?;
        let batch = table_batch(table, &rows)?;
        let provider = MemTable::try_new(batch.schema(), vec![vec![batch]]).map_err(backend_error)?;
        engine.register_table(datafusion::common::TableReference::bare(name), Arc::new(provider)).map_err(backend_error)?;
    }
    let options = SQLOptions::new().with_allow_ddl(false).with_allow_dml(false).with_allow_statements(false);
    let work = async {
        // Planning failures arise before a logical plan exists. They are all
        // user-query errors at this boundary; keep backend variants stable.
        let frame = engine.sql_with_options(&query.to_string(), options).await
            .map_err(|_| error("TypeError", "Invalid query columns, argument types, or relational expression"))?;
        validate_plan(frame.logical_plan())?;
        let schema = Arc::new(frame.schema().as_arrow().clone());
        let batches = frame.collect().await.map_err(backend_error)?;
        Ok((schema, batches))
    };
    tokio::time::timeout(Duration::from_secs(15), work).await
        .map_err(|_| error("ResourceLimit", "Query execution exceeded 15 seconds"))?
}

// Inspect the unoptimized typed plan before DataFusion's coercion analyzer can
// silently turn text into numbers (or JSON's storage string into SQL text).
fn validate_plan(plan: &LogicalPlan) -> Result<()> {
    let mut failure = None;
    plan.apply_with_subqueries(|node| {
        let schemas = node.inputs().into_iter().map(|input| input.schema().as_ref())
            .chain(std::iter::once(node.schema().as_ref())).collect::<Vec<_>>();
        let expression_type = |expr: &LogicalExpr| schemas.iter().find_map(|schema| expr.get_type(*schema).ok());
        let contains_json = |expr: &LogicalExpr| {
            let mut found = false;
            let _ = expr.apply(|child| {
                if let LogicalExpr::Column(column) = child {
                    found |= schemas.iter().any(|schema| schema.field_from_column(column).ok()
                        .and_then(|field| field.metadata().get("kurasql.logical_type"))
                        .is_some_and(|kind| matches!(kind.as_str(), "json" | "jsonb")));
                }
                Ok(TreeNodeRecursion::Continue)
            });
            found
        };
        node.apply_expressions(|root| {
            root.apply(|expr| {
                if let LogicalExpr::BinaryExpr(binary) = expr {
                    if let (Some(left), Some(right)) = (expression_type(&binary.left), expression_type(&binary.right)) {
                        let family = |kind: &DataType| match kind {
                            DataType::Null => 0,
                            DataType::Int8 | DataType::Int16 | DataType::Int32 | DataType::Int64 | DataType::UInt8
                            | DataType::UInt16 | DataType::UInt32 | DataType::UInt64 | DataType::Float32 | DataType::Float64 => 1,
                            DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View => 2,
                            DataType::Boolean => 3,
                            _ => 4,
                        };
                        if family(&left) != 0 && family(&right) != 0 && family(&left) != family(&right) {
                            failure = Some(error("TypeError", "Incompatible operands require an explicit CAST"));
                        }
                    }
                }
                if contains_json(expr) && !matches!(expr, LogicalExpr::Column(_) | LogicalExpr::Alias(_)
                    | LogicalExpr::IsNull(_) | LogicalExpr::IsNotNull(_)) {
                    failure = Some(unsupported("JSON expressions require the native JSON evaluator; direct projections are supported"));
                }
                Ok(TreeNodeRecursion::Continue)
            })?;
            Ok(TreeNodeRecursion::Continue)
        })?;
        if matches!(node, LogicalPlan::Sort(_) | LogicalPlan::Aggregate(_) | LogicalPlan::Distinct(_)) {
            node.apply_expressions(|expr| {
                if contains_json(expr) { failure = Some(unsupported("JSON sorting, grouping and aggregation are not implemented")); }
                Ok(TreeNodeRecursion::Continue)
            })?;
        }
        if matches!(node, LogicalPlan::Distinct(_)) && node.schema().fields().iter().any(|field|
            field.metadata().get("kurasql.logical_type").is_some_and(|kind| matches!(kind.as_str(), "json" | "jsonb"))) {
            failure = Some(unsupported("DISTINCT on JSON values requires native JSON equality semantics"));
        }
        Ok(TreeNodeRecursion::Continue)
    }).map_err(backend_error)?;
    match failure { Some(error_) => Err(error_), None => Ok(()) }
}

fn normalized_ident(identifier: &Ident) -> String {
    if identifier.quote_style.is_some() { identifier.value.clone() } else { identifier.value.to_lowercase() }
}

fn normalize(db: &Database, sql: &str, context: &AuthContext, view_path: &[String]) -> Result<(Box<Query>, BTreeSet<String>)> {
    if sql.len() > MAX_SQL_BYTES || view_path.len() >= MAX_QUERY_NESTING {
        return Err(error("ResourceLimit", "SQL or view nesting limit exceeded"));
    }
    let mut statements = Parser::parse_sql(&PostgreSqlDialect {}, sql)
        .map_err(|_| error("SyntaxError", "Invalid SQL query"))?;
    if statements.len() != 1 { return Err(unsupported("Exactly one read-only SELECT query is required")); }
    let mut query = match statements.remove(0) {
        Statement::Query(query) => query,
        _ => return Err(unsupported("The relational endpoint accepts read-only queries only")),
    };
    let mut normalizer = Normalizer { db, context, view_path, references: BTreeSet::new(), cte_scopes: Vec::new() };
    if let ControlFlow::Break(error_) = VisitMut::visit(&mut query, &mut normalizer) { return Err(error_); }
    Ok((query, normalizer.references))
}

struct Normalizer<'a> {
    db: &'a Database,
    context: &'a AuthContext,
    view_path: &'a [String],
    references: BTreeSet<String>,
    cte_scopes: Vec<BTreeSet<String>>,
}

fn validate_body(body: &SetExpr) -> Result<()> {
    match body {
        SetExpr::Select(select) => {
            if select.into.is_some() || select.top.is_some() || !select.lateral_views.is_empty()
                || select.qualify.is_some() || !select.cluster_by.is_empty() || !select.distribute_by.is_empty()
                || !select.sort_by.is_empty() {
                return Err(unsupported("SELECT INTO and dialect-specific query modifiers are not supported"));
            }
            Ok(())
        }
        SetExpr::SetOperation { left, right, set_quantifier, .. } => {
            if !matches!(set_quantifier, SetQuantifier::All | SetQuantifier::Distinct | SetQuantifier::None) {
                return Err(unsupported("Set operations by column name are not supported"));
            }
            validate_body(left)?; validate_body(right)
        }
        SetExpr::Query(_) | SetExpr::Values(_) => Ok(()),
        _ => Err(unsupported("Mutating CTEs and non-query set expressions are not supported")),
    }
}

impl VisitorMut for Normalizer<'_> {
    type Break = SqlError;

    fn pre_visit_query(&mut self, query: &mut Query) -> ControlFlow<SqlError> {
        if self.cte_scopes.len() >= MAX_QUERY_NESTING { return ControlFlow::Break(error("ResourceLimit", "Query nesting limit exceeded")); }
        if !query.locks.is_empty() || !query.limit_by.is_empty() || query.for_clause.is_some() {
            return ControlFlow::Break(unsupported("Locking and dialect-specific query clauses are not supported"));
        }
        if let Err(error_) = validate_body(&query.body) { return ControlFlow::Break(error_); }
        let aliases = query.with.as_ref().map(|with| with.cte_tables.iter().map(|cte| normalized_ident(&cte.alias.name)).collect()).unwrap_or_default();
        self.cte_scopes.push(aliases);
        ControlFlow::Continue(())
    }

    fn post_visit_query(&mut self, _: &mut Query) -> ControlFlow<SqlError> {
        self.cte_scopes.pop();
        ControlFlow::Continue(())
    }

    fn pre_visit_statement(&mut self, statement: &mut Statement) -> ControlFlow<SqlError> {
        if !matches!(statement, Statement::Query(_)) {
            return ControlFlow::Break(unsupported("Nested mutating statements are not supported"));
        }
        ControlFlow::Continue(())
    }

    fn pre_visit_table_factor(&mut self, factor: &mut TableFactor) -> ControlFlow<SqlError> {
        match factor {
            TableFactor::Table { name, alias, args, with_hints, version, partitions } => {
                if args.is_some() || !with_hints.is_empty() || version.is_some() || !partitions.is_empty() {
                    return ControlFlow::Break(unsupported("Table functions, file sources and versioned relations are not supported"));
                }
                let relation = match kurasql::table_name(name) { Ok(name) => name, Err(error_) => return ControlFlow::Break(error_) };
                let is_cte = name.0.len() == 1 && self.cte_scopes.iter().rev().any(|scope| scope.contains(&relation));
                if is_cte { return ControlFlow::Continue(()); }
                if let Some(view) = self.db.views.get(&relation) {
                    if view.materialized { return ControlFlow::Break(unsupported("Materialized views require the gateway snapshot cache")); }
                    if self.view_path.contains(&relation) { return ControlFlow::Break(unsupported("Cyclic view dependency")); }
                    let mut path = self.view_path.to_vec(); path.push(relation.clone());
                    let (subquery, references) = match normalize(self.db, &view.sql, self.context, &path) {
                        Ok(value) => value, Err(error_) => return ControlFlow::Break(error_),
                    };
                    self.references.extend(references);
                    let derived_alias = alias.clone().or_else(|| Some(TableAlias {
                        name: Ident::with_quote('"', relation), columns: Vec::new(),
                    }));
                    *factor = TableFactor::Derived { lateral: false, subquery, alias: derived_alias };
                } else if self.db.catalog.contains_key(&relation) {
                    self.references.insert(relation.clone());
                    // Views have their own catalog scope; an outer CTE must not
                    // capture a base-table reference inside an expanded view.
                    *name = ObjectName(vec![Ident::new("public"), Ident::with_quote('"', relation)]);
                } else {
                    return ControlFlow::Break(error("UndefinedTable", format!("Relation {relation} does not exist")));
                }
            }
            TableFactor::Derived { lateral: false, .. } | TableFactor::NestedJoin { .. } => {},
            _ => return ControlFlow::Break(unsupported("Only catalog tables, views, CTEs and derived queries are supported")),
        }
        ControlFlow::Continue(())
    }

    fn pre_visit_expr(&mut self, expr: &mut Expr) -> ControlFlow<SqlError> {
        let result = match expr {
            Expr::Function(function) => {
                let name = function.name.0.iter().map(normalized_ident).collect::<Vec<_>>().join(".");
                if matches!(name.as_str(), "auth.uid" | "auth.jwt") {
                    if !function.args.is_empty() || function.over.is_some() || function.filter.is_some()
                        || function.distinct || !function.order_by.is_empty() || function.null_treatment.is_some() {
                        return ControlFlow::Break(unsupported("Identity functions accept no arguments or modifiers"));
                    }
                    if name == "auth.jwt" {
                        return ControlFlow::Break(unsupported("auth.jwt() query output requires the native JSON evaluator; policies still use the fixed AuthContext"));
                    }
                    *expr = Expr::Value(self.context.uid.clone().map(ast::Value::SingleQuotedString).unwrap_or(ast::Value::Null));
                    Ok(())
                } else if matches!(name.as_str(),
                    "count" | "sum" | "avg" | "min" | "max" | "row_number" | "rank" | "dense_rank"
                    | "lag" | "lead" | "first_value" | "last_value" | "nth_value" | "ntile" | "percent_rank" | "cume_dist"
                    | "abs" | "coalesce" | "nullif" | "lower" | "upper" | "trim" | "ltrim" | "rtrim"
                    | "substring" | "substr" | "length" | "char_length" | "character_length" | "concat" | "concat_ws"
                    | "replace" | "starts_with" | "ends_with" | "greatest" | "least") { Ok(()) }
                else { Err(unsupported(format!("Function {name} is not supported by KuraSQL reads"))) }
            }
            Expr::Value(ast::Value::Number(number, _)) => number.parse::<i64>().map(|_| ()).map_err(|_| unsupported("Only signed 64-bit integer literals are currently supported")),
            Expr::Value(ast::Value::Null | ast::Value::Boolean(_) | ast::Value::SingleQuotedString(_)) => Ok(()),
            Expr::Cast { data_type, format, .. } => {
                let target = data_type.to_string().to_lowercase();
                if format.is_some() || !matches!(target.as_str(), "smallint" | "int" | "integer" | "bigint" | "boolean" | "bool" | "text" | "varchar") {
                    Err(unsupported(format!("CAST target {target} is not supported")))
                } else { Ok(()) }
            }
            Expr::BinaryOp { op, .. } => {
                if matches!(op, BinaryOperator::Plus | BinaryOperator::Minus | BinaryOperator::Multiply | BinaryOperator::Divide
                    | BinaryOperator::Modulo | BinaryOperator::Eq | BinaryOperator::NotEq | BinaryOperator::Gt | BinaryOperator::GtEq
                    | BinaryOperator::Lt | BinaryOperator::LtEq | BinaryOperator::And | BinaryOperator::Or | BinaryOperator::StringConcat) { Ok(()) }
                else { Err(unsupported("Binary operator is not supported")) }
            }
            Expr::UnaryOp { op, .. } if matches!(op, UnaryOperator::Plus | UnaryOperator::Minus | UnaryOperator::Not) => Ok(()),
            Expr::Identifier(_) | Expr::CompoundIdentifier(_) | Expr::Nested(_) | Expr::IsFalse(_) | Expr::IsNotFalse(_)
            | Expr::IsTrue(_) | Expr::IsNotTrue(_) | Expr::IsNull(_) | Expr::IsNotNull(_) | Expr::IsUnknown(_) | Expr::IsNotUnknown(_)
            | Expr::IsDistinctFrom(..) | Expr::IsNotDistinctFrom(..) | Expr::InList { .. } | Expr::InSubquery { .. }
            | Expr::Between { .. } | Expr::Like { .. } | Expr::ILike { .. } | Expr::Case { .. } | Expr::Exists { .. }
            | Expr::Subquery(_) | Expr::AggregateExpressionWithFilter { .. } | Expr::Substring { .. } | Expr::Trim { .. }
            | Expr::Position { .. } => Ok(()),
            _ => Err(unsupported("Expression form is not supported by KuraSQL reads")),
        };
        match result { Ok(()) => ControlFlow::Continue(()), Err(error_) => ControlFlow::Break(error_) }
    }
}

fn column_type(type_name: &str) -> Result<DataType> {
    let lowered = type_name.to_lowercase();
    let base = lowered.split('(').next().unwrap_or(&lowered).trim();
    match base {
        "smallint" | "int2" | "int" | "integer" | "int4" | "bigint" | "int8" => Ok(DataType::Int64),
        "boolean" | "bool" => Ok(DataType::Boolean),
        "text" | "varchar" | "character varying" | "uuid" | "json" | "jsonb" => Ok(DataType::Utf8),
        _ => Err(unsupported(format!("Projection type {type_name} is not supported"))),
    }
}

fn table_batch(table: &Table, rows: &[Value]) -> Result<RecordBatch> {
    let mut fields = Vec::new(); let mut arrays: Vec<ArrayRef> = Vec::new();
    for column in &table.columns {
        let data_type = column_type(&column.data_type)?;
        let mut field = Field::new(&column.name, data_type.clone(), column.nullable);
        if matches!(column.data_type.as_str(), "json" | "jsonb") {
            field = field.with_metadata([("kurasql.logical_type".into(), column.data_type.clone())].into_iter().collect());
        }
        fields.push(field);
        let values = rows.iter().map(|row| row.get(&column.name).unwrap_or(&Value::Null)).collect::<Vec<_>>();
        let type_error = || error("TypeError", format!("Invalid canonical value for {}.{}", table.name, column.name));
        if !column.nullable && values.iter().any(|value| value.is_null()) { return Err(type_error()); }
        let array: ArrayRef = match data_type {
            DataType::Int64 => Arc::new(Int64Array::from(values.iter().map(|value| {
                if value.is_null() { Ok(None) } else { value.as_i64().map(Some).ok_or_else(&type_error) }
            }).collect::<Result<Vec<_>>>()?)),
            DataType::Boolean => Arc::new(BooleanArray::from(values.iter().map(|value| {
                if value.is_null() { Ok(None) } else { value.as_bool().map(Some).ok_or_else(&type_error) }
            }).collect::<Result<Vec<_>>>()?)),
            DataType::Utf8 => {
                let json = matches!(column.data_type.to_lowercase().as_str(), "json" | "jsonb");
                let strings = values.iter().map(|value| {
                    if value.is_null() { Ok(None) } else if json { Ok(Some(value.to_string())) }
                    else { value.as_str().map(|value| Some(value.to_owned())).ok_or_else(&type_error) }
                }).collect::<Result<Vec<Option<String>>>>()?;
                Arc::new(StringArray::from(strings))
            }
            _ => return Err(unsupported("Unsupported Arrow input type")),
        };
        arrays.push(array);
    }
    RecordBatch::try_new(Arc::new(Schema::new(fields)), arrays)
        .map_err(|_| error("TypeError", "Canonical rows do not match the projection schema"))
}

fn batches_to_rows(batches: &[RecordBatch]) -> Result<Vec<Value>> {
    let mut result = Vec::new();
    for batch in batches {
        if result.len().saturating_add(batch.num_rows()) > MAX_RESULT_ROWS {
            return Err(error("ResourceLimit", "Query result exceeds 100000 rows; add LIMIT"));
        }
        let schema = batch.schema();
        let mut names = BTreeSet::new();
        for field in schema.fields() {
            if !names.insert(field.name()) { return Err(error("DuplicateColumn", "Use aliases for duplicate result columns")); }
        }
        for index in 0..batch.num_rows() {
            let mut row = Map::new();
            for (column, field) in batch.columns().iter().zip(schema.fields()) {
                let mut value = array_value(column, index)?;
                if field.metadata().get("kurasql.logical_type").is_some_and(|kind| matches!(kind.as_str(), "json" | "jsonb")) {
                    if let Value::String(encoded) = value {
                        value = serde_json::from_str(&encoded).map_err(|_| error("TypeError", "Invalid projected JSON value"))?;
                    }
                }
                row.insert(field.name().clone(), value);
            }
            result.push(Value::Object(row));
        }
    }
    Ok(result)
}

fn array_value(array: &ArrayRef, row: usize) -> Result<Value> {
    if array.is_null(row) { return Ok(Value::Null); }
    macro_rules! value {
        ($array:ty) => { array.as_any().downcast_ref::<$array>().ok_or_else(|| error("ExecutionFailure", "Invalid Arrow array"))?.value(row) };
    }
    Ok(match array.data_type() {
        DataType::Null => Value::Null,
        DataType::Boolean => Value::Bool(value!(BooleanArray)),
        DataType::Int8 => Value::from(value!(Int8Array)),
        DataType::Int16 => Value::from(value!(Int16Array)),
        DataType::Int32 => Value::from(value!(Int32Array)),
        DataType::Int64 => Value::from(value!(Int64Array)),
        DataType::UInt8 => Value::from(value!(UInt8Array)),
        DataType::UInt16 => Value::from(value!(UInt16Array)),
        DataType::UInt32 => Value::from(value!(UInt32Array)),
        DataType::UInt64 => Value::from(value!(UInt64Array)),
        DataType::Float32 => Value::Number(Number::from_f64(value!(Float32Array) as f64).ok_or_else(|| error("TypeError", "Non-finite query result"))?),
        DataType::Float64 => Value::Number(Number::from_f64(value!(Float64Array)).ok_or_else(|| error("TypeError", "Non-finite query result"))?),
        DataType::Utf8 => Value::String(value!(StringArray).to_owned()),
        DataType::LargeUtf8 => Value::String(value!(LargeStringArray).to_owned()),
        DataType::Utf8View => Value::String(value!(StringViewArray).to_owned()),
        other => return Err(unsupported(format!("Query result type {other} is not supported"))),
    })
}
