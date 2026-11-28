//! `#[typiql_type(ops(...))]` — a declared CRUD surface generates only the
//! declared operations and, for `get`/`list`, only the declared filters.

// The macros emit `::typiql::...` paths, as seen from a consuming crate.
extern crate typiql_core as typiql;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{json, Value};
use typiql::{AdapterMap, FieldFilter, FilterOp, Location, TypiQLAdapter};

/// Returns no rows; records every `get_many` filter set and echoes `add`.
#[derive(Default)]
struct RecordingAdapter {
    seen: Mutex<Vec<Vec<FieldFilter>>>,
}

#[async_trait]
impl TypiQLAdapter for RecordingAdapter {
    async fn get_one(&self, _: Location, _: &str, _: &str) -> Option<Value> {
        None
    }
    async fn get_many(&self, _: Location, filters: Vec<FieldFilter>) -> Vec<Value> {
        self.seen.lock().unwrap().push(filters);
        Vec::new()
    }
    async fn get_many_matching_any(&self, _: Location, _: Vec<Vec<FieldFilter>>) -> Vec<Value> {
        Vec::new()
    }
    async fn add(&self, _: Location, key_field: &str, mut values: Value) -> Result<Value, String> {
        values[key_field] = json!("new-id");
        Ok(values)
    }
    async fn update(&self, _: Location, _: &str, _: &str, _: Value) -> Option<Value> {
        None
    }
    async fn remove(&self, _: Location, _: &str, _: &str) -> Option<Value> {
        None
    }
    async fn set_table(&self, _: Location, _: Vec<Value>) {}
}

fn adapters(adapter: Arc<RecordingAdapter>) -> AdapterMap {
    let mut map: AdapterMap = HashMap::new();
    map.insert("default", adapter as Arc<dyn TypiQLAdapter>);
    map
}

/// Append-only: add and list, list filterable by `source` only.
#[typiql::typiql_type(ops(add, list(filters(source))))]
pub struct LogEntry {
    #[typiql(key)]
    pub id: String,
    pub source: String,
    pub message: String,
}

/// Read-only: a get with no filter arguments, and a list with all of them.
#[typiql::typiql_type(ops(get(filters()), list))]
pub struct Setting {
    #[typiql(key)]
    pub id: String,
    pub value: Option<i32>,
}

/// No `ops(...)`: the full surface, as before.
#[typiql::typiql_type]
pub struct Widget {
    #[typiql(key)]
    pub id: String,
    pub name: String,
}

mod schema {
    use super::*;
    typiql::typiql_schema!(LogEntry, Setting, Widget);
}

/// Every operation left out — contributes nothing to any root.
#[typiql::typiql_type(ops())]
pub struct Hidden {
    #[typiql(key)]
    pub id: String,
    pub secret: String,
}

mod hidden_schema {
    use super::*;
    typiql::typiql_schema!(Hidden, Widget);
}

/// Field names on `type Query`/`Mutation`/`Subscription`, with their args.
fn root_fields(sdl: &str, root: &str) -> Vec<String> {
    let start = sdl
        .find(&format!("type TypiQL{root} {{"))
        .unwrap_or_else(|| panic!("no {root} in:\n{sdl}"));
    let body = &sdl[start..];
    let body = &body[body.find('{').unwrap() + 1..body.find("\n}").unwrap()];
    body.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect()
}

fn field<'a>(fields: &'a [String], name: &str) -> Option<&'a String> {
    fields
        .iter()
        .find(|f| f.starts_with(&format!("{name}(")) || f.starts_with(&format!("{name}:")))
}

#[tokio::test]
async fn only_declared_operations_are_generated() {
    let sdl = schema::build_typiql_schema(adapters(Default::default())).sdl();
    let queries = root_fields(&sdl, "Query");
    let mutations = root_fields(&sdl, "Mutation");
    let subscriptions = root_fields(&sdl, "Subscription");

    // LogEntry: add + list only.
    assert!(field(&mutations, "addLogEntry").is_some());
    assert!(field(&queries, "getLogEntries").is_some());
    assert!(field(&queries, "getLogEntry").is_none());
    assert!(field(&mutations, "updateLogEntry").is_none());
    assert!(field(&mutations, "removeLogEntry").is_none());
    assert!(field(&subscriptions, "logEntryChanged").is_none());

    // Setting: get + list, no mutations or subscription.
    assert!(field(&queries, "getSetting").is_some());
    assert!(field(&queries, "getSettings").is_some());
    assert!(!mutations.iter().any(|m| m.contains("Setting")));
    assert!(field(&subscriptions, "settingChanged").is_none());

    // Widget: undeclared, so everything.
    for m in ["addWidget", "updateWidget", "removeWidget"] {
        assert!(field(&mutations, m).is_some(), "{m} missing");
    }
    for q in ["getWidget", "getWidgets"] {
        assert!(field(&queries, q).is_some(), "{q} missing");
    }
    assert!(field(&subscriptions, "widgetChanged").is_some());
    // …but never the bulk operations, which are opt-in only.
    for m in ["addWidgets", "updateWidgets", "removeWidgets"] {
        assert!(
            field(&mutations, m).is_none(),
            "{m} generated without being declared"
        );
    }
}

#[tokio::test]
async fn declared_filters_narrow_the_arguments() {
    let sdl = schema::build_typiql_schema(adapters(Default::default())).sdl();
    let queries = root_fields(&sdl, "Query");

    // `filters(source)`: every operator for `source`, nothing for the rest.
    let list = field(&queries, "getLogEntries").unwrap();
    for arg in [
        "source:",
        "source_not:",
        "source_in:",
        "source_startsWith:",
        "source_notContains:",
    ] {
        assert!(list.contains(arg), "{arg} missing from {list}");
    }
    assert!(!list.contains("message"), "{list}");
    assert!(!list.contains("id"), "{list}");

    // `filters()`: no filter arguments — only sorting/paging remain.
    assert_eq!(
        field(&queries, "getSetting").unwrap(),
        "getSetting(_orderBy: String, _orderBy_desc: String, _start: Int, _limit: Int): Setting"
    );

    // Blank: every field, every operator.
    let settings = field(&queries, "getSettings").unwrap();
    for arg in ["id:", "value:", "value_gt:", "value_notIn:"] {
        assert!(settings.contains(arg), "{arg} missing from {settings}");
    }
}

#[tokio::test]
async fn a_type_with_no_operations_contributes_nothing() {
    let sdl = hidden_schema::build_typiql_schema(adapters(Default::default())).sdl();
    assert!(!sdl.contains("Hidden"), "{sdl}");
    assert!(field(&root_fields(&sdl, "Query"), "getWidgets").is_some());
}

#[tokio::test]
async fn declared_operations_still_resolve() {
    let adapter = Arc::new(RecordingAdapter::default());
    let schema = schema::build_typiql_schema(adapters(adapter.clone()));

    let res = schema
        .execute(
            r#"mutation { addLogEntry(values: { source: "sim", message: "hi" }) { id source } }"#,
        )
        .await;
    assert!(res.errors.is_empty(), "{:?}", res.errors);
    assert_eq!(
        res.data.into_json().unwrap(),
        json!({ "addLogEntry": { "id": "new-id", "source": "sim" } })
    );

    let res = schema
        .execute(r#"{ getLogEntries(source_startsWith: "si") { id } }"#)
        .await;
    assert!(res.errors.is_empty(), "{:?}", res.errors);
    {
        let seen = adapter.seen.lock().unwrap();
        let filters = seen.last().unwrap();
        assert_eq!(filters.len(), 1);
        assert_eq!(filters[0].field, "source");
        assert!(matches!(filters[0].op, FilterOp::StartsWith));
    }

    // An undeclared operation or filter is a schema validation error, not a
    // runtime refusal.
    let res = schema
        .execute(r#"mutation { removeLogEntry(id: "x") { id } }"#)
        .await;
    assert!(!res.errors.is_empty());
    let res = schema
        .execute(r#"{ getLogEntries(message: "hi") { id } }"#)
        .await;
    assert!(!res.errors.is_empty());
}
