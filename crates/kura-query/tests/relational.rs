use kura_query::{execute, materialize};
use kurasql::{AuthContext, Database, View};
use serde_json::json;

fn fixture() -> Database {
    let mut db = Database::default();
    db.execute_sql(
        "CREATE TABLE authors (id bigint PRIMARY KEY, name text NOT NULL, owner text NOT NULL);
         CREATE TABLE books (id bigint PRIMARY KEY, author_id bigint REFERENCES authors(id), title text, pages bigint, owner text NOT NULL);
         INSERT INTO authors VALUES (1, 'Alice', 'alice'), (2, 'Bob', 'bob'), (3, 'Carol', 'alice');
         INSERT INTO books VALUES (10, 1, 'Alpha', 100, 'alice'), (11, 1, 'Beta', 200, 'alice'), (12, 2, 'Gamma', NULL, 'bob');",
        &AuthContext::admin(),
    ).unwrap();
    db
}

fn enable_rls(db: &mut Database) {
    db.execute_sql(
        "ALTER TABLE authors ENABLE ROW LEVEL SECURITY;
         CREATE POLICY own_authors ON authors FOR SELECT TO authenticated USING (owner = auth.uid());
         ALTER TABLE books ENABLE ROW LEVEL SECURITY;
         CREATE POLICY own_books ON books FOR SELECT TO authenticated USING (owner = auth.uid());",
        &AuthContext::admin(),
    ).unwrap();
}

#[tokio::test]
async fn joins_grouping_and_having_use_real_relations() {
    let db = fixture();
    let rows = execute(&db,
        "SELECT a.name, COUNT(b.id) AS books, SUM(b.pages) AS pages FROM authors a LEFT JOIN books b ON a.id = b.author_id GROUP BY a.name HAVING COUNT(b.id) > 0 ORDER BY a.name",
        &AuthContext::admin()).await.unwrap();
    assert_eq!(rows, vec![json!({"name":"Alice","books":2,"pages":300}), json!({"name":"Bob","books":1,"pages":null})]);
    let rows = execute(&db, "SELECT COUNT(*) AS n FROM authors CROSS JOIN books", &AuthContext::admin()).await.unwrap();
    assert_eq!(rows, vec![json!({"n":9})]);
    for join in ["RIGHT", "FULL"] {
        let sql = format!("SELECT a.id AS author_id, b.id AS book_id FROM books b {join} JOIN authors a ON a.id = b.author_id ORDER BY author_id, book_id");
        let rows = execute(&db, &sql, &AuthContext::admin()).await.unwrap();
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[3], json!({"author_id":3,"book_id":null}));
    }
}

#[tokio::test]
async fn ctes_derived_tables_correlated_subqueries_and_windows() {
    let db = fixture();
    let rows = execute(&db,
        "WITH ranked AS (SELECT id, title, ROW_NUMBER() OVER (PARTITION BY author_id ORDER BY pages DESC NULLS LAST) AS position FROM books) SELECT title, position FROM ranked WHERE position = 1 ORDER BY title",
        &AuthContext::admin()).await.unwrap();
    assert_eq!(rows, vec![json!({"title":"Beta","position":1}), json!({"title":"Gamma","position":1})]);
    let rows = execute(&db,
        "SELECT a.name FROM authors a WHERE EXISTS (SELECT 1 FROM books b WHERE b.author_id = a.id AND b.pages > 150) ORDER BY a.name",
        &AuthContext::admin()).await.unwrap();
    assert_eq!(rows, vec![json!({"name":"Alice"})]);
    let rows = execute(&db,
        "SELECT x.title FROM (SELECT title, author_id FROM books) x WHERE x.author_id IN (SELECT id FROM authors WHERE name = 'Alice') ORDER BY title",
        &AuthContext::admin()).await.unwrap();
    assert_eq!(rows, vec![json!({"title":"Alpha"}), json!({"title":"Beta"})]);
}

#[tokio::test]
async fn union_intersection_and_except_preserve_set_semantics() {
    let db = fixture();
    for (sql, expected) in [
        ("SELECT owner FROM authors UNION SELECT owner FROM books ORDER BY owner", vec![json!({"owner":"alice"}), json!({"owner":"bob"})]),
        ("SELECT id FROM authors INTERSECT SELECT author_id FROM books ORDER BY id", vec![json!({"id":1}), json!({"id":2})]),
        ("SELECT id FROM authors EXCEPT SELECT author_id FROM books ORDER BY id", vec![json!({"id":3})]),
    ] {
        assert_eq!(execute(&db, sql, &AuthContext::admin()).await.unwrap(), expected);
    }
    let rows = execute(&db, "SELECT owner FROM authors UNION ALL SELECT owner FROM books", &AuthContext::admin()).await.unwrap();
    assert_eq!(rows.len(), 6);
}

#[tokio::test]
async fn null_logic_and_empty_aggregates() {
    let db = fixture();
    let rows = execute(&db,
        "SELECT id, pages = NULL AS unknown, pages IS NULL AS missing, COALESCE(pages, 0) AS value FROM books WHERE pages IS NULL ORDER BY id",
        &AuthContext::admin()).await.unwrap();
    assert_eq!(rows, vec![json!({"id":12,"unknown":null,"missing":true,"value":0})]);
    let rows = execute(&db, "SELECT COUNT(*) AS n, SUM(pages) AS total FROM books WHERE id = -1", &AuthContext::admin()).await.unwrap();
    assert_eq!(rows, vec![json!({"n":0,"total":null})]);
    assert!(execute(&db, "SELECT id FROM books WHERE pages = NULL", &AuthContext::admin()).await.unwrap().is_empty());
    let rows = execute(&db, "SELECT NOT NULL AS a, FALSE AND NULL AS b, TRUE OR NULL AS c", &AuthContext::admin()).await.unwrap();
    assert_eq!(rows, vec![json!({"a":null,"b":false,"c":true})]);
}

#[tokio::test]
async fn rls_applies_before_joins_aggregates_and_subqueries() {
    let mut db = fixture(); enable_rls(&mut db);
    let alice = AuthContext::authenticated("alice");
    let bob = AuthContext::authenticated("bob");
    let sql = "SELECT a.name, COUNT(b.id) AS n FROM authors a LEFT JOIN books b ON a.id = b.author_id GROUP BY a.name ORDER BY a.name";
    assert_eq!(execute(&db, sql, &alice).await.unwrap(), vec![json!({"name":"Alice","n":2}),json!({"name":"Carol","n":0})]);
    assert_eq!(execute(&db, sql, &bob).await.unwrap(), vec![json!({"name":"Bob","n":1})]);
    assert!(execute(&db, sql, &AuthContext::default()).await.unwrap().is_empty());
    assert_eq!(execute(&db, "SELECT COUNT(*) AS n FROM books", &AuthContext::default()).await.unwrap(), vec![json!({"n":0})]);
    assert!(execute(&db, "SELECT id FROM authors WHERE EXISTS (SELECT 1 FROM books WHERE title = 'Gamma')", &alice).await.unwrap().is_empty());
}

#[tokio::test]
async fn identity_functions_are_bound_to_context_and_safely_quoted() {
    let db = fixture();
    let mut auth = AuthContext::authenticated("alice'; DROP TABLE books; --");
    auth.jwt_claims = json!({"role":"authenticated","user_metadata":{"name":"O'Reilly"}});
    let rows = execute(&db, "SELECT auth.uid() AS uid", &auth).await.unwrap();
    assert_eq!(rows, vec![json!({"uid":auth.uid})]);
    assert_eq!(execute(&db, "SELECT auth.jwt() AS claims", &auth).await.unwrap_err().code, "UnsupportedFeature");
    assert_eq!(db.catalog.len(), 2);
}

#[tokio::test]
async fn views_preserve_invoker_rls_and_isolate_catalog_scope() {
    let mut db = fixture(); enable_rls(&mut db);
    db.views.insert("visible_books".into(), View { sql:"SELECT id, title, owner FROM books".into(), materialized:false });
    db.views.insert("nested_books".into(), View { sql:"SELECT title FROM visible_books".into(), materialized:false });
    let rows = execute(&db, "SELECT title FROM nested_books ORDER BY title", &AuthContext::authenticated("alice")).await.unwrap();
    assert_eq!(rows, vec![json!({"title":"Alpha"}),json!({"title":"Beta"})]);
    let rows = execute(&db, "WITH books AS (SELECT 999 AS id) SELECT title FROM visible_books ORDER BY title", &AuthContext::authenticated("alice")).await.unwrap();
    assert_eq!(rows.len(), 2);
    assert!(execute(&db, "SELECT * FROM visible_books", &AuthContext::default()).await.unwrap().is_empty());
}

#[tokio::test]
async fn rejects_unsafe_sources_and_statements_including_nested_forms() {
    let db = fixture();
    for sql in [
        "DELETE FROM books", "CREATE EXTERNAL TABLE x STORED AS CSV LOCATION '/etc/passwd'",
        "SELECT 1; SELECT 2", "SELECT * INTO stolen FROM books", "SELECT * FROM books FOR UPDATE",
        "SELECT * FROM read_csv('/etc/passwd')", "SELECT * FROM '/etc/passwd'",
        "SELECT * FROM information_schema.tables", "SELECT read_file('/etc/passwd')",
        "WITH source AS (SELECT * FROM read_parquet('https://example.com/file')) SELECT * FROM source",
        "SELECT id FROM books WHERE EXISTS (SELECT 1 FROM read_csv('/etc/passwd'))",
        "SELECT * FROM (SELECT * INTO x FROM books) nested",
        "WITH changed AS (DELETE FROM books RETURNING *) SELECT * FROM changed",
        "SELECT CAST(id AS numeric(70,2)) FROM books",
        "SELECT arbitrary_extension(id) FROM books",
    ] {
        assert!(execute(&db, sql, &AuthContext::admin()).await.is_err(), "accepted unsafe SQL: {sql}");
    }
    assert_eq!(db.catalog["books"].rows.len(), 3);
}

#[tokio::test]
async fn unknown_and_corrupted_canonical_types_fail_closed() {
    let mut db = fixture();
    db.catalog.get_mut("books").unwrap().columns.iter_mut().find(|c| c.name == "pages").unwrap().data_type = "numeric(70,2)".into();
    assert_eq!(execute(&db, "SELECT pages FROM books", &AuthContext::admin()).await.unwrap_err().code, "UnsupportedFeature");
    db.catalog.get_mut("books").unwrap().columns.iter_mut().find(|c| c.name == "pages").unwrap().data_type = "bigint".into();
    db.catalog.get_mut("books").unwrap().rows.values_mut().next().unwrap().values.insert("pages".into(), json!("100"));
    assert_eq!(execute(&db, "SELECT pages FROM books", &AuthContext::admin()).await.unwrap_err().code, "TypeError");
}

#[tokio::test]
async fn incompatible_operands_require_explicit_casts() {
    let db = fixture();
    for sql in ["SELECT 1 = '1' AS wrong", "SELECT id FROM books WHERE id = '10'", "SELECT title + pages FROM books"] {
        assert_eq!(execute(&db, sql, &AuthContext::admin()).await.unwrap_err().code, "TypeError", "{sql}");
    }
    assert_eq!(execute(&db, "SELECT id FROM books WHERE id = CAST('10' AS bigint)", &AuthContext::admin()).await.unwrap(), vec![json!({"id":10})]);
}

#[tokio::test]
async fn preserves_json_values_and_aliases() {
    let mut db = Database::default();
    db.execute_sql("CREATE TABLE entries (id bigint, payload jsonb); INSERT INTO entries VALUES (1, '{\"a\":[1,true,null]}'::jsonb), (2, '\"hello\"'::jsonb), (3, NULL);", &AuthContext::admin()).unwrap();
    let rows = execute(&db, "SELECT payload AS renamed FROM entries ORDER BY id", &AuthContext::admin()).await.unwrap();
    assert_eq!(rows, vec![json!({"renamed":{"a":[1,true,null]}}), json!({"renamed":"hello"}), json!({"renamed":null})]);
    for sql in ["SELECT lower(payload) FROM entries", "SELECT id FROM entries WHERE payload = payload", "SELECT payload FROM entries ORDER BY payload", "SELECT DISTINCT payload FROM entries"] {
        assert_eq!(execute(&db, sql, &AuthContext::admin()).await.unwrap_err().code, "UnsupportedFeature", "{sql}");
    }
    let cached = materialize(&db, "SELECT payload AS renamed FROM entries ORDER BY id", &AuthContext::admin(), "cached_json").await.unwrap();
    assert_eq!(cached.columns[0].data_type, "jsonb");
    assert_eq!(cached.rows[&1].values["renamed"], json!({"a":[1,true,null]}));
}

#[tokio::test]
async fn materialization_retains_empty_schema_and_is_partitioned_by_caller() {
    let mut db = fixture(); enable_rls(&mut db);
    let empty = materialize(&db, "SELECT id, title FROM books", &AuthContext::default(), "cached").await.unwrap();
    assert!(empty.rows.is_empty());
    assert_eq!(empty.columns.iter().map(|c| (c.name.as_str(),c.data_type.as_str())).collect::<Vec<_>>(), vec![("id","bigint"),("title","text")]);
    let cached = materialize(&db, "SELECT id, title FROM books ORDER BY id", &AuthContext::authenticated("alice"), "cached").await.unwrap();
    assert_eq!(cached.rows.len(), 2);
    assert!(!cached.rls_enabled);
    db.catalog.insert("cached".into(), cached);
    let rows = execute(&db, "SELECT COUNT(*) AS n FROM cached", &AuthContext::default()).await.unwrap();
    assert_eq!(rows, vec![json!({"n":2})]); // root must keep this temporary DB/cache scoped to Alice.
}

#[tokio::test]
async fn cyclic_and_uncached_materialized_views_fail_closed() {
    let mut db = fixture();
    db.views.insert("looped".into(), View { sql:"SELECT * FROM looped".into(), materialized:false });
    assert_eq!(execute(&db, "SELECT * FROM looped", &AuthContext::admin()).await.unwrap_err().code, "UnsupportedFeature");
    db.views.insert("cached".into(), View { sql:"SELECT * FROM books".into(), materialized:true });
    assert_eq!(execute(&db, "SELECT * FROM cached", &AuthContext::admin()).await.unwrap_err().code, "UnsupportedFeature");
}

#[tokio::test]
async fn immutable_snapshot_and_policy_errors_are_preserved() {
    let mut db = fixture(); enable_rls(&mut db);
    let before = db.clone();
    execute(&db, "SELECT * FROM books ORDER BY id", &AuthContext::authenticated("alice")).await.unwrap();
    assert_eq!(db, before);
    db.catalog.get_mut("books").unwrap().policies[0].using = Some(sqlparser::ast::Expr::Identifier(sqlparser::ast::Ident::new("missing_policy_column")));
    assert_eq!(execute(&db, "SELECT * FROM books", &AuthContext::authenticated("alice")).await.unwrap_err().code, "UndefinedColumn");
}

#[tokio::test]
async fn quoted_identifiers_and_unicode_are_not_case_folded() {
    let mut db = Database::default();
    db.execute_sql("CREATE TABLE \"書籍\" (\"Title\" text); INSERT INTO \"書籍\" VALUES ('東京');", &AuthContext::admin()).unwrap();
    assert_eq!(execute(&db, "SELECT \"Title\" FROM \"書籍\"", &AuthContext::admin()).await.unwrap(), vec![json!({"Title":"東京"})]);
}
