use kurasql::{AuthContext, Database};
use serde_json::json;

fn database() -> Database {
    let mut db = Database::default();
    db.execute_sql("CREATE TABLE posts (id bigint PRIMARY KEY, title text NOT NULL, score integer DEFAULT 0 CHECK(score >= 0), owner_id text DEFAULT auth.uid(), UNIQUE(title))", &AuthContext::admin()).unwrap();
    db
}

#[test]
fn atomic_crud_and_stable_identity() {
    let mut db = database(); let admin = AuthContext::admin();
    let inserted = db.execute_sql("INSERT INTO posts(id,title) VALUES (1,'hello'),(2,'world') RETURNING *", &admin).unwrap();
    assert_eq!(inserted.affected, 2);
    let table_id = db.table("posts").unwrap().id;
    let row_id = db.table("posts").unwrap().rows.values().next().unwrap().id;
    db.execute_sql("UPDATE posts SET id=10, score=score+3 WHERE id=1", &admin).unwrap();
    assert_eq!(db.table("posts").unwrap().rows[&row_id].version, 2);
    assert_eq!(db.table("posts").unwrap().id, table_id);
    let selected = db.execute_sql("SELECT id,title FROM posts WHERE score>0 ORDER BY id DESC LIMIT 1", &admin).unwrap();
    assert_eq!(selected.rows, vec![json!({"id":10,"title":"hello"})]);
    db.execute_sql("DELETE FROM posts WHERE id=2", &admin).unwrap();
    assert_eq!(db.read_table("posts", &admin).unwrap().len(), 1);
    let before = db.clone();
    assert!(db.execute_sql("UPDATE posts SET title='changed'; INSERT INTO posts(id,title) VALUES(10,'duplicate')", &admin).is_err());
    assert_eq!(db, before);
}

#[test]
fn migration_rolls_back_schema_and_history() {
    let mut db = Database::default(); let admin = AuthContext::admin();
    assert!(db.apply_migration("001", "CREATE TABLE a(id int PRIMARY KEY); INSERT INTO a VALUES(1),(1)", &admin).is_err());
    assert_eq!(db, Database::default());
    db.apply_migration("001", "CREATE TABLE a(id int PRIMARY KEY)", &admin).unwrap();
    db.apply_migration("001", "THIS IS INVALID SQL", &admin).unwrap();
    assert_eq!(db.migrations.len(), 1);
}

#[test]
fn null_three_valued_logic_and_check_unknown() {
    let mut db = database(); let admin = AuthContext::admin();
    db.execute_sql("INSERT INTO posts(id,title,score) VALUES (1,'null',NULL)", &admin).unwrap();
    assert!(db.execute_sql("SELECT * FROM posts WHERE score = NULL", &admin).unwrap().rows.is_empty());
    let result = db.execute_sql("SELECT NULL AND FALSE AS a, NULL OR TRUE AS b, NOT NULL AS c, 2 NOT IN (1,NULL) AS d", &admin).unwrap();
    assert_eq!(result.rows, vec![json!({"a":false,"b":true,"c":null,"d":null})]);
    assert!(db.execute_sql("INSERT INTO posts(id,title,score) VALUES(2,'bad',-1)", &admin).is_err());
}

#[test]
fn constraints_are_atomic_and_strongly_typed() {
    let mut db = database(); let admin = AuthContext::admin();
    db.execute_sql("INSERT INTO posts(id,title) VALUES(1,'first')", &admin).unwrap();
    for sql in ["INSERT INTO posts(id,title) VALUES(2,'first')", "INSERT INTO posts(id,title) VALUES(2,NULL)", "INSERT INTO posts(id,title) VALUES('2','second')", "UPDATE posts SET score=2147483648", "SELECT * FROM posts WHERE id='1'"] {
        let before = db.clone(); assert!(db.execute_sql(sql, &admin).is_err(), "{sql}"); assert_eq!(db, before);
    }
    db.execute_sql("CREATE TABLE replies(id bigint PRIMARY KEY, post_id bigint REFERENCES posts(id))", &admin).unwrap();
    assert!(db.execute_sql("INSERT INTO replies VALUES(1,99)", &admin).is_err());
    db.execute_sql("INSERT INTO replies VALUES(1,1),(2,NULL)", &admin).unwrap();
    assert!(db.execute_sql("DELETE FROM posts", &admin).is_err());
    assert_eq!(db.table("posts").unwrap().rows.len(), 1);
}

#[test]
fn rls_default_deny_policy_composition_and_context_defaults() {
    let mut db = database(); let admin = AuthContext::admin();
    db.execute_sql("ALTER TABLE posts ENABLE ROW LEVEL SECURITY", &admin).unwrap();
    assert!(db.execute_sql("INSERT INTO posts(id,title) VALUES(1,'denied')", &AuthContext::default()).is_err());
    db.execute_sql("CREATE POLICY owners ON posts FOR ALL TO authenticated USING(owner_id = auth.uid()) WITH CHECK(owner_id = auth.uid()); CREATE POLICY positive ON posts AS RESTRICTIVE FOR ALL TO authenticated USING(score < 10) WITH CHECK(score < 10)", &admin).unwrap();
    let alice = AuthContext::authenticated("alice"); let bob = AuthContext::authenticated("bob");
    db.execute_sql("INSERT INTO posts(id,title) VALUES(1,'alice')", &alice).unwrap();
    db.execute_sql("INSERT INTO posts(id,title) VALUES(2,'bob')", &bob).unwrap();
    assert_eq!(db.read_table("posts", &alice).unwrap().len(), 1);
    assert_eq!(db.read_table("posts", &bob).unwrap()[0]["owner_id"], "bob");
    assert_eq!(db.execute_sql("UPDATE posts SET title='stolen' WHERE id=1", &bob).unwrap().affected, 0);
    let before = db.clone();
    assert!(db.execute_sql("UPDATE posts SET score=10 WHERE id=1", &alice).is_err());
    assert_eq!(db, before);
    assert!(db.execute_sql("ALTER TABLE posts DISABLE ROW LEVEL SECURITY", &alice).is_err());
}

#[test]
fn upsert_updates_retains_identity_and_handles_duplicates() {
    let mut db = database(); let admin = AuthContext::admin();
    db.execute_sql("INSERT INTO posts(id,title) VALUES(1,'first')", &admin).unwrap();
    let id = db.table("posts").unwrap().rows.values().next().unwrap().id;
    let result = db.execute_sql("INSERT INTO posts(id,title) VALUES(1,'second') ON CONFLICT(id) DO UPDATE SET title=EXCLUDED.title RETURNING id,title", &admin).unwrap();
    assert_eq!(result.rows, vec![json!({"id":1,"title":"second"})]);
    assert_eq!(db.table("posts").unwrap().rows[&id].version, 2);
    assert_eq!(db.execute_sql("INSERT INTO posts(id,title) VALUES(1,'ignored') ON CONFLICT(id) DO NOTHING", &admin).unwrap().affected, 0);
    let before = db.clone();
    assert!(db.execute_sql("INSERT INTO posts(id,title) VALUES(1,'a'),(1,'b') ON CONFLICT(id) DO UPDATE SET title=EXCLUDED.title", &admin).is_err());
    assert_eq!(db, before);
}

#[test]
fn serialization_and_deterministic_catalog() {
    let a = database(); let b = database();
    assert_eq!(serde_json::to_vec(&a).unwrap(), serde_json::to_vec(&b).unwrap());
    assert_eq!(serde_json::from_slice::<Database>(&serde_json::to_vec(&a).unwrap()).unwrap(), a);
}

#[test]
fn lexer_preserves_semicolons_in_values_and_ignores_comments() {
    let mut db = database(); let admin = AuthContext::admin();
    db.execute_sql("-- ignored ;\n INSERT INTO posts(id,title) VALUES(1,'a; b''c'); /* ; */ SELECT * FROM posts", &admin).unwrap();
    assert_eq!(db.read_table("posts", &admin).unwrap()[0]["title"], "a; b'c");
}

#[test]
fn unsupported_types_and_statements_are_explicit() {
    let mut db = database();
    for sql in ["CREATE TABLE nums (v decimal(70,2))", "CREATE TABLE temps(v timestamp)", "ALTER TABLE posts ALTER COLUMN id TYPE text", "SELECT count(*) FROM posts"] {
        assert_eq!(db.execute_sql(sql, &AuthContext::admin()).unwrap_err().code, "UnsupportedFeature");
    }
}

#[test]
fn like_unicode_wildcards_and_escape() {
    let mut db = database(); let admin = AuthContext::admin();
    db.execute_sql("INSERT INTO posts(id,title) VALUES(1,'Hello世界'),(2,'100%')", &admin).unwrap();
    assert_eq!(db.execute_sql("SELECT id FROM posts WHERE title ILIKE 'hello__'", &admin).unwrap().rows, vec![json!({"id":1})]);
    assert_eq!(db.execute_sql("SELECT id FROM posts WHERE title LIKE '100!%' ESCAPE '!'", &admin).unwrap().rows, vec![json!({"id":2})]);
    assert_eq!(db.execute_sql("DELETE FROM posts WHERE title LIKE '%世界'", &admin).unwrap().affected, 1);
}

#[test]
fn explicit_values_do_not_evaluate_their_defaults() {
    let mut db = Database::default(); let admin = AuthContext::admin();
    db.execute_sql("CREATE TABLE defaults (id int PRIMARY KEY, value int DEFAULT 1 / 0)", &admin).unwrap();
    db.execute_sql("INSERT INTO defaults(id,value) VALUES(1,42)", &admin).unwrap();
    let before = db.clone();
    assert!(db.execute_sql("INSERT INTO defaults(id) VALUES(2)", &admin).is_err());
    assert_eq!(before,db);
}
