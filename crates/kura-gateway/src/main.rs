mod auth;
mod postgrest;
mod projection;
mod query;

use auth::{Auth, Envelope};
use axum::{body::Bytes, extract::{DefaultBodyLimit, Path, RawQuery, State}, http::{HeaderMap, HeaderValue, Method, StatusCode}, response::{IntoResponse, Response}, routing::{any, get, post}, Json, Router};
use kura_chain::Chain;
use kurasql::{AuthContext, Database, SqlResult};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{env, sync::Arc};
use tokio::sync::Mutex;
use tower_http::cors::{Any, CorsLayer};

#[derive(Clone)]
struct App { chain: Chain, publishable_key: String, secret_key: String, writer: Arc<Mutex<()>>, materialized:Arc<Mutex<query::Cache>> }

#[derive(Debug)]
pub struct ApiError { status: StatusCode, code: String, message: String }
impl ApiError {
    fn unauthorized(message: impl Into<String>) -> Self { Self { status:StatusCode::UNAUTHORIZED, code:"PrivilegeError".into(), message:message.into() } }
    fn invalid(message: impl Into<String>) -> Self { Self { status:StatusCode::BAD_REQUEST, code:"InvalidRequest".into(), message:message.into() } }
    fn internal(error: impl std::fmt::Display) -> Self { tracing::error!("{error}"); Self { status:StatusCode::INTERNAL_SERVER_ERROR, code:"ExecutionFailure".into(), message:"Execution failed; consult gateway logs".into() } }
    fn chain(error: impl std::fmt::Display) -> Self { tracing::warn!("Chain operation: {error}"); Self { status:StatusCode::CONFLICT, code:"TransactionConflict".into(), message:"Chain rejected the plan; refresh state and authorization before retrying".into() } }
}
impl From<kurasql::SqlError> for ApiError {
    fn from(e: kurasql::SqlError) -> Self { Self { status:match e.code.as_str() { "RlsViolation" | "PrivilegeError" => StatusCode::FORBIDDEN, "ConstraintViolation" => StatusCode::CONFLICT, _ => StatusCode::BAD_REQUEST }, code:e.code, message:e.message } }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response { (self.status,Json(json!({"code":self.code,"message":self.message,"details":null,"hint":null}))).into_response() }
}
type Result<T> = std::result::Result<T, ApiError>;
fn header<'a>(headers: &'a HeaderMap, key: &str) -> &'a str { headers.get(key).and_then(|v|v.to_str().ok()).unwrap_or("") }

impl App {
    async fn auth(&self, headers: &HeaderMap, admin: bool) -> Result<Auth> {
        let api_key = header(headers,"apikey");
        if api_key != self.publishable_key && api_key != self.secret_key { return Err(ApiError::unauthorized("Invalid project key")); }
        let token = header(headers,"authorization").strip_prefix("Bearer ").unwrap_or("");
        if api_key == self.secret_key && (token.is_empty() || token == self.secret_key) {
            if !self.chain.privileged().await.map_err(ApiError::internal)? { return Err(ApiError::unauthorized("Gateway privileged delegation is absent or revoked")); }
            return Ok(Auth { context:AuthContext::admin(), envelope:None });
        }
        if admin { return Err(ApiError::unauthorized("Administrative endpoint requires the project secret key")); }
        if token.is_empty() || token == self.publishable_key { return Ok(Auth { context:AuthContext::default(), envelope:None }); }
        let envelope = Envelope::decode(token)?;
        let context = envelope.verify(&self.chain).await?;
        Ok(Auth { context, envelope:Some(envelope) })
    }
    async fn current(&self) -> Result<(kura_chain::ChainSnapshot, Database)> {
        let snapshot = self.chain.snapshot().await.map_err(ApiError::internal)?;
        let db = projection::rebuild(&snapshot).map_err(ApiError::internal)?;
        Ok((snapshot,db))
    }
    async fn commit(&self, before: &Database, after: &Database, revision: u64, auth: &Auth) -> Result<Option<String>> {
        let ops = projection::plan(before,after).map_err(ApiError::internal)?;
        if ops.is_empty() { return Ok(None); }
        if !auth.context.privileged && auth.envelope.is_none() { return Err(ApiError::unauthorized("Writes require an independently signed user session")); }
        let session = auth.envelope.as_ref().map(|e| Ok::<_,ApiError>((e.session()?,e.signature.clone()))).transpose()?;
        let hash = self.chain.commit(revision,ops,session).await.map_err(ApiError::chain)?;
        Ok(Some(format!("{hash:#x}")))
    }
}

async fn health(State(app): State<App>) -> Result<Json<Value>> {
    let (snapshot,_) = app.current().await?;
    Ok(Json(json!({"status":"ok","block_number":snapshot.block_number,"block_hash":snapshot.block_hash,"revision":snapshot.revision})))
}
async fn instance(State(app): State<App>, headers: HeaderMap) -> Result<Json<Value>> {
    app.auth(&headers,false).await?;
    let mut info = app.chain.info().await.map_err(ApiError::internal)?;
    let (snapshot,_) = app.current().await?;
    info["block_number"] = snapshot.block_number.into(); info["revision"] = snapshot.revision.into();
    Ok(Json(info))
}
async fn catalog(State(app): State<App>, headers: HeaderMap) -> Result<Json<Value>> {
    app.auth(&headers,false).await?;
    let (_,mut db) = app.current().await?;
    for table in db.catalog.values_mut() { table.rows.clear(); }
    Ok(Json(serde_json::to_value(db).map_err(ApiError::internal)?))
}
#[derive(Deserialize)]
struct SqlRequest { sql:String, version:Option<String> }
async fn sql(State(app): State<App>, headers: HeaderMap, Json(request): Json<SqlRequest>) -> Result<Json<Value>> {
    let auth = app.auth(&headers,true).await?;
    let _guard = app.writer.lock().await;
    let (snapshot,db) = app.current().await?;
    let mut staged = db.clone();
    let SqlResult { rows, affected } = if let Some(version) = request.version {
        if version.is_empty() || !version.bytes().all(|b|b.is_ascii_digit()) { return Err(ApiError::invalid("Migration version must be a nonempty timestamp identifier")); }
        staged.apply_migration(&version,&request.sql,&auth.context)?
    } else {
        let mut cache=app.materialized.lock().await;
        match query::execute(&staged,&request.sql,&auth.context,&mut cache).await? {
            Some(result)=>result,
            None=>staged.execute_sql(&request.sql,&auth.context)?,
        }
    };
    let transaction_hash = app.commit(&db,&staged,snapshot.revision,&auth).await?;
    let (current,_) = app.current().await?;
    Ok(Json(json!({"rows":rows,"affected":affected,"transaction_hash":transaction_hash,"block_number":current.block_number,"revision":current.revision})))
}
async fn migrations(State(app): State<App>, headers: HeaderMap) -> Result<Json<Value>> {
    app.auth(&headers,true).await?;
    let (_,db) = app.current().await?;
    Ok(Json(json!({"migrations":db.migrations})))
}
async fn login(State(app): State<App>, headers: HeaderMap, Json(envelope): Json<Envelope>) -> Result<Json<Value>> {
    if header(&headers,"apikey") != app.publishable_key { return Err(ApiError::unauthorized("Use the publishable project key for session exchange")); }
    let context = envelope.verify(&app.chain).await?;
    Ok(Json(json!({"access_token":envelope.token()?,"token_type":"bearer","user":{"id":context.uid},"expires_at":envelope.session.expires_at})))
}

async fn table(State(app): State<App>, Path(name): Path<String>, RawQuery(raw): RawQuery, method: Method, headers: HeaderMap, body: Bytes) -> Result<Response> {
    let auth = app.auth(&headers,false).await?;
    let params: postgrest::Params = url::form_urlencoded::parse(raw.as_deref().unwrap_or("").as_bytes()).into_owned().collect();
    let is_read = method==Method::GET || method==Method::HEAD;
    let _guard = if is_read { None } else { Some(app.writer.lock().await) };
    let (snapshot,mut db) = app.current().await?;
    if is_read && db.views.contains_key(&name) {
        let statement=format!("SELECT * FROM {}",postgrest::ident(&name)?);
        let mut cache=app.materialized.lock().await;
        db=query::prepared(&db,&statement,&auth.context,&mut cache).await?;
        if db.views.contains_key(&name) {
            let relation=kura_query::materialize(&db,&statement,&auth.context,&name).await?;
            db.views.remove(&name);db.catalog.insert(name.clone(),relation);
        }
    }
    let table = db.catalog.get(&name).ok_or_else(||ApiError::invalid(format!("Unknown table {name}")))?;
    let prefer = header(&headers,"prefer");
    let mut staged = db.clone();
    let statement = if is_read { postgrest::select_sql(table,&params)? } else {
        let value = if body.is_empty() { Value::Null } else { serde_json::from_slice(&body).map_err(|_|ApiError::invalid("Invalid JSON body"))? };
        postgrest::mutation_sql(method.as_str(),table,&params,&value,prefer)?
    };
    let result = staged.execute_sql(&statement,&auth.context)?;
    let mut rows = result.rows;
    let select=postgrest::get(&params,"select").unwrap_or("*");
    if is_read { rows=postgrest::filter_referenced_rows(rows,select,&params,&staged,table,&auth.context)?; }
    let total=rows.len();
    let number = |key: &str| -> Result<Option<usize>> { postgrest::get(&params,key).map(|v|v.parse().map_err(|_|ApiError::invalid(format!("Invalid {key}")))).transpose() };
    let mut offset = number("offset")?.unwrap_or(0); let mut limit = number("limit")?.unwrap_or(usize::MAX);
    if is_read && !header(&headers,"range").is_empty() {
        let range = header(&headers,"range").strip_prefix("items=").unwrap_or(header(&headers,"range"));
        let (start,end) = range.split_once('-').ok_or_else(||ApiError::invalid("Invalid range"))?;
        offset = start.parse().map_err(|_|ApiError::invalid("Invalid range start"))?;
        if !end.is_empty() { let end:usize = end.parse().map_err(|_|ApiError::invalid("Invalid range end"))?; limit = end.checked_sub(offset).and_then(|v|v.checked_add(1)).ok_or_else(||ApiError::invalid("Invalid range bounds"))?; }
    }
    if is_read { rows = rows.into_iter().skip(offset).take(limit).collect(); }
    rows = postgrest::project_with_params(rows,select,&params,&staged,table,&auth.context)?;
    let representation = is_read || prefer.contains("return=representation");
    let singular = header(&headers,"accept").contains("application/vnd.pgrst.object+json");
    if singular && representation && rows.len()!=1 { return Err(ApiError { status:StatusCode::NOT_ACCEPTABLE,code:"PGRST116".into(),message:format!("JSON object requested, multiple (or no) rows returned; result contains {} rows",rows.len()) }); }
    let transaction_hash = if is_read { None } else { app.commit(&db,&staged,snapshot.revision,&auth).await? };
    let status = if is_read { StatusCode::OK } else if method==Method::POST { StatusCode::CREATED } else if representation { StatusCode::OK } else { StatusCode::NO_CONTENT };
    let mut response = if !representation || method==Method::HEAD { status.into_response() } else { (status,Json(if singular { rows[0].clone() } else { Value::Array(rows.clone()) })).into_response() };
    let count = if prefer.contains("count=exact") { total.to_string() } else { "*".into() };
    let content_range = if rows.is_empty() { format!("*/{count}") } else { format!("{}-{}/{count}",offset,offset+rows.len()-1) };
    response.headers_mut().insert("content-range",HeaderValue::from_str(&content_range).map_err(ApiError::internal)?);
    response.headers_mut().insert("x-kurabase-block",HeaderValue::from(snapshot.block_number));
    response.headers_mut().insert("x-kurabase-revision",HeaderValue::from(snapshot.revision));
    if let Some(hash)=transaction_hash { response.headers_mut().insert("x-kurabase-transaction",HeaderValue::from_str(&hash).map_err(ApiError::internal)?); }
    Ok(response)
}
async fn rpc(State(app): State<App>, Path(name): Path<String>, RawQuery(raw): RawQuery, method: Method, headers: HeaderMap, body: Bytes) -> Result<Response> {
    let auth = app.auth(&headers,false).await?;
    let mut params:postgrest::Params = url::form_urlencoded::parse(raw.as_deref().unwrap_or("").as_bytes()).into_owned().collect();
    let _guard = app.writer.lock().await;
    let (snapshot,db) = app.current().await?;
    let function = db.functions.get(&name).ok_or_else(||ApiError::invalid(format!("Unknown function {name}")))?.clone();
    let args = if method==Method::GET || method==Method::HEAD {
        let mut values = serde_json::Map::new();
        for parameter in &function.parameters {
            if let Some(index) = params.iter().position(|(key,_)| key==&parameter.name) {
                let (_,value)=params.remove(index);
                let value=if matches!(parameter.data_type.as_str(),"integer"|"int"|"bigint"|"smallint"|"boolean"|"bool") { serde_json::from_str(&value).map_err(|_|ApiError::invalid("Invalid RPC parameter type"))? } else { Value::String(value) };
                values.insert(parameter.name.clone(),value);
            }
        }
        Value::Object(values)
    } else if method==Method::POST {
        if body.is_empty() { json!({}) } else { serde_json::from_slice(&body).map_err(|_|ApiError::invalid("Invalid RPC JSON"))? }
    } else { return Err(ApiError::invalid("RPC supports GET, HEAD and POST")); };
    let mut staged=db.clone();
    let result=staged.call_function(&name,&args,&auth.context)?;
    let scalar=matches!(function.return_kind,kurasql::FunctionReturn::Scalar(_)|kurasql::FunctionReturn::Void);
    let mut rows=result.rows;
    if !scalar {
        // Function results already reflect INVOKER/DEFINER authority. Filters operate on that returned relation.
        let mut output=match &function.return_kind {
            kurasql::FunctionReturn::SetOf(table)=>db.table(table)?.clone(),
            kurasql::FunctionReturn::Table(columns)=>{
                let mut tmp=Database::default();
                let definition=columns.iter().map(|(n,t)|Ok(format!("{} {t}",postgrest::ident(n)?))).collect::<std::result::Result<Vec<_>,kurasql::SqlError>>()?.join(",");
                tmp.execute_sql(&format!("CREATE TABLE rpc_result ({definition})"),&AuthContext::admin())?;
                tmp.catalog.remove("rpc_result").unwrap()
            }
            _=>unreachable!(),
        };
        output.name="rpc_result".into();output.rls_enabled=false;output.rows.clear();
        output.primary_key.clear();output.unique.clear();output.foreign_keys.clear();output.checks.clear();output.policies.clear();
        for (index,row) in rows.into_iter().enumerate() {
            let object=row.as_object().ok_or_else(||ApiError::invalid("RPC returned a non-row value"))?;
            output.rows.insert(index as u64+1,kurasql::Row{id:index as u64+1,version:0,values:object.iter().map(|(k,v)|(k.clone(),v.clone())).collect()});
        }
        let mut temp=Database::default();temp.catalog.insert(output.name.clone(),output.clone());
        let statement=postgrest::select_sql(&output,&params)?;
        rows=temp.execute_sql(&statement,&AuthContext::admin())?.rows;
        rows=postgrest::project(rows,postgrest::get(&params,"select").unwrap_or("*"),&temp,&output,&AuthContext::admin())?;
    } else if !params.is_empty() { return Err(ApiError::invalid("Scalar RPC does not support relation filters")); }
    let total=rows.len();
    let offset=postgrest::get(&params,"offset").unwrap_or("0").parse::<usize>().map_err(|_|ApiError::invalid("Invalid offset"))?;
    let limit=postgrest::get(&params,"limit").map(str::parse::<usize>).transpose().map_err(|_|ApiError::invalid("Invalid limit"))?.unwrap_or(usize::MAX);
    rows=rows.into_iter().skip(offset).take(limit).collect();
    let singular=header(&headers,"accept").contains("application/vnd.pgrst.object+json");
    if singular && rows.len()!=1 { return Err(ApiError{status:StatusCode::NOT_ACCEPTABLE,code:"PGRST116".into(),message:format!("Result contains {} rows",rows.len())}); }
    if method!=Method::POST && !projection::plan(&db,&staged).map_err(ApiError::internal)?.is_empty() { return Err(ApiError::invalid("Mutating RPC requires POST")); }
    let transaction=app.commit(&db,&staged,snapshot.revision,&auth).await?;
    let value=if scalar || singular { rows.first().cloned().unwrap_or(Value::Null) } else { Value::Array(rows.clone()) };
    let mut response=if method==Method::HEAD { StatusCode::OK.into_response() } else { Json(value).into_response() };
    let count=if header(&headers,"prefer").contains("count=exact") {total.to_string()} else {"*".into()};
    let range=if rows.is_empty(){format!("*/{count}")}else{format!("{offset}-{}/{count}",offset+rows.len()-1)};
    response.headers_mut().insert("content-range",HeaderValue::from_str(&range).map_err(ApiError::internal)?);
    if let Some(hash)=transaction{response.headers_mut().insert("x-kurabase-transaction",HeaderValue::from_str(&hash).map_err(ApiError::internal)?);}
    Ok(response)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_|"info".into())).init();
    let required = |name:&str| env::var(name).map_err(|_|anyhow::anyhow!("Missing {name}"));
    let chain = Chain::connect(&env::var("KURA_RPC_URL").unwrap_or_else(|_|"http://127.0.0.1:8545".into()),&required("KURA_CONTRACT")?,&required("KURA_PRIVATE_KEY")?,env::var("KURA_DEPLOYMENT_BLOCK").unwrap_or_else(|_|"0".into()).parse()?).await?;
    let app = App { chain,publishable_key:required("KURA_PUBLISHABLE_KEY")?,secret_key:required("KURA_SECRET_KEY")?,writer:Arc::new(Mutex::new(())),materialized:Arc::new(Mutex::new(query::Cache::new())) };
    if app.publishable_key.is_empty() || app.secret_key.is_empty() || app.publishable_key==app.secret_key { anyhow::bail!("Project keys must be nonempty and different"); }
    let cors = CorsLayer::new().allow_origin(Any).allow_methods(Any).allow_headers(Any).expose_headers(["content-range".parse().unwrap(),"x-kurabase-block".parse().unwrap(),"x-kurabase-revision".parse().unwrap(),"x-kurabase-transaction".parse().unwrap()]);
    let router = Router::new().route("/health",get(health)).route("/admin/v1/instance",get(instance)).route("/admin/v1/catalog",get(catalog)).route("/admin/v1/sql",post(sql)).route("/admin/v1/migrations",get(migrations).post(sql)).route("/auth/v1/session",post(login)).route("/rest/v1/rpc/{function}",any(rpc)).route("/rest/v1/{table}",any(table)).layer(DefaultBodyLimit::max(1024*1024)).layer(cors).with_state(app);
    let address = env::var("KURA_BIND").unwrap_or_else(|_|"127.0.0.1:54321".into());
    let listener = tokio::net::TcpListener::bind(&address).await?;
    tracing::info!("Kurabase listening at http://{address}");
    axum::serve(listener,router).await?;
    Ok(())
}
