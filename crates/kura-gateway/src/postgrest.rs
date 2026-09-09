use kurasql::{Database, SqlError, Table};
use serde_json::Value;

pub type Params = Vec<(String, String)>;
type Result<T> = std::result::Result<T, SqlError>;
fn error(message: impl Into<String>) -> SqlError { SqlError { code: "UnsupportedFeature".into(), message: message.into() } }
pub fn get<'a>(params: &'a Params, key: &str) -> Option<&'a str> { params.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str()) }

pub fn ident(name: &str) -> Result<String> {
    if name.is_empty() || name.contains('\0') { return Err(error("Invalid identifier")); }
    Ok(format!("\"{}\"", name.replace('"', "\"\"")))
}
pub fn literal(value: &Value) -> Result<String> {
    Ok(match value {
        Value::Null => "NULL".into(),
        Value::Bool(b) => if *b { "TRUE".into() } else { "FALSE".into() },
        Value::Number(n) => n.to_string(),
        Value::String(s) => format!("'{}'", s.replace('\'', "''")),
        Value::Array(_) | Value::Object(_) => format!("'{}'", value.to_string().replace('\'', "''")),
    })
}

fn typed(raw: &str, table: &Table, column: &str) -> Result<String> {
    let col = table.columns.iter().find(|c| c.name == column).ok_or_else(|| error(format!("Unknown column {column}")))?;
    let scalar = matches!(col.data_type.as_str(), "smallint" | "integer" | "int" | "bigint" | "real" | "float" | "double precision" | "boolean" | "bool") || col.data_type.starts_with("numeric") || col.data_type.starts_with("decimal");
    // `is.null` represents SQL NULL. For text, `eq.null` is a perfectly valid
    // request for the literal string "null" and must not be rewritten to NULL.
    if raw == "null" && scalar { return Ok("NULL".into()); }
    let value = if scalar {
        serde_json::from_str(raw).map_err(|_| error("Invalid typed filter value"))?
    } else { Value::String(postgrest_string(raw)?) };
    literal(&value)
}

fn postgrest_string(raw: &str) -> Result<String> {
    if !(raw.starts_with('"') || raw.ends_with('"')) { return Ok(raw.to_string()); }
    if raw.len() < 2 || !raw.ends_with('"') { return Err(error("Unterminated quoted filter value")); }
    let mut result = String::new(); let mut escaped = false;
    for character in raw[1..raw.len() - 1].chars() {
        if escaped { result.push(character); escaped = false; }
        else if character == '\\' { escaped = true; }
        else { result.push(character); }
    }
    if escaped { return Err(error("Unterminated quoted filter escape")); }
    Ok(result)
}

pub fn split(input: &str) -> Result<Vec<&str>> {
    let mut out = Vec::new(); let mut depth = 0_i32; let mut quoted = false; let mut escaped = false; let mut start = 0;
    for (i, c) in input.char_indices() {
        if escaped { escaped = false; continue; }
        if c == '\\' && quoted { escaped = true; continue; }
        if c == '"' { quoted = !quoted; }
        if quoted { continue; }
        if c == '(' { depth += 1; }
        if c == ')' { depth -= 1; if depth < 0 { return Err(error("Unbalanced expression")); } }
        if c == ',' && depth == 0 { out.push(input[start..i].trim()); start = i + 1; }
    }
    if depth != 0 || quoted { return Err(error("Unbalanced expression")); }
    out.push(input[start..].trim());
    Ok(out)
}

fn filter(table: &Table, column: &str, expression: &str) -> Result<String> {
    let (op, value) = expression.split_once('.').ok_or_else(|| error("Filter requires operator.value"))?;
    if op == "not" { return Ok(format!("NOT ({})", filter(table, column, value)?)); }
    let field = ident(column)?;
    let result = match op {
        "eq" | "neq" | "gt" | "gte" | "lt" | "lte" => {
            let operator = match op { "eq" => "=", "neq" => "<>", "gt" => ">", "gte" => ">=", "lt" => "<", _ => "<=" };
            format!("{field} {operator} {}", typed(value, table, column)?)
        }
        "is" if matches!(value, "null" | "true" | "false" | "unknown") => format!("{field} IS {}", value.to_uppercase()),
        "like" | "ilike" => format!("{field} {} {}", op.to_uppercase(), literal(&Value::String(value.replace('*', "%")))?),
        "in" => {
            let values = value.strip_prefix('(').and_then(|s| s.strip_suffix(')')).ok_or_else(|| error("IN requires parentheses"))?;
            if values.is_empty() { "FALSE".into() } else {
                let values = split(values)?.into_iter().map(|v| typed(v, table, column)).collect::<Result<Vec<_>>>()?;
                format!("{field} IN ({})", values.join(","))
            }
        }
        _ => return Err(error(format!("Filter {op} is not yet supported"))),
    };
    Ok(result)
}

fn logic(table: &Table, op: &str, value: &str) -> Result<String> {
    let value = value.strip_prefix('(').and_then(|s| s.strip_suffix(')')).ok_or_else(|| error("Boolean filter requires parentheses"))?;
    let mut conditions = Vec::new();
    for part in split(value)? {
        let (field, expr) = part.split_once('.').ok_or_else(|| error("Invalid boolean filter"))?;
        if matches!(field, "and" | "or") { conditions.push(logic(table, field, expr)?); }
        else { conditions.push(filter(table, field, expr)?); }
    }
    Ok(format!("({})", conditions.join(if op == "or" { " OR " } else { " AND " })))
}

pub fn where_clause(table: &Table, params: &Params) -> Result<String> {
    let mut terms = Vec::new();
    for (name, value) in params {
        // A dotted key belongs to an embedded relation (for example
        // `author.name=eq.ada`), not to this root SQL statement. It is applied
        // while projecting the relation from the same database snapshot.
        if name.contains('.') || ["select", "order", "limit", "offset", "on_conflict", "columns"].contains(&name.as_str()) { continue; }
        terms.push(if matches!(name.as_str(), "or" | "and") { logic(table, name, value)? } else { filter(table, name, value)? });
    }
    Ok(if terms.is_empty() { String::new() } else { format!(" WHERE {}", terms.join(" AND ")) })
}

pub fn select_sql(table: &Table, params: &Params) -> Result<String> {
    let mut sql = format!("SELECT * FROM {}{}", ident(&table.name)?, where_clause(table, params)?);
    if let Some(order) = get(params, "order") {
        let mut terms = Vec::new();
        for order in split(order)? {
            let bits = order.split('.').collect::<Vec<_>>();
            let mut term = ident(bits[0])?;
            for option in &bits[1..] { term.push_str(match *option { "asc" => " ASC", "desc" => " DESC", "nullsfirst" => " NULLS FIRST", "nullslast" => " NULLS LAST", _ => return Err(error("Unknown ordering option")) }); }
            terms.push(term);
        }
        sql.push_str(&format!(" ORDER BY {}", terms.join(",")));
    }
    Ok(sql)
}

pub fn mutation_sql(method: &str, table: &Table, params: &Params, body: &Value, prefer: &str) -> Result<String> {
    let name = ident(&table.name)?;
    match method {
        "DELETE" => Ok(format!("DELETE FROM {name}{} RETURNING *", where_clause(table, params)?)),
        "PATCH" => {
            let object = body.as_object().ok_or_else(|| error("UPDATE expects an object"))?;
            if object.is_empty() { return Err(error("UPDATE requires at least one column")); }
            let assignments = object.iter().map(|(k,v)| Ok(format!("{}={}", ident(k)?, literal(v)?))).collect::<Result<Vec<_>>>()?;
            Ok(format!("UPDATE {name} SET {}{} RETURNING *", assignments.join(","), where_clause(table, params)?))
        }
        "POST" => {
            let rows = if let Some(rows) = body.as_array() { rows.clone() } else { vec![body.clone()] };
            if rows.is_empty() { return Err(error("INSERT expects at least one row")); }
            let first = rows[0].as_object().ok_or_else(|| error("INSERT expects objects"))?;
            let columns = first.keys().cloned().collect::<Vec<_>>();
            let mut groups = Vec::new();
            for row in &rows {
                let row = row.as_object().ok_or_else(|| error("INSERT expects objects"))?;
                if row.keys().collect::<Vec<_>>() != first.keys().collect::<Vec<_>>() { return Err(error("Bulk insert requires consistent columns")); }
                groups.push(format!("({})", columns.iter().map(|c| literal(&row[c])).collect::<Result<Vec<_>>>()?.join(",")));
            }
            let names = columns.iter().map(|c| ident(c)).collect::<Result<Vec<_>>>()?.join(",");
            let mut sql = if columns.is_empty() { format!("INSERT INTO {name} DEFAULT VALUES") } else { format!("INSERT INTO {name} ({names}) VALUES {}", groups.join(",")) };
            if prefer.contains("resolution=") {
                let conflict = get(params, "on_conflict").map(|s| s.split(',').map(str::to_string).collect()).unwrap_or_else(|| table.primary_key.clone());
                if conflict.is_empty() { return Err(error("Upsert requires a conflict target")); }
                sql.push_str(&format!(" ON CONFLICT ({}) ", conflict.iter().map(|c| ident(c)).collect::<Result<Vec<_>>>()?.join(",")));
                if prefer.contains("resolution=ignore-duplicates") { sql.push_str("DO NOTHING"); }
                else { sql.push_str(&format!("DO UPDATE SET {}", columns.iter().map(|c| Ok(format!("{}=EXCLUDED.{}",ident(c)?,ident(c)?))).collect::<Result<Vec<_>>>()?.join(","))); }
            }
            sql.push_str(" RETURNING *");
            Ok(sql)
        }
        _ => Err(error("Unsupported HTTP method")),
    }
}

#[derive(Debug, Clone)]
struct RelationSpec {
    alias: String,
    target: String,
    foreign_key: Option<String>,
    inner: bool,
    nested: String,
}

struct ResolvedRelation<'a> {
    target: &'a Table,
    foreign_key: &'a kurasql::ForeignKey,
    outgoing: bool,
}

fn relation_spec(field: &str) -> Result<Option<RelationSpec>> {
    let Some((raw, nested)) = field.split_once('(') else { return Ok(None); };
    let nested = nested.strip_suffix(')').ok_or_else(|| error("Unclosed relation"))?;
    let (alias, raw) = raw.trim().split_once(':').unwrap_or((raw.trim(), raw.trim()));
    let mut parts = raw.split('!');
    let target = parts.next().unwrap_or_default().trim();
    if target.is_empty() || alias.trim().is_empty() { return Err(error("Nested relation requires a name")); }
    let mut foreign_key = None;
    let mut inner = false;
    for modifier in parts {
        let modifier = modifier.trim();
        if modifier.is_empty() { return Err(error("Invalid relation modifier")); }
        if modifier == "inner" {
            if inner { return Err(error("Duplicate !inner relation modifier")); }
            inner = true;
        } else if foreign_key.replace(modifier.to_string()).is_some() {
            return Err(error("Only one explicit foreign-key selector is supported"));
        }
    }
    Ok(Some(RelationSpec { alias: alias.trim().to_string(), target: target.to_string(), foreign_key, inner, nested: nested.trim().to_string() }))
}

fn fk_matches_hint(fk: &kurasql::ForeignKey, hint: &str) -> bool {
    fk.columns.iter().any(|column| column == hint) || fk.columns.join("_") == hint
}

fn resolve_relation<'a>(db: &'a Database, table: &'a Table, spec: &RelationSpec) -> Result<ResolvedRelation<'a>> {
    let target = db.catalog.get(&spec.target).ok_or_else(|| error(format!("Unknown nested relation {}", spec.target)))?;
    let mut candidates = table.foreign_keys.iter()
        .filter(|fk| fk.foreign_table == spec.target)
        .map(|fk| (fk, true))
        .chain(target.foreign_keys.iter().filter(|fk| fk.foreign_table == table.name).map(|fk| (fk, false)))
        .collect::<Vec<_>>();
    if let Some(hint) = &spec.foreign_key {
        candidates.retain(|(fk, _)| fk_matches_hint(fk, hint));
        if candidates.is_empty() { return Err(error(format!("No relationship from {} to {} matches !{}", table.name, spec.target, hint))); }
    }
    if candidates.len() != 1 {
        return Err(error(format!("Ambiguous relationship from {} to {}; use !<foreign-key-column> (for example !author_id)", table.name, spec.target)));
    }
    let (foreign_key, outgoing) = candidates.pop().expect("checked length");
    Ok(ResolvedRelation { target, foreign_key, outgoing })
}

fn relation_params(params: &Params, spec: &RelationSpec) -> Params {
    params.iter().filter_map(|(name, value)| {
        for prefix in [&spec.alias, &spec.target] {
            if let Some(key) = name.strip_prefix(&format!("{prefix}.")) { return Some((key.to_string(), value.clone())); }
        }
        None
    }).collect()
}

fn relation_rows(db: &Database, relation: &ResolvedRelation<'_>, spec: &RelationSpec, params: &Params, context: &kurasql::AuthContext) -> Result<Vec<Value>> {
    let filters = relation_params(params, spec);
    if filters.is_empty() { return db.read_table(&spec.target, context); }
    // Reuse the ordinary typed filter parser against a cloned immutable snapshot.
    // It avoids interpolating raw dotted filter values into SQL and keeps RLS on
    // the referenced table in force.
    let mut snapshot = db.clone();
    snapshot.execute_sql(&select_sql(relation.target, &filters)?, context).map(|result| result.rows)
}

fn linked(row: &Value, other: &Value, relation: &ResolvedRelation<'_>) -> bool {
    relation.foreign_key.columns.iter().zip(&relation.foreign_key.referred_columns).all(|(local, referred)| {
        let (left, right) = if relation.outgoing { (&row[local], &other[referred]) } else { (&other[local], &row[referred]) };
        !left.is_null() && left == right
    })
}

fn json_path(row: &Value, column: &str) -> Result<Value> {
    let Some((base, paths)) = column.split_once("->") else {
        return row.get(column).cloned().ok_or_else(|| error(format!("Unknown selected column {column}")));
    };
    let mut value = row.get(base).cloned().ok_or_else(|| error(format!("Unknown selected column {base}")))?;
    for raw in paths.split("->") {
        let text = raw.starts_with('>');
        let key = raw.trim_start_matches('>').trim().trim_matches('"');
        if key.is_empty() { return Err(error("JSON path requires a key")); }
        value = match &value {
            Value::Object(object) => object.get(key).cloned().unwrap_or(Value::Null),
            Value::Array(array) => key.parse::<usize>().ok().and_then(|index| array.get(index)).cloned().unwrap_or(Value::Null),
            Value::Null => Value::Null,
            _ => return Err(error(format!("JSON path traverses a non-container at {key}"))),
        };
        if text && !value.is_null() {
            value = Value::String(match value { Value::String(text) => text, other => other.to_string() });
        }
    }
    Ok(value)
}

/// Apply only `!inner` embedded-relation filters before root pagination.
/// The gateway should call this after `execute_sql` and before computing `total`.
pub fn filter_referenced_rows(rows: Vec<Value>, select: &str, params: &Params, db: &Database, table: &Table, context: &kurasql::AuthContext) -> Result<Vec<Value>> {
    let relations = split(select)?.into_iter().map(relation_spec).collect::<Result<Vec<_>>>()?
        .into_iter().flatten().filter(|spec| spec.inner).collect::<Vec<_>>();
    if relations.is_empty() { return Ok(rows); }
    let mut filtered = Vec::new();
    for row in rows {
        let mut keep = true;
        for spec in &relations {
            let relation = resolve_relation(db, table, spec)?;
            let matched = relation_rows(db, &relation, spec, params, context)?.into_iter().any(|other| linked(&row, &other, &relation));
            if !matched { keep = false; break; }
        }
        if keep { filtered.push(row); }
    }
    Ok(filtered)
}

pub fn project(rows: Vec<Value>, select: &str, db: &Database, table: &Table, context: &kurasql::AuthContext) -> Result<Vec<Value>> {
    project_with_params(rows, select, &Vec::new(), db, table, context)
}

/// Project rows and apply filters addressed to selected embedded relations.
pub fn project_with_params(rows: Vec<Value>, select: &str, params: &Params, db: &Database, table: &Table, context: &kurasql::AuthContext) -> Result<Vec<Value>> {
    let fields = split(select)?;
    rows.into_iter().map(|row| {
        let mut result = serde_json::Map::new();
        for field in &fields {
            if *field == "*" { result.extend(row.as_object().cloned().unwrap_or_default()); continue; }
            if let Some(spec) = relation_spec(field)? {
                let relation = resolve_relation(db, table, &spec)?;
                let matched = relation_rows(db, &relation, &spec, params, context)?.into_iter()
                    .filter(|other| linked(&row, other, &relation)).collect();
                let mut values = project_with_params(matched, &spec.nested, params, db, relation.target, context)?;
                result.insert(spec.alias, if relation.outgoing { values.pop().unwrap_or(Value::Null) } else { Value::Array(values) });
            } else {
                let (alias, column) = field.split_once(':').unwrap_or((field,field));
                result.insert(alias.into(), json_path(&row, column)?);
            }
        }
        Ok(Value::Object(result))
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use kurasql::{AuthContext, Column, ForeignKey, Row};
    use serde_json::json;
    use std::collections::{BTreeMap, BTreeSet};

    fn table(name: &str, columns: &[&str], foreign_keys: Vec<ForeignKey>, rows: Vec<Value>) -> Table {
        Table {
            id: 1,
            name: name.into(),
            columns: columns.iter().enumerate().map(|(id, name)| Column { id: id as u64 + 1, name: (*name).into(), data_type: if *name == "id" || name.ends_with("_id") { "integer".into() } else if *name == "meta" { "json".into() } else { "text".into() }, nullable: false, default: None, identity: None, generated: None }).collect(),
            primary_key: vec!["id".into()], unique: vec![], deferred_unique: BTreeSet::new(), foreign_keys, checks: vec![], rls_enabled: false, policies: vec![],
            rows: rows.into_iter().enumerate().map(|(id, value)| (id as u64 + 1, Row { id: id as u64 + 1, version: 0, values: value.as_object().unwrap().iter().map(|(key, value)| (key.clone(), value.clone())).collect::<BTreeMap<_,_>>() })).collect(),
            next_row_id: 100,
        }
    }

    fn fixture() -> Database {
        let mut db = Database::default();
        db.catalog.insert("users".into(), table("users", &["id", "name"], vec![], vec![json!({"id": 1, "name": "Ada"}), json!({"id": 2, "name": "Grace"})]));
        db.catalog.insert("posts".into(), table("posts", &["id", "author_id", "editor_id", "title", "meta"], vec![
            ForeignKey { columns: vec!["author_id".into()], foreign_table: "users".into(), referred_columns: vec!["id".into()], on_delete: None, on_update: None, deferred: false },
            ForeignKey { columns: vec!["editor_id".into()], foreign_table: "users".into(), referred_columns: vec!["id".into()], on_delete: None, on_update: None, deferred: false },
        ], vec![json!({"id": 10, "author_id": 1, "editor_id": 2, "title": "first", "meta": {"title": "first", "labels": ["red"]}}), json!({"id": 11, "author_id": 2, "editor_id": 1, "title": "second", "meta": {"title": "second"}})]));
        db
    }

    #[test]
    fn selects_json_paths_and_requires_explicit_fk_when_ambiguous() {
        let db = fixture(); let context = AuthContext::admin(); let posts = db.catalog.get("posts").unwrap();
        let rows = db.read_table("posts", &context).unwrap();
        let error = project(rows.clone(), "users(id)", &db, posts, &context).unwrap_err();
        assert!(error.message.contains("!<foreign-key-column>"));
        let projected = project(rows, "id,author:users!author_id(id),headline:meta->>title", &db, posts, &context).unwrap();
        assert_eq!(projected[0], json!({"id": 10, "author": {"id": 1}, "headline": "first"}));
    }

    #[test]
    fn referenced_filters_apply_to_embeds_and_inner_rows_before_pagination() {
        let db = fixture(); let context = AuthContext::admin(); let posts = db.catalog.get("posts").unwrap();
        let rows = db.read_table("posts", &context).unwrap();
        let params = vec![("author.name".into(), "eq.Ada".into())];
        let select = "id,author:users!author_id!inner(id,name)";
        let filtered = filter_referenced_rows(rows, select, &params, &db, posts, &context).unwrap();
        assert_eq!(filtered.len(), 1);
        let projected = project_with_params(filtered, select, &params, &db, posts, &context).unwrap();
        assert_eq!(projected, vec![json!({"id": 10, "author": {"id": 1, "name": "Ada"}})]);
    }

    #[test]
    fn dotted_filters_do_not_leak_into_root_sql_and_quoted_in_values_are_literals() {
        let db = fixture(); let posts = db.catalog.get("posts").unwrap();
        let params = vec![
            ("author.name".into(), "eq.Ada".into()),
            ("title".into(), "in.(first,\"second,third\",\"1' OR 1=1 --\")".into()),
        ];
        let sql = select_sql(posts, &params).unwrap();
        assert!(!sql.contains("author.name"));
        assert!(sql.contains("\"title\" IN ('first','second,third','1'' OR 1=1 --')"));
    }

    #[test]
    fn text_null_is_a_literal_and_quoted_in_escapes_are_preserved_safely() {
        let db = fixture(); let posts = db.catalog.get("posts").unwrap();
        let text_null = select_sql(posts, &vec![("title".into(), "eq.null".into())]).unwrap();
        let sql_null = select_sql(posts, &vec![("title".into(), "is.null".into())]).unwrap();
        let quoted = select_sql(posts, &vec![("title".into(), r#"in.("a,b","quoted\"value","x' OR 1=1 --")"#.into())]).unwrap();
        assert!(text_null.contains("\"title\" = 'null'"));
        assert!(sql_null.contains("\"title\" IS NULL"));
        assert!(quoted.contains("\"title\" IN ('a,b','quoted\"value','x'' OR 1=1 --')"));
    }
}
