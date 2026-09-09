use kurasql::{AuthContext,Database};
use serde_json::json;

#[test]
fn stable_column_ids_across_atomic_schema_evolution() {
    let mut db = Database::default(); let admin = AuthContext::admin();
    db.execute_sql("CREATE TABLE posts(id bigint PRIMARY KEY,title text NOT NULL); INSERT INTO posts VALUES(1,'hello')",&admin).unwrap();
    let table_id = db.catalog["posts"].id; let column_id = db.catalog["posts"].columns[1].id;
    db.execute_sql("ALTER TABLE posts RENAME COLUMN title TO body; ALTER TABLE posts ADD COLUMN score int DEFAULT 3 NOT NULL",&admin).unwrap();
    assert_eq!(db.catalog["posts"].id,table_id); assert_eq!(db.catalog["posts"].columns[1].id,column_id);
    assert_eq!(db.read_table("posts",&admin).unwrap(),vec![json!({"id":1,"body":"hello","score":3})]);
    let score_id = db.catalog["posts"].columns[2].id;
    db.execute_sql("ALTER TABLE posts DROP COLUMN score; ALTER TABLE posts ADD COLUMN other text",&admin).unwrap();
    assert!(db.catalog["posts"].columns[2].id > score_id);
    let before = db.clone();
    assert!(db.execute_sql("ALTER TABLE posts ADD COLUMN failing text NOT NULL",&admin).is_err());
    assert_eq!(db,before);
}

#[test]
fn dependency_restrict_and_cascade_are_atomic() {
    let mut db = Database::default(); let admin = AuthContext::admin();
    db.execute_sql("CREATE TABLE parent(id bigint PRIMARY KEY); CREATE TABLE child(id bigint PRIMARY KEY,parent_id bigint REFERENCES parent(id)); CREATE VIEW parents AS SELECT id FROM parent; CREATE VIEW all_parents AS SELECT id FROM parents",&admin).unwrap();
    let before = db.clone(); assert!(db.execute_sql("DROP TABLE parent",&admin).is_err()); assert_eq!(db,before);
    db.execute_sql("DROP TABLE parent CASCADE",&admin).unwrap();
    assert!(db.views.is_empty()); assert!(db.catalog["child"].foreign_keys.is_empty());
}

#[test]
fn rename_rebinds_local_fk_and_check_expressions() {
    let mut db = Database::default(); let admin = AuthContext::admin();
    db.execute_sql("CREATE TABLE parent(id bigint PRIMARY KEY CHECK(id>0)); CREATE TABLE child(id bigint PRIMARY KEY,parent_id bigint REFERENCES parent(id)); INSERT INTO parent VALUES(1); INSERT INTO child VALUES(1,1); ALTER TABLE parent RENAME COLUMN id TO identifier; ALTER TABLE parent RENAME TO renamed",&admin).unwrap();
    assert_eq!(db.catalog["child"].foreign_keys[0].foreign_table,"renamed");
    assert_eq!(db.catalog["child"].foreign_keys[0].referred_columns,vec!["identifier"]);
    assert!(db.execute_sql("INSERT INTO renamed VALUES(-1)",&admin).is_err());
    assert!(db.execute_sql("DELETE FROM renamed",&admin).is_err());
}

#[test]
fn cascading_fk_update_delete_and_set_null() {
    let mut db = Database::default(); let admin = AuthContext::admin();
    db.execute_sql("CREATE TABLE parent(id bigint PRIMARY KEY); CREATE TABLE child(id bigint PRIMARY KEY,parent_id bigint REFERENCES parent(id) ON UPDATE CASCADE ON DELETE CASCADE); CREATE TABLE weak(id bigint PRIMARY KEY,parent_id bigint REFERENCES parent(id) ON UPDATE CASCADE ON DELETE SET NULL); INSERT INTO parent VALUES(1); INSERT INTO child VALUES(1,1); INSERT INTO weak VALUES(1,1)",&admin).unwrap();
    db.execute_sql("UPDATE parent SET id=2 WHERE id=1",&admin).unwrap();
    assert_eq!(db.read_table("child",&admin).unwrap()[0]["parent_id"],2);
    assert_eq!(db.read_table("weak",&admin).unwrap()[0]["parent_id"],2);
    db.execute_sql("DELETE FROM parent",&admin).unwrap();
    assert!(db.catalog["child"].rows.is_empty());
    assert_eq!(db.read_table("weak",&admin).unwrap()[0]["parent_id"],json!(null));
}

#[test]
fn invalid_cascade_rolls_back_every_table() {
    let mut db = Database::default(); let admin = AuthContext::admin();
    db.execute_sql("CREATE TABLE parent(id int PRIMARY KEY); CREATE TABLE child(id int PRIMARY KEY,parent_id int NOT NULL REFERENCES parent(id) ON DELETE SET NULL); INSERT INTO parent VALUES(1); INSERT INTO child VALUES(1,1)",&admin).unwrap();
    let before = db.clone(); assert!(db.execute_sql("DELETE FROM parent",&admin).is_err()); assert_eq!(db,before);
}

#[test]
fn identity_is_replayable_and_generated_columns_recompute() {
    let mut db = Database::default(); let admin = AuthContext::admin();
    db.execute_sql("CREATE TABLE calculations(id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, base int NOT NULL, doubled int GENERATED ALWAYS AS (base * 2) STORED); INSERT INTO calculations(base) VALUES(3),(4)",&admin).unwrap();
    assert_eq!(db.read_table("calculations",&admin).unwrap(),vec![json!({"id":1,"base":3,"doubled":6}),json!({"id":2,"base":4,"doubled":8})]);
    db.execute_sql("UPDATE calculations SET base=5 WHERE id=1",&admin).unwrap();
    assert_eq!(db.read_table("calculations",&admin).unwrap()[0]["doubled"],10);
    let before = db.clone();
    assert!(db.execute_sql("UPDATE calculations SET doubled=99",&admin).is_err());
    assert!(db.execute_sql("INSERT INTO calculations(id,base) VALUES(99,5)",&admin).is_err());
    assert_eq!(db,before);
    db.execute_sql("DELETE FROM calculations WHERE id=2; INSERT INTO calculations(base) VALUES(6)",&admin).unwrap();
    assert_eq!(db.read_table("calculations",&admin).unwrap()[1]["id"],3);
    let mut rebuilt: Database = serde_json::from_value(serde_json::to_value(&db).unwrap()).unwrap();
    rebuilt.execute_sql("INSERT INTO calculations(base) VALUES(7)",&admin).unwrap();
    assert_eq!(rebuilt.read_table("calculations",&admin).unwrap()[2]["id"],4);
}

#[test]
fn identity_start_increment_and_by_default() {
    let mut db = Database::default(); let admin = AuthContext::admin();
    db.execute_sql("CREATE TABLE ids(id bigint GENERATED BY DEFAULT AS IDENTITY (INCREMENT BY 2 START WITH 10) PRIMARY KEY); INSERT INTO ids VALUES(DEFAULT),(DEFAULT)",&admin).unwrap();
    assert_eq!(db.read_table("ids",&admin).unwrap(),vec![json!({"id":10}),json!({"id":12})]);
    db.execute_sql("INSERT INTO ids VALUES(100)",&admin).unwrap();
    assert!(db.execute_sql("INSERT INTO ids VALUES(NULL)",&admin).is_err());
}
