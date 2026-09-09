use super::*;
use std::ops::ControlFlow;

fn matching_paren(words: &[String], start: usize) -> Result<usize> {
    if words.get(start).map(String::as_str) != Some("(") { return Err(err("SyntaxError", "Expected opening parenthesis")); }
    let mut depth = 0;
    for (index,word) in words.iter().enumerate().skip(start) {
        if word == "(" { depth += 1; }
        if word == ")" { depth -= 1; if depth == 0 { return Ok(index); } }
    }
    Err(err("SyntaxError", "Unclosed function declaration"))
}
fn declarations(sql: &str) -> Result<Vec<ColumnDef>> {
    match Parser::parse_sql(&PostgreSqlDialect {}, &format!("CREATE TABLE __parameters ({sql})")).map_err(|e| err("SyntaxError", e.to_string()))?.remove(0) {
        Statement::CreateTable { columns, constraints, .. } if constraints.is_empty() => Ok(columns),
        _ => Err(err("SyntaxError", "Expected typed named parameters")),
    }
}
fn sql_value(value: &Value) -> Result<Expr> {
    Ok(Expr::Value(match value {
        Value::Null => sqlparser::ast::Value::Null,
        Value::Bool(value) => sqlparser::ast::Value::Boolean(*value),
        Value::Number(value) if value.as_i64().is_some() => sqlparser::ast::Value::Number(value.to_string(),false),
        Value::String(value) => sqlparser::ast::Value::SingleQuotedString(value.clone()),
        Value::Array(_) | Value::Object(_) => return Ok(Expr::Cast { expr: Box::new(Expr::Value(sqlparser::ast::Value::SingleQuotedString(value.to_string()))), data_type: DataType::JSON, format: None }),
        _ => return Err(unsupported("Non-integer numeric function arguments")),
    }))
}

impl Database {
    pub(super) fn function_statement(&mut self, sql: &str, context: &AuthContext) -> Result<Option<SqlResult>> {
        let words = tokens(sql)?;
        let upper: Vec<String> = words.iter().map(|s| s.to_uppercase()).collect();
        let replace = upper.starts_with(&["CREATE".into(),"OR".into(),"REPLACE".into(),"FUNCTION".into()]);
        if !replace && !upper.starts_with(&["CREATE".into(),"FUNCTION".into()]) { return Ok(None); }
        if !context.privileged { return Err(err("PrivilegeError", "Function DDL requires administrative authority")); }
        let name_start = if replace { 4 } else { 2 };
        let args_start = (name_start..words.len()).find(|&i| words[i] == "(").ok_or_else(|| err("SyntaxError", "Missing function parameters"))?;
        let name = parse_relation(&words[name_start..args_start].join(" "))?;
        let args_end = matching_paren(&words,args_start)?;
        if upper.get(args_end+1).map(String::as_str) != Some("RETURNS") { return Err(err("SyntaxError", "Function requires RETURNS")); }
        let return_start = args_end+2;
        let return_end = (return_start..words.len()).find(|&i| ["LANGUAGE","AS","SECURITY","IMMUTABLE","STABLE","VOLATILE"].contains(&upper[i].as_str())).ok_or_else(|| err("SyntaxError", "SQL function requires LANGUAGE SQL and AS body"))?;
        let returns = &words[return_start..return_end];
        let return_kind = match returns.first().map(|s| s.to_uppercase()).as_deref() {
            Some("VOID") if returns.len() == 1 => FunctionReturn::Void,
            Some("SETOF") => {
                let target = returns[1..].join(" ").to_lowercase();
                if check_type_supported(&target).is_err() { self.table(&parse_relation(&target)?)?; }
                FunctionReturn::SetOf(target)
            }
            Some("TABLE") => {
                if matching_paren(returns,1)? != returns.len()-1 { return Err(err("SyntaxError", "Invalid TABLE return declaration")); }
                let columns = declarations(&returns[2..returns.len()-1].join(" "))?;
                let mut typed = Vec::new();
                for column in columns { if !column.options.is_empty() { return Err(unsupported("Return column options")); } let ty = column.data_type.to_string().to_lowercase(); check_type_supported(&ty)?; typed.push((ident(&column.name),ty)); }
                FunctionReturn::Table(typed)
            }
            Some(_) => { let ty = returns.join(" ").to_lowercase(); check_type_supported(&ty)?; FunctionReturn::Scalar(ty) }
            None => return Err(err("SyntaxError", "Missing return type")),
        };
        let mut parameters = Vec::new(); let mut parameter_names = BTreeSet::new();
        for column in declarations(&words[args_start+1..args_end].join(" "))? {
            let name = ident(&column.name);
            if !parameter_names.insert(name.clone()) { return Err(err("DuplicateColumn", "Duplicate function parameter")); }
            let ty = column.data_type.to_string().to_lowercase(); check_type_supported(&ty)?;
            let mut default = None;
            for option in column.options { match option.option { ColumnOption::Default(expr) => default = Some(expr), _ => return Err(unsupported("Function parameter options")) } }
            parameters.push(FunctionParameter { name, data_type: ty, default });
        }
        let mut index = return_end; let mut body = None; let mut language = None; let mut definer = false; let mut behavior = "VOLATILE";
        let mut seen = BTreeSet::new();
        while index < words.len() {
            let keyword = upper[index].as_str(); index += 1;
            let category = if ["IMMUTABLE","STABLE","VOLATILE"].contains(&keyword) { "BEHAVIOR" } else { keyword };
            if !seen.insert(category) { return Err(err("SyntaxError", "Duplicate function clause")); }
            match keyword {
                "LANGUAGE" => { language = upper.get(index).cloned(); index += 1; }
                "SECURITY" => { definer = match upper.get(index).map(String::as_str) { Some("DEFINER") => true, Some("INVOKER") => false, _ => return Err(err("SyntaxError", "Expected SECURITY INVOKER or DEFINER")) }; index += 1; }
                "AS" => {
                    let definition = words.get(index).ok_or_else(|| err("SyntaxError", "Missing function body"))?;
                    let parsed = sqlparser::tokenizer::Tokenizer::new(&PostgreSqlDialect {}, definition).tokenize().map_err(|e| err("SyntaxError",e.to_string()))?;
                    body = Some(match parsed.as_slice() {
                        [sqlparser::tokenizer::Token::DollarQuotedString(value)] => value.value.clone(),
                        [sqlparser::tokenizer::Token::SingleQuotedString(value)] => value.clone(),
                        _ => return Err(err("SyntaxError", "Function body must be a quoted SQL string")),
                    }); index += 1;
                }
                "IMMUTABLE" | "STABLE" | "VOLATILE" => behavior = keyword,
                _ => return Err(unsupported(format!("Function clause {keyword}"))),
            }
        }
        if language.as_deref() != Some("SQL") { return Err(unsupported("Only LANGUAGE SQL functions are implemented")); }
        let body = body.ok_or_else(|| err("SyntaxError", "Missing SQL function body"))?;
        let statements = Parser::parse_sql(&PostgreSqlDialect {}, &body).map_err(|e| err("SyntaxError",e.to_string()))?;
        if statements.is_empty() { return Err(err("SyntaxError", "SQL function body is empty")); }
        for stmt in &statements {
            match stmt {
                Statement::Query(_) => {},
                Statement::Insert { .. } | Statement::Update { .. } | Statement::Delete { .. } if behavior == "VOLATILE" => {},
                _ => return Err(unsupported("Function body permits SELECT and VOLATILE DML only")),
            }
        }
        if self.functions.contains_key(&name) && !replace { return Err(err("DuplicateFunction",name)); }
        self.functions.insert(name.clone(), SqlFunction { name, parameters, return_kind, body, security_definer: definer, definer_context: if definer { Some(context.clone()) } else { None } });
        self.schema_version += 1;
        Ok(Some(SqlResult::default()))
    }

    /// Execute a SQL-language RPC against a private staged snapshot and publish only on success.
    pub fn call_function(&mut self, name: &str, args: &Value, context: &AuthContext) -> Result<SqlResult> {
        let mut staged = self.clone();
        let result = staged.call_function_inner(name,args,context,0)?;
        staged.validate(context)?;
        *self = staged;
        Ok(result)
    }
    fn call_function_inner(&mut self, name: &str, args: &Value, context: &AuthContext, depth: usize) -> Result<SqlResult> {
        if depth >= 32 { return Err(err("ResourceLimit", "SQL function call depth exceeded")); }
        let function = self.functions.get(name.strip_prefix("public.").unwrap_or(name)).cloned().ok_or_else(|| err("UndefinedFunction",name))?;
        let args = args.as_object().ok_or_else(|| err("TypeError", "RPC arguments must be an object"))?;
        for key in args.keys() { if !function.parameters.iter().any(|p| &p.name == key) { return Err(err("TypeError",format!("Unknown function argument {key}"))); } }
        let mut effective = context.clone();
        if function.security_definer {
            let definer = function.definer_context.as_ref().ok_or_else(|| err("PrivilegeError", "Function has no granted definer authority"))?;
            effective.role = definer.role.clone(); effective.privileged = definer.privileged;
        }
        let mut bound = BTreeMap::new();
        for (index,parameter) in function.parameters.iter().enumerate() {
            let value = match args.get(&parameter.name) {
                Some(value) => value.clone(),
                None => match &parameter.default { Some(expr) => eval(expr,&BTreeMap::new(),context)?, None => return Err(err("TypeError",format!("Missing function argument {}",parameter.name))) },
            };
            check_value(&Column { id: 0, name: parameter.name.clone(), data_type: parameter.data_type.clone(), nullable: true, default: None, identity: None, generated: None }, &value)?;
            let expr = sql_value(&value)?;
            bound.insert(parameter.name.clone(),expr.clone()); bound.insert(format!("${}",index+1),expr.clone()); bound.insert(format!("{}.{}",function.name,parameter.name),expr);
        }
        let mut statements = Parser::parse_sql(&PostgreSqlDialect {}, &function.body).map_err(|e| err("SyntaxError",e.to_string()))?;
        let _: ControlFlow<()> = visit_expressions_mut(&mut statements, |expr| {
            let key = match expr { Expr::Identifier(id) => Some(ident(id)), Expr::CompoundIdentifier(ids) => Some(ids.iter().map(ident).collect::<Vec<_>>().join(".")), Expr::Value(sqlparser::ast::Value::Placeholder(value)) => Some(value.clone()), _ => None };
            if let Some(value) = key.and_then(|key| bound.get(&key)) { *expr = value.clone(); }
            ControlFlow::Continue(())
        });
        let mut result = SqlResult::default(); let mut affected = 0;
        for statement in statements {
            let before = self.clone();
            if let Some((callee,arguments)) = nested_call(&statement,&self.functions,&effective)? {
                result = self.call_function_inner(&callee,&arguments,&effective,depth+1)?;
            } else { result = self.execute_statement(statement,&effective)?; }
            affected += result.affected;
            self.apply_referential_actions(&before,&effective)?;
            self.validate_immediate(&effective)?;
        }
        result.affected = affected;
        match &function.return_kind {
            FunctionReturn::Void => result.rows = vec![Value::Null],
            FunctionReturn::Scalar(ty) => {
                if result.rows.len() > 1 { return Err(err("CardinalityError", "Scalar function returned multiple rows")); }
                let value = match result.rows.pop() { None => Value::Null, Some(Value::Object(map)) if map.len() == 1 => map.into_iter().next().unwrap().1, Some(Value::Object(_)) => return Err(err("TypeError", "Scalar function returned multiple columns")), Some(value) => value };
                check_return_value(ty,&value)?; result.rows = vec![value];
            }
            FunctionReturn::SetOf(ty) if check_type_supported(ty).is_ok() => {
                for row in &mut result.rows { if let Value::Object(map) = row { if map.len() != 1 { return Err(err("TypeError", "SETOF scalar requires one column")); } *row = map.values().next().unwrap().clone(); } check_return_value(ty,row)?; }
            }
            FunctionReturn::SetOf(table_name) => {
                let table = self.table(table_name)?;
                for row in &result.rows { let row = row.as_object().ok_or_else(|| err("TypeError", "SETOF table requires rows"))?; if row.len() != table.columns.len() { return Err(err("TypeError", "Function result shape does not match table")); } for col in &table.columns { check_return_value(&col.data_type,row.get(&col.name).ok_or_else(|| err("TypeError", "Missing function result column"))?)?; } }
            }
            FunctionReturn::Table(columns) => {
                for row in &result.rows { let row = row.as_object().ok_or_else(|| err("TypeError", "TABLE function requires rows"))?; if row.len() != columns.len() { return Err(err("TypeError", "Function result shape does not match RETURNS TABLE")); } for (name,ty) in columns { check_return_value(ty,row.get(name).ok_or_else(|| err("TypeError",format!("Missing return column {name}; use explicit aliases")))?)?; } }
            }
        }
        Ok(result)
    }
}
fn check_return_value(ty: &str, value: &Value) -> Result<()> { check_value(&Column { id: 0, name: "function return".into(), data_type: ty.into(), nullable: true, default: None, identity: None, generated: None },value) }
fn nested_call(statement: &Statement, functions: &BTreeMap<String,SqlFunction>, context: &AuthContext) -> Result<Option<(String,Value)>> {
    let Statement::Query(query) = statement else { return Ok(None); };
    let SetExpr::Select(select) = &*query.body else { return Ok(None); };
    if select.projection.len() != 1 || !select.from.is_empty() || select.selection.is_some() || query.with.is_some() || query.limit.is_some() || !query.order_by.is_empty() { return Ok(None); }
    let expr = match &select.projection[0] { SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => expr, _ => return Ok(None) };
    let Expr::Function(call) = expr else { return Ok(None); };
    let name = table_name(&call.name)?;
    let Some(function) = functions.get(&name) else { return Ok(None); };
    if call.args.len() != function.parameters.len() || call.over.is_some() || call.filter.is_some() || call.distinct { return Err(unsupported("Nested function arguments/modifiers")); }
    let mut args = Map::new();
    for (arg,param) in call.args.iter().zip(&function.parameters) { let FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) = arg else { return Err(unsupported("Nested function named arguments")); }; args.insert(param.name.clone(),eval(expr,&BTreeMap::new(),context)?); }
    Ok(Some((name,Value::Object(args))))
}
