use kurasql::{AuthContext,Database,FunctionReturn};
use serde_json::json;

#[test]
fn scalar_table_and_nested_sql_functions() {
    let mut db = Database::default(); let admin = AuthContext::admin();
    db.execute_sql("CREATE TABLE posts(id bigint PRIMARY KEY,title text NOT NULL); INSERT INTO posts VALUES(1,'first'),(2,'second'); CREATE FUNCTION add_one(n bigint) RETURNS bigint LANGUAGE sql AS $$ SELECT n + 1 $$; CREATE FUNCTION nested(n bigint) RETURNS bigint LANGUAGE sql AS $$ SELECT add_one(n) $$; CREATE FUNCTION get_posts(minimum bigint DEFAULT 1) RETURNS TABLE(id bigint,title text) LANGUAGE sql STABLE AS $$ SELECT id,title FROM posts WHERE id >= minimum ORDER BY id $$", &admin).unwrap();
    assert_eq!(db.call_function("add_one",&json!({"n":41}),&admin).unwrap().rows,vec![json!(42)]);
    assert_eq!(db.call_function("nested",&json!({"n":41}),&admin).unwrap().rows,vec![json!(42)]);
    assert_eq!(db.call_function("get_posts",&json!({"minimum":2}),&admin).unwrap().rows,vec![json!({"id":2,"title":"second"})]);
    assert_eq!(db.call_function("get_posts",&json!({}),&admin).unwrap().rows.len(),2);
    assert!(db.call_function("add_one",&json!({"n":"41"}),&admin).is_err());
    assert!(matches!(db.functions["get_posts"].return_kind,FunctionReturn::Table(_)));
}

#[test]
fn mutating_rpc_atomicity_and_safe_parameter_binding() {
    let mut db = Database::default(); let admin = AuthContext::admin();
    db.execute_sql("CREATE TABLE posts(id bigint PRIMARY KEY,title text NOT NULL); CREATE FUNCTION add_post(p_id bigint,p_title text) RETURNS SETOF posts LANGUAGE sql AS $$ INSERT INTO posts VALUES(p_id,p_title) RETURNING * $$; CREATE FUNCTION fail_post(p_id bigint) RETURNS void LANGUAGE sql AS $$ INSERT INTO posts VALUES(p_id,'first'); INSERT INTO posts VALUES(p_id,'duplicate') $$", &admin).unwrap();
    let title = "'; DROP TABLE posts; --";
    assert_eq!(db.call_function("add_post",&json!({"p_id":1,"p_title":title}),&admin).unwrap().rows,vec![json!({"id":1,"title":title})]);
    let before = db.clone();
    assert_eq!(db.call_function("fail_post",&json!({"p_id":2}),&admin).unwrap_err().code,"ConstraintViolation");
    assert_eq!(db,before);
}

#[test]
fn invoker_rls_and_explicit_definer_authority() {
    let mut db = Database::default(); let admin = AuthContext::admin(); let anon = AuthContext::default();
    db.execute_sql("CREATE TABLE posts(id bigint PRIMARY KEY,title text NOT NULL); ALTER TABLE posts ENABLE ROW LEVEL SECURITY; CREATE FUNCTION add_invoker(p_id bigint) RETURNS void LANGUAGE sql SECURITY INVOKER AS $$ INSERT INTO posts VALUES(p_id,'invoker') $$; CREATE FUNCTION add_definer(p_id bigint) RETURNS void LANGUAGE sql SECURITY DEFINER AS $$ INSERT INTO posts VALUES(p_id,'definer') $$",&admin).unwrap();
    assert_eq!(db.call_function("add_invoker",&json!({"p_id":1}),&anon).unwrap_err().code,"RlsViolation");
    db.call_function("add_definer",&json!({"p_id":2}),&anon).unwrap();
    assert_eq!(db.read_table("posts",&admin).unwrap().len(),1);
    assert!(db.execute_sql("CREATE FUNCTION forged() RETURNS int LANGUAGE sql SECURITY DEFINER AS $$ SELECT 1 $$",&anon).is_err());
    db.functions.get_mut("add_definer").unwrap().definer_context=None;
    assert_eq!(db.call_function("add_definer",&json!({"p_id":3}),&anon).unwrap_err().code,"PrivilegeError");
}

#[test]
fn views_are_catalog_metadata_and_indexes_are_noops() {
    let mut db = Database::default(); let admin = AuthContext::admin();
    db.execute_sql("CREATE TABLE posts(id bigint PRIMARY KEY); CREATE VIEW latest AS SELECT id FROM posts; CREATE MATERIALIZED VIEW cached AS SELECT id FROM posts",&admin).unwrap();
    assert!(!db.views["latest"].materialized); assert!(db.views["cached"].materialized);
    let before = db.clone(); db.execute_sql("CREATE INDEX post_ids ON posts(id)",&admin).unwrap(); assert_eq!(before,db);
    db.execute_sql("DROP VIEW latest",&admin).unwrap(); assert!(!db.views.contains_key("latest"));
    assert_eq!(serde_json::from_value::<Database>(serde_json::to_value(&db).unwrap()).unwrap(),db);
}
