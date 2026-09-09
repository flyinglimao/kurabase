use crate::{ApiError, Result};
use kurasql::{AuthContext, Database, SqlResult, Table};
use sqlparser::{ast::Statement,dialect::PostgreSqlDialect,parser::Parser};
use std::collections::BTreeMap;

#[derive(Clone)]
pub struct Materialized {
    pub name:String,
    pub definition:String,
    pub context:AuthContext,
    pub table:Table,
}
pub type Cache=BTreeMap<String,Materialized>;

fn cache_key(name:&str,sql:&str,context:&AuthContext,schema_version:u64)->Result<String> {
    // A materialized value is intentionally stale across row writes until an
    // administrative REFRESH. It must never outlive authorization, policy, or
    // definition changes, which all advance the canonical schema version.
    Ok(format!("{name}:{}",alloy::primitives::keccak256(serde_json::to_vec(&(sql,context,schema_version)).map_err(ApiError::internal)?)))
}

fn dependencies(db:&Database,sql:&str,path:&mut Vec<String>,result:&mut Vec<String>)->Result<()> {
    let statements=Parser::parse_sql(&PostgreSqlDialect{},sql).map_err(|_|ApiError::invalid("Invalid view definition"))?;
    let mut names=Vec::new();
    let _:std::ops::ControlFlow<()> = sqlparser::ast::visit_relations(&statements,|name|{if let Ok(n)=kurasql::table_name(name){names.push(n);}std::ops::ControlFlow::Continue(())});
    for name in names {
        if let Some(view)=db.views.get(&name) {
            if path.contains(&name)||path.len()>64 {return Err(ApiError::invalid("Cyclic or deeply nested view"));}
            path.push(name.clone());dependencies(db,&view.sql,path,result)?;path.pop();
            if view.materialized && !result.contains(&name){result.push(name);}
        }
    }
    Ok(())
}
pub async fn prepared(db:&Database,sql:&str,context:&AuthContext,cache:&mut Cache)->Result<Database> {
    let mut result=db.clone();
    let mut names=Vec::new();dependencies(db,sql,&mut Vec::new(),&mut names)?;
    for name in &names {
        let view=&db.views[name];
        let key=cache_key(name,&view.sql,context,db.schema_version)?;
        if !cache.contains_key(&key) {
            let table=kura_query::materialize(&result,&view.sql,context,name).await?;
            cache.insert(key.clone(),Materialized{name:name.clone(),definition:view.sql.clone(),context:context.clone(),table});
        }
        result.views.remove(name);
        result.catalog.insert(name.clone(),cache[&key].table.clone());
    }
    Ok(result)
}

/// Returns None for statements handled by the transactional SQL core.
pub async fn execute(db:&Database,sql:&str,context:&AuthContext,cache:&mut Cache)->Result<Option<SqlResult>> {
    use sqlparser::tokenizer::{Token,Tokenizer};
    let mut tokens=Tokenizer::new(&PostgreSqlDialect{},sql).tokenize().unwrap_or_default().into_iter().filter(|t|!matches!(t,Token::Whitespace(_))).collect::<Vec<_>>();
    if tokens.last()==Some(&Token::SemiColon){tokens.pop();}
    let word=|token:Option<&Token>,expected:&str| matches!(token,Some(Token::Word(w)) if w.quote_style.is_none() && w.value.eq_ignore_ascii_case(expected));
    if word(tokens.first(),"REFRESH") {
        if !word(tokens.get(1),"MATERIALIZED") || !word(tokens.get(2),"VIEW") {return Err(ApiError::invalid("Expected REFRESH MATERIALIZED VIEW name"));}
        let name=match &tokens[3..] {
            [Token::Word(name)]=>if name.quote_style.is_some(){name.value.clone()}else{name.value.to_lowercase()},
            [Token::Word(schema),Token::Period,Token::Word(name)] if schema.value.eq_ignore_ascii_case("public")=>if name.quote_style.is_some(){name.value.clone()}else{name.value.to_lowercase()},
            _=>return Err(ApiError::invalid("Refresh requires one local view name; CONCURRENTLY is unsupported")),
        };
        if !context.privileged {return Err(ApiError::unauthorized("Materialized view refresh requires administrative authority"));}
        let view=db.views.get(&name).filter(|v|v.materialized).ok_or_else(||ApiError::invalid("Unknown materialized view"))?;
        let mut replacements=Vec::new();
        for (key,value) in cache.iter().filter(|(_,v)|v.name==name && v.definition==view.sql) {
            let table=kura_query::materialize(db,&view.sql,&value.context,&name).await?;
            replacements.push((key.clone(),Materialized{name:name.clone(),definition:view.sql.clone(),context:value.context.clone(),table}));
        }
        let key=cache_key(&name,&view.sql,context,db.schema_version)?;
        if !replacements.iter().any(|(k,_)|k==&key) {
            let table=kura_query::materialize(db,&view.sql,context,&name).await?;
            replacements.push((key,Materialized{name,definition:view.sql.clone(),context:context.clone(),table}));
        }
        for (key,value) in replacements {cache.insert(key,value);}
        return Ok(Some(SqlResult::default()));
    }
    // Function bodies and unsupported PostgreSQL extensions stay with the core parser.
    let statements=match Parser::parse_sql(&PostgreSqlDialect{},sql) {Ok(s)=>s,Err(_)=>return Ok(None)};
    if statements.len()!=1 {return Ok(None);}
    match &statements[0] {
        Statement::Query(_)=>{
            let ready=prepared(db,sql,context,cache).await?;
            match kura_query::execute(&ready,sql,context).await {
                Ok(rows)=>Ok(Some(SqlResult{rows,affected:0})),
                // The query engine owns every SELECT accepted here. Falling
                // back to the small transactional evaluator would silently
                // shrink semantics (for example CTE/window queries).
                Err(e)=>Err(e.into()),
            }
        }
        _=>Ok(None),
    }
}
