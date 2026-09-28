//! Shared test fixtures.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::{json, Value};
use typiql_core::{FieldFilter, FilterOp, Location, TypiQLAdapter};

/// Collections of JSON rows; equality filters only (all this test needs).
#[derive(Default)]
pub struct MemAdapter {
    pub tables: Mutex<HashMap<String, Vec<Value>>>,
    next_id: Mutex<u32>,
}

fn table(location: &Location) -> String {
    match location {
        Location::Named(t) => t.clone(),
        other => panic!("unexpected location {other:?}"),
    }
}

fn matches(row: &Value, filters: &[FieldFilter]) -> bool {
    filters.iter().all(|f| {
        assert!(matches!(f.op, FilterOp::Eq), "MemAdapter only does Eq");
        row.get(&f.field) == Some(&f.value)
    })
}

#[async_trait]
impl TypiQLAdapter for MemAdapter {
    async fn get_one(&self, l: Location, key: &str, value: &str) -> Option<Value> {
        let tables = self.tables.lock().unwrap();
        tables.get(&table(&l))?.iter().find(|r| r[key] == value).cloned()
    }
    async fn get_many(&self, l: Location, filters: Vec<FieldFilter>) -> Vec<Value> {
        let tables = self.tables.lock().unwrap();
        let rows = tables.get(&table(&l)).cloned().unwrap_or_default();
        rows.into_iter().filter(|r| matches(r, &filters)).collect()
    }
    async fn get_many_matching_any(&self, l: Location, groups: Vec<Vec<FieldFilter>>) -> Vec<Value> {
        let tables = self.tables.lock().unwrap();
        let rows = tables.get(&table(&l)).cloned().unwrap_or_default();
        rows.into_iter().filter(|r| groups.iter().any(|g| matches(r, g))).collect()
    }
    async fn add(&self, l: Location, key: &str, mut values: Value) -> Result<Value, String> {
        if values.get(key).is_none() {
            let mut next = self.next_id.lock().unwrap();
            *next += 1;
            values[key] = json!(format!("id-{next}"));
        }
        self.tables.lock().unwrap().entry(table(&l)).or_default().push(values.clone());
        Ok(values)
    }
    async fn update(&self, l: Location, key: &str, value: &str, patch: Value) -> Option<Value> {
        let mut tables = self.tables.lock().unwrap();
        let row = tables.get_mut(&table(&l))?.iter_mut().find(|r| r[key] == value)?;
        for (k, v) in patch.as_object()? {
            row[k] = v.clone();
        }
        Some(row.clone())
    }
    async fn remove(&self, l: Location, key: &str, value: &str) -> Option<Value> {
        let mut tables = self.tables.lock().unwrap();
        let rows = tables.get_mut(&table(&l))?;
        let i = rows.iter().position(|r| r[key] == value)?;
        Some(rows.remove(i))
    }
    async fn set_table(&self, l: Location, rows: Vec<Value>) {
        self.tables.lock().unwrap().insert(table(&l), rows);
    }
}
