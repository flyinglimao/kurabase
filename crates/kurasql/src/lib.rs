//! Deterministic, serializable relational state and atomic SQL mutations.
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sqlparser::{ast::*, dialect::PostgreSqlDialect, parser::Parser};
use std::collections::{BTreeMap, BTreeSet};
mod functions;
mod schema;
mod deferred;

pub type StableId = u64;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, thiserror::Error)]
#[error("{code}: {message}")]
pub struct SqlError { pub code: String, pub message: String }
pub type Result<T> = std::result::Result<T, SqlError>;
fn err(code: &str, message: impl Into<String>) -> SqlError { SqlError { code: code.into(), message: message.into() } }
fn unsupported(message: impl Into<String>) -> SqlError { err("UnsupportedFeature", message) }

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AuthContext {
    pub role: String,
    pub uid: Option<String>,
    pub jwt_claims: Value,
    pub privileged: bool,
}
fn factor_name(factor: &TableFactor) -> Result<String> {
    match factor { TableFactor::Table { name, args: None, .. } => table_name(name), _ => Err(unsupported("Derived tables and table functions require the query backend")) }
}
fn require_column(table: &Table, column: &str) -> Result<()> {
    if table.columns.iter().any(|c| c.name == column) { Ok(()) } else { Err(err("UndefinedColumn", format!("{}.{}", table.name, column))) }
}
fn row_json(values: &BTreeMap<String, Value>) -> Value { Value::Object(values.iter().map(|(k,v)| (k.clone(),v.clone())).collect()) }
fn check_type_supported(data_type: &str) -> Result<()> {
    let base = data_type.split('(').next().unwrap_or(data_type);
    match base { "smallint" | "int2" | "int" | "integer" | "int4" | "bigint" | "int8" | "text" | "varchar" | "character varying" | "boolean" | "bool" | "uuid" | "json" | "jsonb" => Ok(()), _ => Err(unsupported(format!("Data type {data_type} is not implemented in the canonical mutation engine"))) }
}
fn check_value(column: &Column, value: &Value) -> Result<()> {
    if value.is_null() { return if column.nullable { Ok(()) } else { Err(err("ConstraintViolation", format!("{} is NOT NULL", column.name))) }; }
    let base = column.data_type.split('(').next().unwrap_or(&column.data_type);
    let valid = match base {
        "smallint" | "int2" => value.as_i64().is_some_and(|n| i16::try_from(n).is_ok()),
        "int" | "integer" | "int4" => value.as_i64().is_some_and(|n| i32::try_from(n).is_ok()),
        "bigint" | "int8" => value.as_i64().is_some(),
        "text" | "varchar" | "character varying" => value.is_string(),
        "boolean" | "bool" => value.is_boolean(),
        "uuid" => value.as_str().is_some_and(|s| s.len() == 36 && s.chars().enumerate().all(|(i,c)| if [8,13,18,23].contains(&i) { c == '-' } else { c.is_ascii_hexdigit() })),
        "json" | "jsonb" => true,
        _ => false,
    };
    if !valid { return Err(err("TypeError", format!("Invalid {} value for column {}", column.data_type, column.name))); }
    if matches!(base, "varchar" | "character varying") {
        if let Some(length) = column.data_type.split('(').nth(1).and_then(|s| s.trim_end_matches(')').parse::<usize>().ok()) {
            if value.as_str().is_some_and(|s| s.chars().count() > length) { return Err(err("TypeError", format!("{} exceeds length {length}", column.name))); }
        }
    }
    Ok(())
}
fn check_row(table: &Table, row: &BTreeMap<String, Value>, context: &AuthContext) -> Result<()> {
    for column in &table.columns { check_value(column, row.get(&column.name).unwrap_or(&Value::Null))?; }
    for check in &table.checks {
        if truth(eval(check, row, context)?)? == Some(false) { return Err(err("ConstraintViolation", format!("CHECK constraint on {} failed", table.name))); }
    }
    Ok(())
}
fn is_default(expr: &Expr) -> bool { matches!(expr,Expr::Identifier(id) if id.quote_style.is_none() && ident(id) == "default") }
fn identity_value(identity: &Identity, row_id: u64) -> Result<Value> {
    i64::try_from(row_id.saturating_sub(1)).ok().and_then(|id| id.checked_mul(identity.increment)).and_then(|n| n.checked_add(identity.start)).map(Value::from).ok_or_else(|| err("TypeError","Identity integer overflow"))
}
fn configure_generated(column: &mut Column, generated_as: GeneratedAs, sequence_options: Option<Vec<SequenceOptions>>, generation_expr: Option<Expr>, context: &AuthContext) -> Result<()> {
    if column.generated.is_some() || column.identity.is_some() { return Err(err("SyntaxError","Duplicate generated column declaration")); }
    if let Some(expr) = generation_expr {
        if sequence_options.is_some() { return Err(unsupported("Generated expression sequence options")); }
        column.generated = Some(expr);
    } else {
        if !["smallint","int2","int","integer","int4","bigint","int8"].contains(&column.data_type.as_str()) { return Err(err("TypeError","Identity requires an integer column")); }
        let mut identity = Identity { always: matches!(generated_as,GeneratedAs::Always), start: 1, increment: 1 };
        for option in sequence_options.unwrap_or_default() {
            match option {
                SequenceOptions::StartWith(expr,_) => identity.start = eval(&expr,&BTreeMap::new(),context)?.as_i64().ok_or_else(|| err("TypeError","Identity START requires integer"))?,
                SequenceOptions::IncrementBy(expr,_) => identity.increment = eval(&expr,&BTreeMap::new(),context)?.as_i64().filter(|n| *n != 0).ok_or_else(|| err("TypeError","Identity INCREMENT requires a nonzero integer"))?,
                SequenceOptions::MinValue(MinMaxValue::Empty) | SequenceOptions::MaxValue(MinMaxValue::Empty) => {},
                _ => return Err(unsupported("Identity MIN/MAX/CACHE/CYCLE options")),
            }
        }
        column.identity = Some(identity); column.nullable = false;
    }
    Ok(())
}
fn recompute_generated(table: &Table, row: &mut BTreeMap<String,Value>, context: &AuthContext) -> Result<()> {
    let base = row.clone();
    for column in &table.columns { if let Some(expr) = &column.generated { row.insert(column.name.clone(),eval(expr,&base,context)?); } }
    Ok(())
}
fn assign_value(table: &Table, row: &mut BTreeMap<String,Value>, name: &str, expr: &Expr, scope: &BTreeMap<String,Value>, context: &AuthContext) -> Result<()> {
    let column = table.columns.iter().find(|c| c.name == name).ok_or_else(|| err("UndefinedColumn",name))?;
    if column.generated.is_some() { return if is_default(expr) { Ok(()) } else { Err(err("ConstraintViolation",format!("{name} is GENERATED ALWAYS"))) }; }
    if column.identity.is_some() && is_default(expr) { return Err(unsupported("UPDATE identity DEFAULT requires an independent identity allocator")); }
    if column.identity.as_ref().is_some_and(|id| id.always) { return Err(err("ConstraintViolation",format!("{name} is GENERATED ALWAYS"))); }
    let value = if is_default(expr) { column.default.as_ref().map(|expr| eval(expr,&BTreeMap::new(),context)).transpose()?.unwrap_or(Value::Null) } else { eval(expr,scope,context)? };
    row.insert(name.into(),value); Ok(())
}

impl Database {
    pub fn validate(&self, context: &AuthContext) -> Result<()> {
        self.validate_constraints(context,true)
    }
    fn validate_immediate(&self, context: &AuthContext) -> Result<()> {
        self.validate_constraints(context,false)
    }
    fn validate_constraints(&self, context: &AuthContext, final_check: bool) -> Result<()> {
        for table in self.catalog.values() {
            let sample: BTreeMap<String, Value> = table.columns.iter().map(|c| (c.name.clone(),Value::Null)).collect();
            for column in &table.columns {
                if let Some(expr) = &column.generated {
                    let bad = visit_expressions(expr,|expr| {
                        let invalid = match expr { Expr::Identifier(id) => table.columns.iter().any(|c| c.name == ident(id) && c.generated.is_some()), Expr::Function(f) => f.name.to_string().to_lowercase() != "coalesce", _ => false };
                        if invalid { std::ops::ControlFlow::Break(()) } else { std::ops::ControlFlow::Continue(()) }
                    });
                    if matches!(bad,std::ops::ControlFlow::Break(())) { return Err(unsupported("Generated expressions must be immutable and cannot reference generated columns")); }
                    eval(expr,&sample,context)?;
                }
            }
            for check in &table.checks { truth(eval(check,&sample,context)?)?; }
            for policy in &table.policies { for expr in policy.using.iter().chain(policy.check.iter()) { truth(eval(expr,&sample,context)?)?; } }
            for key in table.unique.iter().chain(std::iter::once(&table.primary_key)).filter(|key| !key.is_empty()) {
                for name in key { require_column(table, name)?; }
                if !final_check && table.deferred_unique.contains(key) { continue; }
                let mut seen = BTreeSet::new();
                for row in table.rows.values() {
                    let values: Vec<&Value> = key.iter().map(|name| &row.values[name]).collect();
                    if values.iter().any(|v| v.is_null()) { continue; }
                    if !seen.insert(serde_json::to_string(&values).map_err(|e| err("ExecutionFailure", e.to_string()))?) { return Err(err("ConstraintViolation", format!("Duplicate key in {} ({})", table.name, key.join(",")))); }
                }
            }
            for row in table.rows.values() { check_row(table, &row.values, context)?; }
            for fk in &table.foreign_keys {
                let target = self.table(&fk.foreign_table)?;
                let refs = if fk.referred_columns.is_empty() { &target.primary_key } else { &fk.referred_columns };
                if refs.is_empty() || refs.len() != fk.columns.len() { return Err(err("ConstraintViolation", "Foreign-key arity differs")); }
                for col in &fk.columns { require_column(table, col)?; }
                for col in refs { require_column(target, col)?; }
                if *refs != target.primary_key && !target.unique.contains(refs) { return Err(err("ConstraintViolation", "Foreign key target must be a primary or unique key")); }
                if !final_check && fk.deferred { continue; }
                for row in table.rows.values() {
                    let values: Vec<&Value> = fk.columns.iter().map(|c| &row.values[c]).collect();
                    if values.iter().any(|v| v.is_null()) { continue; }
                    if !target.rows.values().any(|r| refs.iter().map(|c| &r.values[c]).eq(values.iter().copied())) { return Err(err("ConstraintViolation", format!("Foreign key from {} to {} failed", table.name, target.name))); }
                }
            }
        }
        Ok(())
    }

    fn select(&self, query: &Query, context: &AuthContext) -> Result<SqlResult> {
        if query.with.is_some() || query.fetch.is_some() || !query.locks.is_empty() { return Err(unsupported("CTE/FETCH/locking require the query backend")); }
        let select = match &*query.body { SetExpr::Select(select) => select, _ => return Err(unsupported("Set operations require the query backend")) };
        if select.from.len() > 1 || select.from.iter().any(|f| !f.joins.is_empty()) || select.having.is_some() || !select.lateral_views.is_empty() || !select.named_window.is_empty() || select.qualify.is_some() || select.top.is_some() || select.into.is_some() {
            return Err(unsupported("JOIN/aggregation/windows require the query backend"));
        }
        match &select.group_by { GroupByExpr::Expressions(e) if e.is_empty() => {}, _ => return Err(unsupported("GROUP BY requires the query backend")) }
        let mut source = Vec::new();
        if let Some(from) = select.from.first() {
            let table = self.table(&factor_name(&from.relation)?)?;
            let sample: BTreeMap<String, Value> = table.columns.iter().map(|c| (c.name.clone(), Value::Null)).collect();
            project(&select.projection, &sample, context)?;
            predicate(select.selection.as_ref(), &sample, context)?;
            for row in table.rows.values() {
                if allowed(table, "SELECT", &row.values, context, false)? && predicate(select.selection.as_ref(), &row.values, context)? { source.push(row.values.clone()); }
            }
        } else if predicate(select.selection.as_ref(), &BTreeMap::new(), context)? { source.push(BTreeMap::new()); }
        let mut ordered = Vec::new();
        for values in source {
            let result = project(&select.projection, &values, context)?;
            let mut order_values = Vec::new();
            for order in &query.order_by {
                let mut scope = values.clone();
                if let Some(map) = result.as_object() { scope.extend(map.iter().map(|(k,v)| (k.clone(),v.clone()))); }
                order_values.push(eval(&order.expr, &scope, context)?);
            }
            ordered.push((result, order_values));
        }
        // Validate ordering types before using an infallible sorting comparator.
        for index in 0..query.order_by.len() {
            let mut previous = None;
            for (_, values) in &ordered { if !values[index].is_null() { if let Some(ref p) = previous { compare(p, &values[index])?; } previous = Some(values[index].clone()); } }
        }
        ordered.sort_by(|a,b| {
            for (index, order) in query.order_by.iter().enumerate() {
                let (a,b) = (&a.1[index], &b.1[index]);
                let asc = order.asc.unwrap_or(true);
                let nulls_first = order.nulls_first.unwrap_or(!asc);
                let cmp = match (a.is_null(), b.is_null()) {
                    (true,true) => std::cmp::Ordering::Equal,
                    (true,false) => if nulls_first { std::cmp::Ordering::Less } else { std::cmp::Ordering::Greater },
                    (false,true) => if nulls_first { std::cmp::Ordering::Greater } else { std::cmp::Ordering::Less },
                    _ => { let c = compare(a,b).expect("ordering types were validated"); if asc { c } else { c.reverse() } },
                };
                if !cmp.is_eq() { return cmp; }
            }
            std::cmp::Ordering::Equal
        });
        let mut rows: Vec<Value> = ordered.into_iter().map(|(r,_)| r).collect();
        if let Some(distinct) = &select.distinct {
            if !matches!(distinct, Distinct::Distinct) { return Err(unsupported("DISTINCT ON")); }
            let mut seen = BTreeSet::new(); rows.retain(|r| seen.insert(r.to_string()));
        }
        let offset = query.offset.as_ref().map(|o| nonnegative(&o.value, context)).transpose()?.unwrap_or(0);
        let limit = query.limit.as_ref().map(|e| nonnegative(e, context)).transpose()?.unwrap_or(usize::MAX);
        rows = rows.into_iter().skip(offset).take(limit).collect();
        Ok(SqlResult { affected: 0, rows })
    }
}
fn nonnegative(expr: &Expr, context: &AuthContext) -> Result<usize> { eval(expr, &BTreeMap::new(), context)?.as_u64().and_then(|n| usize::try_from(n).ok()).ok_or_else(|| err("TypeError", "LIMIT/OFFSET must be a nonnegative integer")) }
fn project(projection: &[SelectItem], values: &BTreeMap<String, Value>, context: &AuthContext) -> Result<Value> {
    let mut out = Map::new();
    for item in projection {
        match item {
            SelectItem::Wildcard(_) => out.extend(values.iter().map(|(k,v)| (k.clone(),v.clone()))),
            SelectItem::ExprWithAlias { expr, alias } => { out.insert(ident(alias), eval(expr, values, context)?); },
            SelectItem::UnnamedExpr(expr) => {
                let name = match expr { Expr::Identifier(id) => ident(id), Expr::CompoundIdentifier(ids) => ident(ids.last().unwrap()), _ => expr.to_string() };
                if out.insert(name.clone(), eval(expr, values, context)?).is_some() { return Err(unsupported(format!("Duplicate result column {name}; use aliases"))); }
            }
            _ => return Err(unsupported("Qualified wildcard")),
        }
    }
    Ok(Value::Object(out))
}
fn truth(value: Value) -> Result<Option<bool>> { match value { Value::Null => Ok(None), Value::Bool(b) => Ok(Some(b)), _ => Err(err("TypeError", "Predicate requires boolean")) } }
fn predicate(expr: Option<&Expr>, row: &BTreeMap<String, Value>, context: &AuthContext) -> Result<bool> { match expr { None => Ok(true), Some(expr) => Ok(truth(eval(expr, row, context)?)? == Some(true)) } }
fn compare(a: &Value, b: &Value) -> Result<std::cmp::Ordering> {
    match (a,b) {
        (Value::Number(a), Value::Number(b)) => match (a.as_i64(), b.as_i64()) { (Some(a),Some(b)) => Ok(a.cmp(&b)), _ => Err(unsupported("Non-integer numeric comparison")) },
        (Value::String(a), Value::String(b)) => Ok(a.cmp(b)),
        (Value::Bool(a), Value::Bool(b)) => Ok(a.cmp(b)),
        _ => Err(err("TypeError", "Incompatible comparison operands")),
    }
}
pub fn eval(expr: &Expr, row: &BTreeMap<String, Value>, context: &AuthContext) -> Result<Value> {
    match expr {
        Expr::Value(value) => match value {
            sqlparser::ast::Value::Null => Ok(Value::Null),
            sqlparser::ast::Value::Boolean(b) => Ok(Value::Bool(*b)),
            sqlparser::ast::Value::SingleQuotedString(s) => Ok(Value::String(s.clone())),
            sqlparser::ast::Value::Number(s, _) => s.parse::<i64>().map(Value::from).map_err(|_| unsupported("Only signed 64-bit integer literals are implemented")),
            _ => Err(unsupported(format!("Literal {value}"))),
        },
        Expr::Identifier(id) => row.get(&ident(id)).cloned().ok_or_else(|| err("UndefinedColumn", ident(id))),
        Expr::CompoundIdentifier(ids) => row.get(&ids.iter().map(ident).collect::<Vec<_>>().join(".")).cloned().ok_or_else(|| unsupported("Qualified expression requires the query backend")),
        Expr::Nested(expr) => eval(expr, row, context),
        Expr::IsNull(expr) => Ok(Value::Bool(eval(expr, row, context)?.is_null())),
        Expr::IsNotNull(expr) => Ok(Value::Bool(!eval(expr, row, context)?.is_null())),
        Expr::IsTrue(expr) => Ok(Value::Bool(truth(eval(expr, row, context)?)? == Some(true))),
        Expr::IsFalse(expr) => Ok(Value::Bool(truth(eval(expr, row, context)?)? == Some(false))),
        Expr::UnaryOp { op, expr } => {
            let v = eval(expr, row, context)?;
            if v.is_null() { return Ok(v); }
            match op {
                UnaryOperator::Not => Ok(Value::Bool(!truth(v)?.unwrap())),
                UnaryOperator::Plus => if v.as_i64().is_some() { Ok(v) } else { Err(err("TypeError", "Unary plus requires integer")) },
                UnaryOperator::Minus => v.as_i64().and_then(i64::checked_neg).map(Value::from).ok_or_else(|| err("TypeError", "Integer negation overflow or incompatible type")),
                _ => Err(unsupported(format!("Unary operator {op}"))),
            }
        }
        Expr::BinaryOp { left, op, right } => {
            let a = eval(left, row, context)?; let b = eval(right, row, context)?;
            if matches!(op, BinaryOperator::And | BinaryOperator::Or) {
                let (a,b) = (truth(a)?, truth(b)?);
                let result = if matches!(op, BinaryOperator::And) { if a == Some(false) || b == Some(false) { Some(false) } else if a == Some(true) && b == Some(true) { Some(true) } else { None } } else if a == Some(true) || b == Some(true) { Some(true) } else if a == Some(false) && b == Some(false) { Some(false) } else { None };
                return Ok(result.map(Value::Bool).unwrap_or(Value::Null));
            }
            if a.is_null() || b.is_null() { return Ok(Value::Null); }
            match op {
                BinaryOperator::Eq => Ok(Value::Bool(compare(&a,&b)?.is_eq())),
                BinaryOperator::NotEq => Ok(Value::Bool(!compare(&a,&b)?.is_eq())),
                BinaryOperator::Gt => Ok(Value::Bool(compare(&a,&b)?.is_gt())),
                BinaryOperator::GtEq => Ok(Value::Bool(!compare(&a,&b)?.is_lt())),
                BinaryOperator::Lt => Ok(Value::Bool(compare(&a,&b)?.is_lt())),
                BinaryOperator::LtEq => Ok(Value::Bool(!compare(&a,&b)?.is_gt())),
                BinaryOperator::Plus | BinaryOperator::Minus | BinaryOperator::Multiply | BinaryOperator::Divide | BinaryOperator::Modulo => {
                    let (a,b) = (a.as_i64().ok_or_else(|| err("TypeError", "Arithmetic requires integers"))?, b.as_i64().ok_or_else(|| err("TypeError", "Arithmetic requires integers"))?);
                    let result = match op { BinaryOperator::Plus => a.checked_add(b), BinaryOperator::Minus => a.checked_sub(b), BinaryOperator::Multiply => a.checked_mul(b), BinaryOperator::Divide => a.checked_div(b), _ => a.checked_rem(b) };
                    result.map(Value::from).ok_or_else(|| err("TypeError", "Integer overflow or division by zero"))
                }
                BinaryOperator::StringConcat => match (a.as_str(), b.as_str()) { (Some(a),Some(b)) => Ok(Value::String(format!("{a}{b}"))), _ => Err(err("TypeError", "Concatenation requires text")) },
                _ => Err(unsupported(format!("Binary operator {op}"))),
            }
        }
        Expr::InList { expr, list, negated } => {
            let value = eval(expr, row, context)?;
            let mut unknown = value.is_null();
            for item in list { let item = eval(item, row, context)?; if item.is_null() { unknown = true; } else if !value.is_null() && compare(&value,&item)?.is_eq() { return Ok(Value::Bool(!negated)); } }
            Ok(if unknown { Value::Null } else { Value::Bool(*negated) })
        }
        Expr::Between { expr, negated, low, high } => {
            let value = eval(expr, row, context)?; let low = eval(low, row, context)?; let high = eval(high, row, context)?;
            if value.is_null() || low.is_null() || high.is_null() { Ok(Value::Null) } else { Ok(Value::Bool((!compare(&value,&low)?.is_lt() && !compare(&value,&high)?.is_gt()) != *negated)) }
        }
        Expr::Function(function) => {
            if function.over.is_some() || function.filter.is_some() || function.null_treatment.is_some() || function.distinct || !function.order_by.is_empty() { return Err(unsupported("Function modifiers require the query backend")); }
            let name = function.name.to_string().to_lowercase();
            match name.as_str() {
                "auth.uid" if function.args.is_empty() => Ok(context.uid.clone().map(Value::String).unwrap_or(Value::Null)),
                "auth.jwt" if function.args.is_empty() => Ok(context.jwt_claims.clone()),
                "coalesce" => { for arg in &function.args { match arg { FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) => { let value = eval(expr,row,context)?; if !value.is_null() { return Ok(value); } }, _ => return Err(unsupported("Function argument")) } } Ok(Value::Null) },
                _ => Err(unsupported(format!("Function {name}"))),
            }
        }
        Expr::Cast { expr, data_type, .. } => {
            let value = eval(expr, row, context)?; if value.is_null() { return Ok(value); }
            let target = data_type.to_string().to_lowercase(); check_type_supported(&target)?;
            let value = match target.as_str() {
                "text" => Value::String(match value { Value::String(s) => s, other => other.to_string() }),
                "int" | "integer" | "bigint" | "smallint" => if let Some(s) = value.as_str() { s.parse::<i64>().map(Value::from).map_err(|_| err("TypeError", "Invalid integer CAST"))? } else { value },
                "json" | "jsonb" => if let Some(s) = value.as_str() { serde_json::from_str(s).map_err(|_| err("TypeError", "Invalid JSON CAST"))? } else { value },
                _ => value,
            };
            check_value(&Column { id: 0, name: "cast".into(), data_type: target, nullable: true, default: None, identity: None, generated: None }, &value)?; Ok(value)
        }
        Expr::Like { negated, expr: value, pattern, escape_char } | Expr::ILike { negated, expr: value, pattern, escape_char } => {
            let value = eval(value,row,context)?; let pattern = eval(pattern,row,context)?;
            if value.is_null() || pattern.is_null() { return Ok(Value::Null); }
            let value = value.as_str().ok_or_else(|| err("TypeError", "LIKE requires text"))?;
            let pattern = pattern.as_str().ok_or_else(|| err("TypeError", "LIKE pattern requires text"))?;
            let insensitive = matches!(expr, Expr::ILike { .. });
            let value = if insensitive { value.to_lowercase() } else { value.to_owned() };
            let pattern = if insensitive { pattern.to_lowercase() } else { pattern.to_owned() };
            Ok(Value::Bool(like(&value,&pattern,escape_char.unwrap_or('\\'))? != *negated))
        }
        Expr::Case { operand, conditions, results, else_result } => {
            for (condition,result) in conditions.iter().zip(results) {
                let matches = if let Some(operand) = operand { let a = eval(operand,row,context)?; let b = eval(condition,row,context)?; !a.is_null() && !b.is_null() && compare(&a,&b)?.is_eq() } else { predicate(Some(condition),row,context)? };
                if matches { return eval(result,row,context); }
            }
            else_result.as_ref().map(|e| eval(e,row,context)).unwrap_or(Ok(Value::Null))
        }
        _ => Err(unsupported(format!("Expression {expr}"))),
    }
}
fn like(value: &str, pattern: &str, escape: char) -> Result<bool> {
    let chars: Vec<char> = value.chars().collect();
    let mut matches = vec![false; chars.len()+1]; matches[0] = true;
    let mut pattern = pattern.chars();
    while let Some(mut token) = pattern.next() {
        let escaped = token == escape;
        if escaped { token = pattern.next().ok_or_else(|| err("SyntaxError", "LIKE pattern ends with escape"))?; }
        let mut next = vec![false; chars.len()+1];
        if token == '%' && !escaped { next[0] = matches[0]; for i in 1..next.len() { next[i] = matches[i] || next[i-1]; } }
        else { for i in 1..next.len() { next[i] = matches[i-1] && (token == '_' && !escaped || chars[i-1] == token); } }
        matches = next;
    }
    Ok(matches[chars.len()])
}

fn allowed(table: &Table, command: &str, row: &BTreeMap<String, Value>, context: &AuthContext, check: bool) -> Result<bool> {
    if context.privileged || !table.rls_enabled { return Ok(true); }
    let mut permissive = false;
    let mut restrictive = true;
    for policy in &table.policies {
        if policy.command != "ALL" && policy.command != command { continue; }
        if !policy.roles.iter().any(|role| role == "public" || role == &context.role) { continue; }
        let expr = if check { policy.check.as_ref().or(policy.using.as_ref()) } else { policy.using.as_ref() };
        let matched = predicate(expr,row,context)?;
        if policy.permissive { permissive |= matched; } else { restrictive &= matched; }
    }
    Ok(permissive && restrictive)
}

fn tokens(sql: &str) -> Result<Vec<String>> {
    sqlparser::tokenizer::Tokenizer::new(&PostgreSqlDialect {}, sql).with_unescape(false).tokenize().map(|tokens| tokens.into_iter().filter(|t| !matches!(t, sqlparser::tokenizer::Token::Whitespace(_))).map(|t| t.to_string()).collect()).map_err(|e| err("SyntaxError", e.to_string()))
}
fn split_statements(sql: &str) -> Result<Vec<String>> {
    let mut out = Vec::new(); let mut current = Vec::new();
    for token in tokens(sql)? { if token == ";" { if !current.is_empty() { out.push(current.join(" ")); current.clear(); } } else { current.push(token); } }
    if !current.is_empty() { out.push(current.join(" ")); }
    Ok(out)
}
fn parse_expression(sql: &str) -> Result<Expr> {
    let mut statements = Parser::parse_sql(&PostgreSqlDialect {}, &format!("SELECT {sql}")).map_err(|e| err("SyntaxError", e.to_string()))?;
    match statements.remove(0) { Statement::Query(query) => match *query.body { SetExpr::Select(select) if select.projection.len() == 1 && select.from.is_empty() => match select.projection.into_iter().next().unwrap() { SelectItem::UnnamedExpr(expr) => Ok(expr), _ => Err(err("SyntaxError", "Expected policy expression")) }, _ => Err(err("SyntaxError", "Expected policy expression")) }, _ => Err(err("SyntaxError", "Expected policy expression")) }
}
impl Database {
    fn policy_statement(&mut self, sql: &str, context: &AuthContext) -> Result<Option<SqlResult>> {
        let words = tokens(sql)?;
        let upper: Vec<String> = words.iter().map(|s| s.to_uppercase()).collect();
        let is_policy = upper.starts_with(&["CREATE".into(), "POLICY".into()]);
        let is_rls = upper.starts_with(&["ALTER".into(), "TABLE".into()]) && upper.windows(3).any(|w| w == ["ROW", "LEVEL", "SECURITY"]);
        if !is_policy && !is_rls { return Ok(None); }
        if !context.privileged { return Err(err("PrivilegeError", "Policy DDL requires administrative authority")); }
        if is_rls {
            let action = upper.iter().position(|s| s == "ENABLE" || s == "DISABLE").ok_or_else(|| unsupported("RLS ALTER action"))?;
            if action < 3 || upper[action+1..] != ["ROW", "LEVEL", "SECURITY"] { return Err(err("SyntaxError", "Expected ENABLE/DISABLE ROW LEVEL SECURITY")); }
            let name = parse_relation(&words[2..action].join(" "))?;
            let table = self.catalog.get_mut(&name).ok_or_else(|| err("UndefinedTable", name.clone()))?;
            table.rls_enabled = upper[action] == "ENABLE";
            self.schema_version += 1;
            return Ok(Some(SqlResult::default()));
        }
        if words.len() < 5 || upper[3] != "ON" { return Err(err("SyntaxError", "Expected CREATE POLICY name ON table")); }
        let name = words[2].trim_matches('"').to_string();
        let end = (4..words.len()).find(|&i| ["AS","FOR","TO","USING","WITH"].contains(&upper[i].as_str())).unwrap_or(words.len());
        let table_name = parse_relation(&words[4..end].join(" "))?;
        let mut policy = Policy { name: name.clone(), command: "ALL".into(), roles: vec!["public".into()], permissive: true, using: None, check: None };
        let mut index = end;
        let mut clauses = BTreeSet::new();
        while index < words.len() {
            let clause = upper[index].clone(); if !clauses.insert(clause.clone()) { return Err(err("SyntaxError", "Duplicate policy clause")); }
            index += 1;
            match clause.as_str() {
                "AS" => { match upper.get(index).map(String::as_str) { Some("PERMISSIVE") => policy.permissive = true, Some("RESTRICTIVE") => policy.permissive = false, _ => return Err(err("SyntaxError", "Expected PERMISSIVE or RESTRICTIVE")) } index += 1; }
                "FOR" => { let command = upper.get(index).ok_or_else(|| err("SyntaxError", "Missing policy command"))?; if !["ALL","SELECT","INSERT","UPDATE","DELETE"].contains(&command.as_str()) { return Err(unsupported("Policy command")); } policy.command = command.clone(); index += 1; }
                "TO" => { policy.roles.clear(); while index < words.len() && !["USING","WITH"].contains(&upper[index].as_str()) { if words[index] != "," { policy.roles.push(words[index].trim_matches('"').to_lowercase()); } index += 1; } if policy.roles.is_empty() { return Err(err("SyntaxError", "Missing policy role")); } }
                "USING" | "WITH" => {
                    if clause == "WITH" { if upper.get(index).map(String::as_str) != Some("CHECK") { return Err(err("SyntaxError", "Expected WITH CHECK")); } index += 1; }
                    if words.get(index).map(String::as_str) != Some("(") { return Err(err("SyntaxError", "Policy expression must be parenthesized")); }
                    index += 1; let start = index; let mut depth = 1;
                    while index < words.len() { if words[index] == "(" { depth += 1; } if words[index] == ")" { depth -= 1; if depth == 0 { break; } } index += 1; }
                    if depth != 0 { return Err(err("SyntaxError", "Unclosed policy expression")); }
                    let expr = parse_expression(&words[start..index].join(" "))?;
                    if clause == "USING" { policy.using = Some(expr); } else { policy.check = Some(expr); }
                    index += 1;
                }
                _ => return Err(err("SyntaxError", format!("Unexpected policy clause {clause}"))),
            }
        }
        if policy.command == "INSERT" && policy.using.is_some() || ["SELECT","DELETE"].contains(&policy.command.as_str()) && policy.check.is_some() { return Err(err("SyntaxError", "Policy clause is invalid for this command")); }
        let table = self.catalog.get_mut(&table_name).ok_or_else(|| err("UndefinedTable", table_name.clone()))?;
        if table.policies.iter().any(|p| p.name == name) { return Err(err("DuplicatePolicy", name)); }
        table.policies.push(policy); self.schema_version += 1;
        self.validate(context)?;
        Ok(Some(SqlResult::default()))
    }
}
fn parse_relation(sql: &str) -> Result<String> {
    let mut statements = Parser::parse_sql(&PostgreSqlDialect {}, &format!("SELECT * FROM {sql}")).map_err(|e| err("SyntaxError",e.to_string()))?;
    match statements.remove(0) { Statement::Query(query) => match *query.body { SetExpr::Select(select) if select.from.len() == 1 && select.from[0].joins.is_empty() => factor_name(&select.from[0].relation), _ => Err(err("SyntaxError", "Expected table name")) }, _ => Err(err("SyntaxError", "Expected table name")) }
}
impl Default for AuthContext {
    fn default() -> Self { Self { role: "anon".into(), uid: None, jwt_claims: Value::Null, privileged: false } }
}
impl AuthContext {
    pub fn admin() -> Self { Self { role: "service_role".into(), privileged: true, ..Self::default() } }
    pub fn authenticated(uid: impl Into<String>) -> Self { Self { role: "authenticated".into(), uid: Some(uid.into()), ..Self::default() } }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Column {
    pub id: StableId,
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    pub default: Option<Expr>,
    #[serde(default)] pub identity: Option<Identity>,
    #[serde(default)] pub generated: Option<Expr>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Identity { pub always: bool, pub start: i64, pub increment: i64 }
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Row { pub id: StableId, pub version: u64, pub values: BTreeMap<String, Value> }
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ForeignKey {
    pub columns: Vec<String>, pub foreign_table: String, pub referred_columns: Vec<String>,
    #[serde(default)] pub on_delete: Option<ReferentialAction>,
    #[serde(default)] pub on_update: Option<ReferentialAction>,
    #[serde(default)] pub deferred: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Policy {
    pub name: String,
    pub command: String,
    pub roles: Vec<String>,
    pub permissive: bool,
    pub using: Option<Expr>,
    pub check: Option<Expr>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Table {
    pub id: StableId,
    pub name: String,
    pub columns: Vec<Column>,
    pub primary_key: Vec<String>,
    pub unique: Vec<Vec<String>>,
    #[serde(default)] pub deferred_unique: BTreeSet<Vec<String>>,
    pub foreign_keys: Vec<ForeignKey>,
    pub checks: Vec<Expr>,
    pub rls_enabled: bool,
    pub policies: Vec<Policy>,
    pub rows: BTreeMap<StableId, Row>,
    pub next_row_id: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Database {
    pub catalog: BTreeMap<String, Table>,
    #[serde(default)]
    pub views: BTreeMap<String, View>,
    #[serde(default)]
    pub functions: BTreeMap<String, SqlFunction>,
    pub migrations: BTreeSet<String>,
    pub schema_version: u64,
    pub next_object_id: u64,
}
impl Default for Database {
    fn default() -> Self { Self { catalog: BTreeMap::new(), views: BTreeMap::new(), functions: BTreeMap::new(), migrations: BTreeSet::new(), schema_version: 0, next_object_id: 1 } }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct View { pub sql: String, pub materialized: bool }
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FunctionParameter { pub name: String, pub data_type: String, pub default: Option<Expr> }
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum FunctionReturn { Scalar(String), SetOf(String), Table(Vec<(String,String)>), Void }
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SqlFunction {
    pub name: String,
    pub parameters: Vec<FunctionParameter>,
    pub return_kind: FunctionReturn,
    pub body: String,
    pub security_definer: bool,
    pub definer_context: Option<AuthContext>,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SqlResult { pub rows: Vec<Value>, pub affected: usize }

pub fn table_name(name: &ObjectName) -> Result<String> {
    match name.0.as_slice() {
        [table] => Ok(ident(table)),
        [schema, table] if ident(schema) == "public" => Ok(ident(table)),
        _ => Err(unsupported("Only the local public schema is supported")),
    }
}
fn ident(value: &Ident) -> String { if value.quote_style.is_some() { value.value.clone() } else { value.value.to_lowercase() } }
fn names(values: &[Ident]) -> Vec<String> { values.iter().map(ident).collect() }

impl Database {
    fn allocate(&mut self) -> u64 { let id = self.next_object_id; self.next_object_id += 1; id }
    pub fn table(&self, name: &str) -> Result<&Table> { self.catalog.get(name.strip_prefix("public.").unwrap_or(name)).ok_or_else(|| err("UndefinedTable", format!("Table {name} does not exist"))) }
    /// An entire SQL batch is one atomic unit; the original state survives every error.
    pub fn execute_sql(&mut self, sql: &str, context: &AuthContext) -> Result<SqlResult> {
        let mut staged = self.clone();
        let statements = split_statements(sql)?;
        let mut result = SqlResult::default();
        for sql in statements {
            if let Some(r) = staged.deferred_statement(&sql, context)? { result = r; continue; }
            if let Some(r) = staged.function_statement(&sql, context)? { result = r; continue; }
            if let Some(r) = staged.policy_statement(&sql, context)? { result = r; continue; }
            let ast = Parser::parse_sql(&PostgreSqlDialect {}, &sql).map_err(|e| err("SyntaxError", e.to_string()))?;
            for stmt in ast { let before = staged.clone(); result = staged.execute_statement(stmt, context)?; staged.apply_referential_actions(&before,context)?; staged.validate_immediate(context)?; }
        }
        staged.validate(context)?;
        *self = staged;
        Ok(result)
    }
    pub fn apply_migration(&mut self, version: &str, sql: &str, context: &AuthContext) -> Result<SqlResult> {
        if !context.privileged { return Err(err("PrivilegeError", "Migrations require administrative authority")); }
        if self.migrations.contains(version) { return Ok(SqlResult::default()); }
        let mut staged = self.clone();
        let result = staged.execute_sql(sql, context)?;
        staged.migrations.insert(version.into());
        *self = staged;
        Ok(result)
    }
    pub fn read_table(&self, name: &str, context: &AuthContext) -> Result<Vec<Value>> {
        let table = self.table(name)?;
        let mut rows = Vec::new();
        for row in table.rows.values() {
            if allowed(table, "SELECT", &row.values, context, false)? { rows.push(row_json(&row.values)); }
        }
        Ok(rows)
    }
    fn execute_statement(&mut self, stmt: Statement, context: &AuthContext) -> Result<SqlResult> {
        match stmt {
            Statement::AlterTable { name, if_exists, only, operations } => {
                if only { return Err(unsupported("ALTER TABLE ONLY")); }
                self.alter_table(&table_name(&name)?,if_exists,operations,context)
            }
            Statement::Drop { object_type: ObjectType::Table, names, if_exists, cascade, temporary, purge, .. } => {
                if temporary || purge { return Err(unsupported("DROP TABLE modifiers")); }
                self.drop_tables(names,if_exists,cascade,context)
            }
            Statement::CreateView { name, query, materialized, or_replace, if_not_exists, columns, temporary, with_options, cluster_by, with_no_schema_binding } => {
                if !context.privileged { return Err(err("PrivilegeError", "View DDL requires administrative authority")); }
                if !columns.is_empty() || temporary || !with_options.is_empty() || !cluster_by.is_empty() || with_no_schema_binding { return Err(unsupported("CREATE VIEW modifiers")); }
                let name = table_name(&name)?;
                if self.catalog.contains_key(&name) { return Err(err("DuplicateTable", name)); }
                if self.views.contains_key(&name) && !or_replace { return if if_not_exists { Ok(SqlResult::default()) } else { Err(err("DuplicateTable", name)) }; }
                self.views.insert(name, View { sql: query.to_string(), materialized }); self.schema_version += 1;
                Ok(SqlResult::default())
            }
            Statement::Drop { object_type: ObjectType::View, names, if_exists, cascade, .. } => {
                if !context.privileged { return Err(err("PrivilegeError", "View DDL requires administrative authority")); }
                for name in names { let name = table_name(&name)?; self.remove_dependents(&name,cascade)?; if self.views.remove(&name).is_none() && !if_exists { return Err(err("UndefinedTable",name)); } }
                self.schema_version += 1; Ok(SqlResult::default())
            }
            Statement::CreateIndex { table_name: name, unique, columns, predicate, .. } => {
                if !context.privileged { return Err(err("PrivilegeError", "Index DDL requires administrative authority")); }
                if unique { return Err(unsupported("CREATE UNIQUE INDEX; use a UNIQUE table constraint")); }
                let table = self.table(&table_name(&name)?)?;
                let sample = table.columns.iter().map(|c| (c.name.clone(),Value::Null)).collect();
                for col in columns { eval(&col.expr,&sample,context)?; }
                if let Some(predicate) = predicate { truth(eval(&predicate,&sample,context)?)?; }
                // Ordinary indexes carry only optimization semantics and may be physical no-ops.
                Ok(SqlResult::default())
            }
            Statement::CreateTable { name, columns, constraints, if_not_exists, query, like, clone, temporary, external, or_replace, transient, global, collation, on_commit, .. } => {
                if !context.privileged { return Err(err("PrivilegeError", "DDL requires administrative authority")); }
                if query.is_some() || like.is_some() || clone.is_some() || temporary || external || or_replace || transient || global.is_some() || collation.is_some() || on_commit.is_some() { return Err(unsupported("CREATE TABLE modifiers")); }
                let name = table_name(&name)?;
                if self.catalog.contains_key(&name) { return if if_not_exists { Ok(SqlResult::default()) } else { Err(err("DuplicateTable", name)) }; }
                let mut table = Table { id: self.allocate(), name: name.clone(), columns: Vec::new(), primary_key: Vec::new(), unique: Vec::new(), deferred_unique: BTreeSet::new(), foreign_keys: Vec::new(), checks: Vec::new(), rls_enabled: false, policies: Vec::new(), rows: BTreeMap::new(), next_row_id: 1 };
                for column in columns {
                    if column.collation.is_some() { return Err(unsupported("Column collation")); }
                    let cname = ident(&column.name);
                    if table.columns.iter().any(|c| c.name == cname) { return Err(err("DuplicateColumn", cname)); }
                    let data_type = column.data_type.to_string().to_lowercase();
                    check_type_supported(&data_type)?;
                    let mut col = Column { id: self.allocate(), name: cname.clone(), data_type, nullable: true, default: None, identity: None, generated: None };
                    for option in column.options {
                        match option.option {
                            ColumnOption::Null => col.nullable = true,
                            ColumnOption::NotNull => col.nullable = false,
                            ColumnOption::Default(expr) => col.default = Some(expr),
                            ColumnOption::Unique { is_primary } => {
                                if is_primary { if !table.primary_key.is_empty() { return Err(err("ConstraintViolation", "Multiple primary keys")); } table.primary_key = vec![cname.clone()]; col.nullable = false; }
                                else { table.unique.push(vec![cname.clone()]); }
                            }
                            ColumnOption::ForeignKey { foreign_table, referred_columns, on_delete, on_update } => {
                                table.foreign_keys.push(ForeignKey { columns: vec![cname.clone()], foreign_table: table_name(&foreign_table)?, referred_columns: names(&referred_columns), on_delete, on_update, deferred: false });
                            }
                            ColumnOption::Check(expr) => table.checks.push(expr),
                            ColumnOption::Generated { generated_as,sequence_options,generation_expr,.. } => configure_generated(&mut col,generated_as,sequence_options,generation_expr,context)?,
                            other => return Err(unsupported(format!("Column option {other}"))),
                        }
                    }
                    if col.identity.is_some() { col.nullable = false; }
                    if col.default.is_some() && (col.identity.is_some() || col.generated.is_some()) { return Err(err("SyntaxError","Generated/identity columns cannot also have a DEFAULT")); }
                    table.columns.push(col);
                }
                for constraint in constraints {
                    match constraint {
                        TableConstraint::Unique { columns, is_primary, .. } => {
                            if is_primary { if !table.primary_key.is_empty() { return Err(err("ConstraintViolation", "Multiple primary keys")); } table.primary_key = names(&columns); }
                            else { table.unique.push(names(&columns)); }
                        }
                        TableConstraint::ForeignKey { columns, foreign_table, referred_columns, on_delete, on_update, .. } => {
                            table.foreign_keys.push(ForeignKey { columns: names(&columns), foreign_table: table_name(&foreign_table)?, referred_columns: names(&referred_columns), on_delete, on_update, deferred: false });
                        }
                        TableConstraint::Check { expr, .. } => table.checks.push(*expr),
                        other => return Err(unsupported(format!("Constraint {other}"))),
                    }
                }
                for col in &mut table.columns { if table.primary_key.contains(&col.name) { col.nullable = false; } }
                self.catalog.insert(name, table);
                self.schema_version += 1;
                Ok(SqlResult::default())
            }
            Statement::Insert { table_name: name, columns, source, returning, on, or, ignore, overwrite, partitioned, after_columns, .. } => {
                if or.is_some() || ignore || overwrite || partitioned.is_some() || !after_columns.is_empty() { return Err(unsupported("INSERT modifiers")); }
                if source.as_ref().is_some_and(|q| q.with.is_some() || !q.order_by.is_empty() || q.limit.is_some() || q.offset.is_some() || q.fetch.is_some() || !q.locks.is_empty()) { return Err(unsupported("INSERT VALUES query modifiers")); }
                let name = table_name(&name)?;
                let mut table = self.table(&name)?.clone();
                let conflict = match &on { Some(OnInsert::OnConflict(c)) => Some(c), None => None, _ => return Err(unsupported("ON DUPLICATE KEY")) };
                let conflict_keys = match conflict.map(|c| &c.conflict_target) {
                    Some(Some(ConflictTarget::Columns(columns))) => {
                        let columns = names(columns);
                        if columns != table.primary_key && !table.unique.contains(&columns) { return Err(err("ConstraintViolation", "ON CONFLICT target must identify a unique or primary key")); }
                        vec![columns]
                    }
                    Some(Some(_)) => return Err(unsupported("ON CONFLICT ON CONSTRAINT")),
                    Some(None) => { if matches!(conflict.map(|c| &c.action), Some(OnConflictAction::DoUpdate(_))) { return Err(err("SyntaxError", "DO UPDATE requires a conflict target")); } table.unique.iter().chain(std::iter::once(&table.primary_key)).filter(|k| !k.is_empty()).cloned().collect() },
                    None => Vec::new(),
                };
                if conflict_keys.iter().any(|key| table.deferred_unique.contains(key)) { return Err(unsupported("Deferred constraints cannot arbitrate ON CONFLICT")); }
                let target = if columns.is_empty() { table.columns.iter().map(|c| c.name.clone()).collect() } else { names(&columns) };
                for name in &target { require_column(&table, name)?; }
                if target.iter().collect::<BTreeSet<_>>().len() != target.len() { return Err(err("DuplicateColumn", "Duplicate INSERT column")); }
                let input = match source.map(|q| *q.body) {
                    Some(SetExpr::Values(values)) => values.rows,
                    None => vec![Vec::new()],
                    _ => return Err(unsupported("INSERT currently requires VALUES")),
                };
                let mut output = Vec::new();
                let mut affected = 0;
                let mut touched = BTreeSet::new();
                for expressions in input {
                    if !expressions.is_empty() && expressions.len() != target.len() { return Err(err("TypeError", "INSERT column/value arity differs")); }
                    let mut values = BTreeMap::new();
                    for (col, expr) in target.iter().zip(&expressions) {
                        if is_default(expr) { continue; }
                        let column = table.columns.iter().find(|c| &c.name == col).unwrap();
                        if column.generated.is_some() || column.identity.as_ref().is_some_and(|id| id.always) { return Err(err("ConstraintViolation",format!("{col} is GENERATED ALWAYS"))); }
                        values.insert(col.clone(), eval(expr, &BTreeMap::new(), context)?);
                    }
                    for col in &table.columns { if !values.contains_key(&col.name) { values.insert(col.name.clone(), if let Some(identity) = &col.identity { identity_value(identity,table.next_row_id)? } else if let Some(default) = &col.default { eval(default, &BTreeMap::new(), context)? } else { Value::Null }); } }
                    recompute_generated(&table,&mut values,context)?;
                    check_row(&table, &values, context)?;
                    if !allowed(&table, "INSERT", &values, context, true)? { return Err(err("RlsViolation", "New row violates row-level security policy")); }
                    let existing = table.rows.values().find(|row| conflict_keys.iter().any(|key| key.iter().all(|col| !values[col].is_null() && values[col] == row.values[col]))).cloned();
                    if let Some(mut row) = existing {
                        match &conflict.expect("conflict keys imply a conflict clause").action {
                            OnConflictAction::DoNothing => continue,
                            OnConflictAction::DoUpdate(update) => {
                                if !allowed(&table, "UPDATE", &row.values, context, false)? { return Err(err("RlsViolation", "Conflicting row violates UPDATE policy")); }
                                let mut scope = row.values.clone();
                                scope.extend(values.iter().map(|(k,v)| (format!("excluded.{k}"),v.clone())));
                                if !predicate(update.selection.as_ref(), &scope, context)? { continue; }
                                if !touched.insert(row.id) { return Err(err("CardinalityError", "ON CONFLICT cannot affect the same row twice")); }
                                for assignment in &update.assignments {
                                    if assignment.id.len() != 1 { return Err(unsupported("Qualified upsert assignment target")); }
                                    let column = ident(&assignment.id[0]); require_column(&table, &column)?;
                                    assign_value(&table,&mut row.values,&column,&assignment.value,&scope,context)?;
                                }
                                recompute_generated(&table,&mut row.values,context)?;
                                check_row(&table, &row.values, context)?;
                                if !allowed(&table, "UPDATE", &row.values, context, true)? { return Err(err("RlsViolation", "Upserted row violates UPDATE policy")); }
                                row.version += 1;
                                if let Some(ref projection) = returning { output.push(project(projection, &row.values, context)?); }
                                table.rows.insert(row.id, row); affected += 1;
                                continue;
                            }
                        }
                    }
                    let id = table.next_row_id; table.next_row_id += 1;
                    touched.insert(id);
                    if let Some(ref projection) = returning { output.push(project(projection, &values, context)?); }
                    table.rows.insert(id, Row { id, version: 1, values }); affected += 1;
                }
                self.catalog.insert(name, table);
                Ok(SqlResult { rows: output, affected })
            }
            Statement::Update { table, assignments, from, selection, returning } => {
                if from.is_some() || !table.joins.is_empty() { return Err(unsupported("UPDATE FROM/JOIN")); }
                let name = factor_name(&table.relation)?;
                let mut table = self.table(&name)?.clone();
                for assignment in &assignments { if assignment.id.len() != 1 { return Err(unsupported("Qualified assignment target")); } require_column(&table, &ident(&assignment.id[0]))?; }
                let mut changed = Vec::new(); let mut output = Vec::new();
                for row in table.rows.values() {
                    if !predicate(selection.as_ref(), &row.values, context)? || !allowed(&table, "UPDATE", &row.values, context, false)? { continue; }
                    let mut next = row.clone();
                    for assignment in &assignments { assign_value(&table,&mut next.values,&ident(&assignment.id[0]),&assignment.value,&row.values,context)?; }
                    recompute_generated(&table,&mut next.values,context)?;
                    check_row(&table, &next.values, context)?;
                    if !allowed(&table, "UPDATE", &next.values, context, true)? { return Err(err("RlsViolation", "Updated row violates row-level security policy")); }
                    next.version += 1;
                    if let Some(ref projection) = returning { output.push(project(projection, &next.values, context)?); }
                    changed.push(next);
                }
                let affected = changed.len();
                for row in changed { table.rows.insert(row.id, row); }
                self.catalog.insert(name, table);
                Ok(SqlResult { rows: output, affected })
            }
            Statement::Delete { from, using, selection, returning, tables, order_by, limit } => {
                if from.len() != 1 || using.is_some() || !tables.is_empty() || !order_by.is_empty() || limit.is_some() || !from[0].joins.is_empty() { return Err(unsupported("DELETE join/using/order/limit")); }
                let name = factor_name(&from[0].relation)?;
                let mut table = self.table(&name)?.clone();
                let mut deleted = Vec::new(); let mut output = Vec::new();
                for row in table.rows.values() {
                    if predicate(selection.as_ref(), &row.values, context)? && allowed(&table, "DELETE", &row.values, context, false)? {
                        if let Some(ref projection) = returning { output.push(project(projection, &row.values, context)?); }
                        deleted.push(row.id);
                    }
                }
                let affected = deleted.len();
                for id in deleted { table.rows.remove(&id); }
                self.catalog.insert(name, table);
                Ok(SqlResult { rows: output, affected })
            }
            Statement::Query(query) => self.select(&query, context),
            other => Err(unsupported(format!("Statement {other}"))),
        }
    }
}
