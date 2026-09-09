use alloy::primitives::{B256, U256};
use anyhow::{bail, Result};
use kura_chain::{id, key, operation, ChainSnapshot, Operation};
use kurasql::{Database, Row};
use std::collections::BTreeMap;
use serde::{Deserialize,Serialize};

const CATALOG_KEY: &str = "kurasql.catalog.v1";

/// Deterministic UTF-8 JSON envelope: field order is fixed by this struct and
/// nested object keys use serde_json's sorted map. Type metadata distinguishes
/// integer/decimal/float, JSON/text, temporal values and NULL unambiguously.
#[derive(Serialize,Deserialize)]
struct CanonicalCell { version:u8, sql_type:String, value:serde_json::Value }
fn encode(column:&kurasql::Column,value:&serde_json::Value)->Result<Vec<u8>> {
    Ok(serde_json::to_vec(&CanonicalCell{version:1,sql_type:column.data_type.clone(),value:value.clone()})?)
}
fn decode(column:&kurasql::Column,bytes:&[u8])->Result<serde_json::Value> {
    let cell:CanonicalCell=serde_json::from_slice(bytes)?;
    if cell.version!=1 || cell.sql_type!=column.data_type {bail!("Unsupported canonical cell version or mismatched SQL type");}
    Ok(cell.value)
}

/// Catalog contains definitions only; every row is reconstructed from canonical cell events.
fn metadata(db: &Database) -> Database {
    let mut catalog = db.clone();
    for table in catalog.catalog.values_mut() {
        table.rows.clear();
        table.next_row_id = 1;
    }
    catalog
}

fn number(value: B256) -> Result<u64> { Ok(U256::from_be_bytes(value.0).try_into()?) }

pub fn rebuild(snapshot: &ChainSnapshot) -> Result<Database> {
    let mut db = Database::default();
    let mut rows: BTreeMap<(u64, u64), (u64, BTreeMap<B256, Vec<u8>>)> = BTreeMap::new();
    let mut allocators: BTreeMap<u64, u64> = BTreeMap::new();
    for event in &snapshot.changes {
        match event.kind {
            0 if event.column == key(CATALOG_KEY) => db = serde_json::from_slice(&event.data)?,
            2 => {
                let table = number(event.table)?; let row = number(event.row)?;
                allocators.entry(table).and_modify(|n| *n = (*n).max(row + 1)).or_insert(row + 1);
                rows.insert((table, row), (event.revision, BTreeMap::new()));
            }
            3 => { if let Some((version, cells)) = rows.get_mut(&(number(event.table)?, number(event.row)?)) { *version = event.revision; cells.insert(event.column, event.data.clone()); } else { bail!("Cell event has no row"); } }
            4 => { if let Some((version, cells)) = rows.get_mut(&(number(event.table)?, number(event.row)?)) { *version = event.revision; cells.remove(&event.column); } }
            5 => { rows.remove(&(number(event.table)?, number(event.row)?)); }
            _ => {}
        }
    }
    for table in db.catalog.values_mut() {
        table.rows.clear();
        table.next_row_id = allocators.get(&table.id).copied().unwrap_or(1);
        for ((table_id, row_id), (version, cells)) in &rows {
            if *table_id != table.id { continue; }
            let mut values = BTreeMap::new();
            for column in &table.columns {
                let value = cells.get(&id(column.id)).map(|bytes| decode(column,bytes)).transpose()?.unwrap_or(serde_json::Value::Null);
                values.insert(column.name.clone(), value);
            }
            table.rows.insert(*row_id, Row { id: *row_id, version: *version, values });
        }
    }
    Ok(db)
}

pub fn plan(before: &Database, after: &Database) -> Result<Vec<Operation>> {
    let mut ops = Vec::new();
    if metadata(before) != metadata(after) {
        ops.push(operation(0, B256::ZERO, B256::ZERO, key(CATALOG_KEY), serde_json::to_vec(&metadata(after))?));
    }
    for old_table in before.catalog.values() {
        let new = after.catalog.values().find(|t| t.id == old_table.id);
        for row in old_table.rows.values() {
            if new.is_none_or(|t| !t.rows.contains_key(&row.id)) {
                ops.push(operation(5, id(old_table.id), id(row.id), B256::ZERO, vec![]));
            }
        }
    }
    for table in after.catalog.values() {
        let old_table = before.catalog.values().find(|t| t.id == table.id);
        for row in table.rows.values() {
            let old = old_table.and_then(|t| t.rows.get(&row.id));
            if old.is_none() { ops.push(operation(2, id(table.id), id(row.id), B256::ZERO, vec![])); }
            for column in &table.columns {
                let value = row.values.get(&column.name).unwrap_or(&serde_json::Value::Null);
                let previous = old.and_then(|r| old_table.and_then(|t| t.columns.iter().find(|c| c.id == column.id && c.data_type==column.data_type)).and_then(|c| r.values.get(&c.name)));
                if previous != Some(value) {
                    ops.push(operation(3, id(table.id), id(row.id), id(column.id), encode(column,value)?));
                }
            }
        }
    }
    for version in after.migrations.difference(&before.migrations) {
        ops.push(operation(6, B256::ZERO, B256::ZERO, key(version), version.as_bytes().to_vec()));
    }
    Ok(ops)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kurasql::AuthContext;
    #[test]
    fn cell_replay_preserves_data_and_deleted_allocator() {
        let mut db = Database::default();
        db.execute_sql("CREATE TABLE posts(id integer PRIMARY KEY, title text); INSERT INTO posts VALUES (1, 'hello');", &AuthContext::admin()).unwrap();
        let mut events = Vec::new();
        for (revision, (before, after)) in [(1, (Database::default(), db.clone())), (2, (db.clone(), { let mut d = db.clone(); d.execute_sql("DELETE FROM posts", &AuthContext::admin()).unwrap(); d }))] {
            for op in plan(&before, &after).unwrap() { events.push(kura_chain::Change { revision, kind:op.kind, table:op.tableId, row:op.rowId, column:op.columnId, data:op.data.to_vec() }); }
        }
        let restored = rebuild(&ChainSnapshot { block_number:2, block_hash:B256::ZERO, revision:2, changes:events }).unwrap();
        assert!(restored.catalog["posts"].rows.is_empty());
        assert_eq!(restored.catalog["posts"].next_row_id, 2);
    }
}
