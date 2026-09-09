use super::*;

impl Database {
    /// sqlparser 0.41 lacks constraint timing clauses. Normalize only this
    /// well-defined CREATE TABLE extension, preserving each constraint target.
    pub(super) fn deferred_statement(&mut self, sql: &str, context: &AuthContext) -> Result<Option<SqlResult>> {
        let words = tokens(sql)?;
        if !words.iter().any(|word| word.eq_ignore_ascii_case("DEFERRABLE")) { return Ok(None); }
        if words.len() < 4 || !words[0].eq_ignore_ascii_case("CREATE") || !words[1].eq_ignore_ascii_case("TABLE") { return Err(unsupported("Constraint timing currently requires CREATE TABLE")); }
        let start = words.iter().position(|word| word == "(").ok_or_else(|| err("SyntaxError","Missing table definition"))?;
        let mut depth = 0; let mut end = None; let mut split = start+1; let mut segments = Vec::new();
        for index in start..words.len() {
            match words[index].as_str() {
                "(" => depth += 1,
                ")" => { depth -= 1; if depth == 0 { segments.push(words[split..index].to_vec()); end = Some(index); break; } },
                "," if depth == 1 => { segments.push(words[split..index].to_vec()); split = index+1; },
                _ => {},
            }
        }
        let end = end.ok_or_else(|| err("SyntaxError","Unclosed table definition"))?;
        let mut normalized = Vec::new();
        let mut unique = Vec::new(); let mut foreign = Vec::new();
        for mut segment in segments {
            let Some(index) = segment.iter().position(|w| w.eq_ignore_ascii_case("DEFERRABLE")) else { normalized.push(segment.join(" ")); continue; };
            let not_deferrable = index > 0 && segment[index-1].eq_ignore_ascii_case("NOT");
            let remove_start = if not_deferrable { index-1 } else { index };
            let suffix = &segment[index+1..];
            let initially_deferred = match suffix {
                [] => false,
                [initially,timing] if initially.eq_ignore_ascii_case("INITIALLY") && timing.eq_ignore_ascii_case("IMMEDIATE") => false,
                [initially,timing] if initially.eq_ignore_ascii_case("INITIALLY") && timing.eq_ignore_ascii_case("DEFERRED") && !not_deferrable => true,
                _ => return Err(err("SyntaxError","Expected DEFERRABLE [INITIALLY IMMEDIATE/DEFERRED] at the end of the constraint")),
            };
            segment.truncate(remove_start);
            let segment = segment.join(" ");
            let ast = Parser::parse_sql(&PostgreSqlDialect {},&format!("CREATE TABLE __constraint ({segment})")).map_err(|e| err("SyntaxError",e.to_string()))?;
            let Statement::CreateTable { columns,constraints,.. } = &ast[0] else { return Err(err("SyntaxError","Invalid timed constraint")); };
            let mut found = false;
            for column in columns {
                for option in &column.options {
                    match &option.option {
                        ColumnOption::Unique { .. } => { found = true; if initially_deferred { unique.push(vec![ident(&column.name)]); } },
                        ColumnOption::ForeignKey { .. } => { found = true; if initially_deferred { foreign.push(vec![ident(&column.name)]); } },
                        _ => {},
                    }
                }
            }
            for constraint in constraints {
                match constraint {
                    TableConstraint::Unique { columns,.. } => { found = true; if initially_deferred { unique.push(names(columns)); } },
                    TableConstraint::ForeignKey { columns,.. } => { found = true; if initially_deferred { foreign.push(names(columns)); } },
                    _ => {},
                }
            }
            if !found { return Err(unsupported("Only PRIMARY KEY, UNIQUE and FOREIGN KEY can be deferred")); }
            normalized.push(segment);
        }
        let normalized = format!("{} ( {} ) {}",words[..start].join(" "),normalized.join(" , "),words[end+1..].join(" "));
        let mut ast = Parser::parse_sql(&PostgreSqlDialect {},&normalized).map_err(|e| err("SyntaxError",e.to_string()))?;
        let name = match &ast[0] { Statement::CreateTable { name,.. } => table_name(name)?, _ => return Err(err("SyntaxError","Expected CREATE TABLE")) };
        let existed = self.catalog.contains_key(&name);
        let result = self.execute_statement(ast.remove(0),context)?;
        if !existed {
            let table = self.catalog.get_mut(&name).ok_or_else(|| err("UndefinedTable",&name))?;
            table.deferred_unique.extend(unique);
            for columns in foreign { for fk in &mut table.foreign_keys { if fk.columns == columns { fk.deferred = true; } } }
        }
        self.validate_immediate(context)?;
        Ok(Some(result))
    }
}
