use kurasql::{AuthContext,Database};

#[test]
fn deferred_fk_validates_final_atomic_state() {
    let mut db = Database::default(); let admin = AuthContext::admin();
    db.execute_sql("CREATE TABLE parent(id int PRIMARY KEY); CREATE TABLE child(id int PRIMARY KEY,parent_id int REFERENCES parent(id) DEFERRABLE INITIALLY DEFERRED)",&admin).unwrap();
    assert!(db.catalog["child"].foreign_keys[0].deferred);
    db.execute_sql("INSERT INTO child VALUES(1,1); INSERT INTO parent VALUES(1)",&admin).unwrap();
    let before = db.clone();
    assert!(db.execute_sql("INSERT INTO child VALUES(2,2)",&admin).is_err());
    assert_eq!(before,db);
    db.execute_sql("DELETE FROM parent; DELETE FROM child",&admin).unwrap();
}

#[test]
fn deferred_unique_supports_atomic_key_swap() {
    let mut db = Database::default(); let admin = AuthContext::admin();
    db.execute_sql("CREATE TABLE items(id int PRIMARY KEY, label text, UNIQUE(label) DEFERRABLE INITIALLY DEFERRED); INSERT INTO items VALUES(1,'a'),(2,'b')",&admin).unwrap();
    db.execute_sql("UPDATE items SET label='b' WHERE id=1; UPDATE items SET label='a' WHERE id=2",&admin).unwrap();
    let before = db.clone();
    assert!(db.execute_sql("UPDATE items SET label='a' WHERE id=1",&admin).is_err());
    assert_eq!(before,db);
}

#[test]
fn immediate_constraints_do_not_defer_implicitly() {
    let mut db = Database::default(); let admin = AuthContext::admin();
    db.execute_sql("CREATE TABLE parent(id int PRIMARY KEY); CREATE TABLE child(id int PRIMARY KEY,parent_id int REFERENCES parent(id) DEFERRABLE INITIALLY IMMEDIATE)",&admin).unwrap();
    let before = db.clone();
    assert!(db.execute_sql("INSERT INTO child VALUES(1,1); INSERT INTO parent VALUES(1)",&admin).is_err());
    assert_eq!(before,db);
}
