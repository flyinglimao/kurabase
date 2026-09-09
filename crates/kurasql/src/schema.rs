use super::*;
use std::collections::VecDeque;
use std::ops::ControlFlow;

fn references(sql: &str, relation: &str) -> Result<bool> {
    let statements = Parser::parse_sql(&PostgreSqlDialect {},sql).map_err(|e| err("SyntaxError",e.to_string()))?;
    let found = visit_relations(&statements, |name| {
        if table_name(name).is_ok_and(|name| name == relation) { ControlFlow::Break(()) } else { ControlFlow::Continue(()) }
    });
    Ok(matches!(found,ControlFlow::Break(())))
}
fn expression_uses(expr: &Expr, column: &str) -> bool {
    matches!(visit_expressions(expr,|expr| {
        if matches!(expr,Expr::Identifier(id) if ident(id) == column) { ControlFlow::Break(()) } else { ControlFlow::Continue(()) }
    }),ControlFlow::Break(()))
}
fn rename_expression(expr: &mut Expr, old: &str, new: &str) {
    let _: ControlFlow<()> = visit_expressions_mut(expr,|expr| { if matches!(expr,Expr::Identifier(id) if ident(id) == old) { *expr = Expr::Identifier(Ident::with_quote('"',new)); } ControlFlow::Continue(()) });
}

impl Database {
    fn dependents(&self, relation: &str) -> Result<(Vec<String>,Vec<String>)> {
        let mut views = Vec::new(); let mut functions = Vec::new();
        for (name,view) in &self.views { if references(&view.sql,relation)? { views.push(name.clone()); } }
        for (name,function) in &self.functions { if references(&function.body,relation)? { functions.push(name.clone()); } }
        Ok((views,functions))
    }
    pub(super) fn remove_dependents(&mut self, relation: &str, cascade: bool) -> Result<()> {
        let (views,functions) = self.dependents(relation)?;
        if !cascade && (!views.is_empty() || !functions.is_empty()) { return Err(err("DependencyViolation",format!("{relation} has dependent views/functions"))); }
        for name in views { self.views.remove(&name); self.remove_dependents(&name,true)?; }
        for name in functions { self.functions.remove(&name); }
        Ok(())
    }
    pub(super) fn drop_tables(&mut self, names: Vec<ObjectName>, if_exists: bool, cascade: bool, context: &AuthContext) -> Result<SqlResult> {
        if !context.privileged { return Err(err("PrivilegeError","DDL requires administrative authority")); }
        let names = names.iter().map(table_name).collect::<Result<BTreeSet<_>>>()?;
        for name in &names { if !self.catalog.contains_key(name) && !if_exists { return Err(err("UndefinedTable",name)); } }
        for name in &names {
            self.remove_dependents(name,cascade)?;
            for (source,table) in &mut self.catalog {
                if names.contains(source) { continue; }
                if table.foreign_keys.iter().any(|fk| &fk.foreign_table == name) && !cascade { return Err(err("DependencyViolation",format!("{source} references {name}"))); }
                table.foreign_keys.retain(|fk| &fk.foreign_table != name);
            }
        }
        for name in names { self.catalog.remove(&name); }
        self.schema_version += 1;
        Ok(SqlResult::default())
    }
    pub(super) fn alter_table(&mut self, name: &str, if_exists: bool, operations: Vec<AlterTableOperation>, context: &AuthContext) -> Result<SqlResult> {
        if !context.privileged { return Err(err("PrivilegeError","DDL requires administrative authority")); }
        if !self.catalog.contains_key(name) { return if if_exists { Ok(SqlResult::default()) } else { Err(err("UndefinedTable",name)) }; }
        let mut name = name.to_string();
        for operation in operations {
            let mut table = self.table(&name)?.clone();
            match operation {
                AlterTableOperation::AddColumn { column_def, if_not_exists, .. } => {
                    let colname = ident(&column_def.name);
                    if table.columns.iter().any(|col| col.name == colname) { if if_not_exists { continue; } return Err(err("DuplicateColumn",colname)); }
                    if column_def.collation.is_some() { return Err(unsupported("Column collation")); }
                    let ty = column_def.data_type.to_string().to_lowercase(); check_type_supported(&ty)?;
                    let mut column = Column { id: self.allocate(), name: colname.clone(), data_type: ty, nullable: true, default: None, identity: None, generated: None };
                    for option in column_def.options {
                        match option.option {
                            ColumnOption::Null => column.nullable = true,
                            ColumnOption::NotNull => column.nullable = false,
                            ColumnOption::Default(expr) => column.default = Some(expr),
                            ColumnOption::Unique { is_primary: false } => table.unique.push(vec![colname.clone()]),
                            ColumnOption::Check(expr) => table.checks.push(expr),
                            ColumnOption::Generated { generated_as,sequence_options,generation_expr,.. } => configure_generated(&mut column,generated_as,sequence_options,generation_expr,context)?,
                            ColumnOption::ForeignKey { foreign_table,referred_columns,on_delete,on_update } => table.foreign_keys.push(ForeignKey { columns: vec![colname.clone()], foreign_table: table_name(&foreign_table)?, referred_columns: names(&referred_columns), on_delete,on_update, deferred: false }),
                            _ => return Err(unsupported("ADD COLUMN option")),
                        }
                    }
                    for row in table.rows.values_mut() { let value = if let Some(identity) = &column.identity { identity_value(identity,row.id)? } else if let Some(expr) = &column.generated { eval(expr,&row.values,context)? } else { column.default.as_ref().map(|expr| eval(expr,&BTreeMap::new(),context)).transpose()?.unwrap_or(Value::Null) }; check_value(&column,&value)?; row.values.insert(colname.clone(),value); row.version += 1; }
                    table.columns.push(column);
                }
                AlterTableOperation::RenameColumn { old_column_name, new_column_name } => {
                    let old = ident(&old_column_name); let new = ident(&new_column_name); require_column(&table,&old)?;
                    if table.columns.iter().any(|c| c.name == new) { return Err(err("DuplicateColumn",new)); }
                    let (views,functions) = self.dependents(&name)?;
                    if !views.is_empty() || !functions.is_empty() { return Err(unsupported("Rename a column with dependent view/function definitions requires dependency rebinding")); }
                    for column in &mut table.columns { if column.name == old { column.name = new.clone(); } for expr in column.default.iter_mut().chain(column.generated.iter_mut()) { rename_expression(expr,&old,&new); } }
                    for key in table.unique.iter_mut().chain(std::iter::once(&mut table.primary_key)) { for col in key { if *col == old { *col = new.clone(); } } }
                    table.deferred_unique = table.deferred_unique.into_iter().map(|key| key.into_iter().map(|col| if col == old { new.clone() } else { col }).collect()).collect();
                    for fk in &mut table.foreign_keys { for col in &mut fk.columns { if *col == old { *col = new.clone(); } } }
                    for check in &mut table.checks { rename_expression(check,&old,&new); }
                    for policy in &mut table.policies { for expr in policy.using.iter_mut().chain(policy.check.iter_mut()) { rename_expression(expr,&old,&new); } }
                    for row in table.rows.values_mut() { let value = row.values.remove(&old).unwrap_or(Value::Null); row.values.insert(new.clone(),value); }
                    for target in self.catalog.values_mut() { for fk in &mut target.foreign_keys { if fk.foreign_table == name { for col in &mut fk.referred_columns { if *col == old { *col = new.clone(); } } } } }
                    // Self-referential constraints are held in the staged table clone.
                    for fk in &mut table.foreign_keys { if fk.foreign_table == name { for col in &mut fk.referred_columns { if *col == old { *col = new.clone(); } } } }
                }
                AlterTableOperation::DropColumn { column_name, if_exists, cascade } => {
                    let column = ident(&column_name);
                    if !table.columns.iter().any(|c| c.name == column) { if if_exists { continue; } return Err(err("UndefinedColumn",column)); }
                    self.remove_dependents(&name,cascade)?;
                    for (source,target) in &mut self.catalog {
                        if source == &name { continue; }
                        let dependent = |fk: &ForeignKey| fk.foreign_table == name && (fk.referred_columns.contains(&column) || fk.referred_columns.is_empty() && table.primary_key.contains(&column));
                        if target.foreign_keys.iter().any(dependent) && !cascade { return Err(err("DependencyViolation",format!("{source} references {name}.{column}"))); }
                        target.foreign_keys.retain(|fk| !dependent(fk));
                    }
                    if table.columns.iter().any(|c| c.generated.as_ref().is_some_and(|e| expression_uses(e,&column))) { return Err(unsupported("DROP a column referenced by a generated column")); }
                    table.columns.retain(|c| c.name != column);
                    if table.primary_key.contains(&column) { table.primary_key.clear(); }
                    table.unique.retain(|key| !key.contains(&column));
                    table.deferred_unique.retain(|key| !key.contains(&column));
                    table.foreign_keys.retain(|fk| !fk.columns.contains(&column) && !(fk.foreign_table == name && fk.referred_columns.contains(&column)));
                    table.checks.retain(|expr| !expression_uses(expr,&column));
                    if table.policies.iter().any(|p| p.using.iter().chain(p.check.iter()).any(|e| expression_uses(e,&column))) {
                        if !cascade { return Err(err("DependencyViolation","Column is referenced by a policy")); }
                        table.policies.retain(|p| !p.using.iter().chain(p.check.iter()).any(|e| expression_uses(e,&column)));
                    }
                    for row in table.rows.values_mut() { row.values.remove(&column); row.version += 1; }
                }
                AlterTableOperation::AlterColumn { column_name,op } => {
                    let name = ident(&column_name); require_column(&table,&name)?;
                    let primary = table.primary_key.contains(&name);
                    let column = table.columns.iter_mut().find(|c| c.name == name).unwrap();
                    match op {
                        AlterColumnOperation::SetDefault { value } => column.default = Some(value),
                        AlterColumnOperation::DropDefault => column.default = None,
                        AlterColumnOperation::SetNotNull => column.nullable = false,
                        AlterColumnOperation::DropNotNull if !primary => column.nullable = true,
                        AlterColumnOperation::DropNotNull => return Err(err("ConstraintViolation","Primary-key columns cannot be nullable")),
                        _ => return Err(unsupported("ALTER COLUMN TYPE")),
                    }
                }
                AlterTableOperation::RenameTable { table_name: new } => {
                    let new = table_name(&new)?;
                    if self.catalog.contains_key(&new) || self.views.contains_key(&new) { return Err(err("DuplicateTable",new)); }
                    let (views,functions) = self.dependents(&name)?;
                    if !views.is_empty() || !functions.is_empty() { return Err(unsupported("Rename a table with dependent views/functions requires dependency rebinding")); }
                    for target in self.catalog.values_mut() { for fk in &mut target.foreign_keys { if fk.foreign_table == name { fk.foreign_table = new.clone(); } } }
                    for fk in &mut table.foreign_keys { if fk.foreign_table == name { fk.foreign_table = new.clone(); } }
                    self.catalog.remove(&name); name = new; table.name = name.clone();
                }
                _ => return Err(unsupported("ALTER TABLE operation")),
            }
            self.catalog.insert(name.clone(),table); self.schema_version += 1;
        }
        Ok(SqlResult::default())
    }

    pub(super) fn apply_referential_actions(&mut self, before: &Database, context: &AuthContext) -> Result<()> {
        let mut changes = VecDeque::new();
        for (name,old_table) in &before.catalog {
            let Some(new_table) = self.catalog.get(name) else { continue; };
            for (id,old_row) in &old_table.rows {
                match new_table.rows.get(id) { Some(new_row) if new_row.values == old_row.values => {}, next => changes.push_back((name.clone(),old_row.clone(),next.cloned())) }
            }
        }
        let mut steps = 0;
        while let Some((target_name,old,new)) = changes.pop_front() {
            steps += 1;
            if steps > 100_000 { return Err(err("ResourceLimit","Cascading referential actions exceeded limit")); }
            let target = self.table(&target_name)?.clone();
            let sources: Vec<(String,ForeignKey)> = self.catalog.iter().flat_map(|(name,table)| table.foreign_keys.iter().filter(|fk| fk.foreign_table == target_name).map(move |fk| (name.clone(),fk.clone()))).collect();
            for (source_name,fk) in sources {
                let references = if fk.referred_columns.is_empty() { &target.primary_key } else { &fk.referred_columns };
                if references.is_empty() { continue; }
                let Some(old_key) = references.iter().map(|c| old.values.get(c).cloned()).collect::<Option<Vec<_>>>() else { continue; };
                let new_key = new.as_ref().and_then(|row| references.iter().map(|c| row.values.get(c).cloned()).collect::<Option<Vec<_>>>());
                if new_key.as_ref() == Some(&old_key) || old_key.iter().any(Value::is_null) { continue; }
                let action = if new.is_some() { fk.on_update } else { fk.on_delete };
                let mut source = self.table(&source_name)?.clone();
                let rows: Vec<Row> = source.rows.values().filter(|row| fk.columns.iter().map(|c| row.values.get(c).unwrap_or(&Value::Null)).eq(old_key.iter())).cloned().collect();
                if rows.is_empty() { continue; }
                match action {
                    None | Some(ReferentialAction::NoAction) => continue,
                    Some(ReferentialAction::Restrict) => return Err(err("ConstraintViolation","Foreign-key RESTRICT action prevents mutation")),
                    Some(ReferentialAction::Cascade) if new.is_none() => {
                        for row in rows { source.rows.remove(&row.id); changes.push_back((source_name.clone(),row,None)); }
                    }
                    Some(action) => {
                        for row in rows {
                            let mut next = row.clone();
                            for (index,name) in fk.columns.iter().enumerate() {
                                let value = match action {
                                    ReferentialAction::Cascade => new_key.as_ref().ok_or_else(|| err("ConstraintViolation","Missing cascade target"))?[index].clone(),
                                    ReferentialAction::SetNull => Value::Null,
                                    ReferentialAction::SetDefault => source.columns.iter().find(|c| &c.name == name).and_then(|c| c.default.as_ref()).map(|expr| eval(expr,&BTreeMap::new(),context)).transpose()?.unwrap_or(Value::Null),
                                    _ => unreachable!(),
                                };
                                next.values.insert(name.clone(),value);
                            }
                            recompute_generated(&source,&mut next.values,context)?;
                            next.version += 1; source.rows.insert(next.id,next.clone()); changes.push_back((source_name.clone(),row,Some(next)));
                        }
                    }
                }
                self.catalog.insert(source_name,source);
            }
        }
        Ok(())
    }
}
