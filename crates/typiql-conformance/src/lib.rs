//! One GraphQL test suite, run against every adapter.
//!
//! typiql's promise is that GraphQL is the only query language a consumer
//! ever sees: which adapter serves a type is backend configuration, and
//! moving a type from one adapter to another changes nothing a client can
//! observe (performance aside). This crate holds that promise to account.
//! Every case here talks to the schema purely through GraphQL, and each
//! `tests/*.rs` file runs the whole suite against a different adapter setup
//! via [`conformance_suite!`] — one adapter for everything, or a relation
//! crossing from one adapter to another.
//!
//! A new adapter gets checked by adding one `tests/<adapter>.rs` with a
//! [`Store`] constructor; a new feature, by adding a case here and a line to
//! [`conformance_suite!`].

use std::any::Any;
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use serde::Serialize;
use serde_json::{json, Value};
use typiql::{with_user, AdapterMap, TypiQLAdapter, TypiQLUser};
use typiql_adapter_duckdb::DuckDbAdapter;
use typiql_adapter_json::JsonAdapter;

// ── the types under test ────────────────────────────────────────────────────

/// Every scalar kind the filter operators care about, nullable variants of
/// them, nested data, and a has-one relation to a type that may live on a
/// different adapter.
#[typiql::typiql_type]
pub struct Item {
    #[typiql(key)]
    pub id: String,
    pub name: String,
    pub note: Option<String>,
    pub count: i64,
    pub maybe_count: Option<i64>,
    pub price: f64,
    pub active: bool,
    pub tags: Vec<String>,
    pub size: Size,
    pub dims: Option<Dims>,
    /// Filterable element by element (`events_any`, `_first`, `_last`,
    /// `_atIndex`).
    #[typiql(elements)]
    pub events: Vec<Event>,
    pub owner: Option<String>,
    #[typiql(relation(local = "owner", foreign = "id"))]
    pub owner_record: Option<Owner>,
    /// The same relation in join mode (it has a `data_name`): joined into
    /// every item read, so items can sort by `ownerJoined.name`.
    #[typiql(data_name = "ownerDoc", relation(local = "owner", foreign = "id"))]
    pub owner_joined: Option<Box<Owner>>,
}

#[derive(async_graphql::Enum, Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, serde::Deserialize)]
pub enum Size {
    #[default]
    Small,
    Large,
}

/// The element of a filterable list.
#[derive(
    async_graphql::SimpleObject, async_graphql::InputObject, typiql::TypiQLElement,
    Clone, Debug, Serialize, serde::Deserialize,
)]
#[graphql(input_name = "EventInput")]
pub struct Event {
    pub kind: String,
    pub at: i64,
}

/// Nested data with no adapter-native column type.
#[derive(async_graphql::SimpleObject, async_graphql::InputObject, Clone, Debug, Serialize, serde::Deserialize)]
#[graphql(input_name = "DimsInput")]
pub struct Dims {
    pub width: f64,
    pub label: String,
}

/// Served by whatever adapter is registered as `"secondary"` — the same as
/// `"default"` for a single-adapter run, a different one for a mixed run.
/// Renamed storage fields, so relations are checked across `data_name`.
#[typiql::typiql_type(adapter = "secondary")]
pub struct Owner {
    #[typiql(key, data_name = "_id")]
    pub id: String,
    #[typiql(data_name = "DISPLAY_NAME")]
    pub name: String,
    #[typiql(log, relation(local = "id", foreign = "owner"))]
    pub items: Vec<Item>,
    /// A non-equality condition with a literal value.
    #[typiql(relation(local = "id", foreign = "owner"))]
    #[typiql(relation(foreign = "price", op = "gt", value = 2.0))]
    pub expensive_items: Vec<Item>,
    /// Has-many with its own sort and limit (C#'s relation `_orderBy_desc`
    /// and `_limit` arguments).
    #[typiql(relation(local = "id", foreign = "owner", order_by_desc = "count", limit = 1))]
    pub top_item: Vec<Item>,
    /// Has-one picks the first match by its sort… (boxed: `Item` relates
    /// back to `Owner` one-to-one, so one side needs indirection).
    #[typiql(relation(local = "id", foreign = "owner", order_by_desc = "count"))]
    pub latest_item: Option<Box<Item>>,
    /// …after skipping `start`.
    #[typiql(relation(local = "id", foreign = "owner", order_by_desc = "count", start = 1))]
    pub second_item: Option<Box<Item>>,
    /// Has-many in join mode, with its own sort.
    #[typiql(data_name = "itemsDoc", relation(local = "id", foreign = "owner", order_by = "count"))]
    pub items_joined: Vec<Item>,
}

/// Declared surface plus auth: only the owner reads or changes a note.
#[typiql::typiql_type(ops(
    list(name = "myNotes", scope(owner = "@currentUser")),
    add(roles("User"), set(owner = "@currentUser", created = "@now"), log),
    update(roles("User"), scope(owner = "@currentUser")),
    remove(roles("User"), scope(owner = "@currentUser")),
    subscribe(scope(owner = "@currentUser")),
))]
pub struct Note {
    #[typiql(key)]
    pub id: String,
    pub owner: String,
    pub body: String,
    pub created: Option<String>,
}

/// C#'s upsert patterns: "save my settings" found by the scope alone
/// (`updateMyUserData`), an import found by an external id
/// (`uploadClient`), and a plain keyed update, which gains `_upsert`.
#[typiql::typiql_type(ops(
    get(name = "myProfile", scope(username = "@currentUser")),
    list,
    update,
    update(name = "updateMyProfile", by(), scope(username = "@currentUser"), upsert, set(seen = "@now")),
    update(name = "importProfile", by(external_id), upsert),
))]
pub struct Profile {
    #[typiql(key)]
    pub id: String,
    pub username: Option<String>,
    pub external_id: Option<String>,
    pub bio: Option<String>,
    pub visits: i64,
    pub seen: Option<String>,
}

/// Bulk operations (C#'s AddMany / UpdateMany / RemoveMany).
#[typiql::typiql_type(ops(
    list,
    add_many(set(created = "@now")),
    update_many,
    remove_many,
    remove_many(name = "clearMyTasks", filters(), scope(owner = "@currentUser")),
))]
pub struct Task {
    #[typiql(key)]
    pub id: String,
    pub title: String,
    pub done: bool,
    pub owner: Option<String>,
    pub created: Option<String>,
}

/// An integer key (an `INT IDENTITY` table in SQL): adapters number new rows.
#[typiql::typiql_type]
pub struct Counter {
    #[typiql(key)]
    pub id: i64,
    pub label: String,
}

/// An aggregate type: Items grouped by owner.
#[typiql::typiql_type(aggregate = "Item")]
pub struct ItemSummary {
    pub owner: Option<String>,
    #[typiql(aggregate(count))]
    pub items: i64,
    #[typiql(aggregate(count = "note"))]
    pub noted: i64,
    #[typiql(aggregate(sum = "count"))]
    pub total: i64,
    #[typiql(aggregate(avg = "price"))]
    pub avg_price: Option<f64>,
    #[typiql(aggregate(min = "name"))]
    pub first_name: Option<String>,
    #[typiql(aggregate(max = "price"))]
    pub max_price: Option<f64>,
}

/// No group fields: one row over the whole collection.
#[typiql::typiql_type(aggregate = "Item", ops(list(name = "itemTotals")))]
pub struct ItemTotals {
    #[typiql(aggregate(count))]
    pub items: i64,
    #[typiql(aggregate(sum = "count"))]
    pub total: i64,
    #[typiql(aggregate(sum = "price"))]
    pub revenue: f64,
}

/// Over a collection that stays empty.
#[typiql::typiql_type(aggregate = "Task", ops(list(name = "taskTotals")))]
pub struct TaskTotals {
    #[typiql(aggregate(count))]
    pub tasks: i64,
    #[typiql(aggregate(sum = "title"))]
    pub nonsense: i64,
}

/// The log type: including it in the schema turns logging on.
#[typiql::typiql_log(adapter = "default")]
pub struct Log;

pub mod schema {
    use super::*;
    typiql::typiql_schema!(Item, Owner, Note, Profile, Task, Counter, ItemSummary, ItemTotals, TaskTotals, Log);
    pub type Schema = async_graphql::Schema<TypiQLQuery, TypiQLMutation, TypiQLSubscription>;
}

/// The app-side user type the auth cases run as.
#[derive(Serialize, Clone)]
pub struct TestUser {
    pub username: String,
    pub groups: Vec<String>,
}

impl TypiQLUser for TestUser {
    fn name(&self) -> &str {
        &self.username
    }
    fn has_role(&self, role: &str) -> bool {
        self.groups.iter().any(|g| g == role)
    }
}

pub fn user(name: &str) -> TestUser {
    TestUser {
        username: name.into(),
        groups: vec!["User".into()],
    }
}

// ── adapter setups ──────────────────────────────────────────────────────────

/// A fresh, empty adapter setup: the map to build the schema from, plus
/// whatever must stay alive for it (temp directories).
pub struct Store {
    pub adapters: AdapterMap,
    pub keep_alive: Box<dyn Any + Send>,
}

fn json_adapter() -> (Arc<dyn TypiQLAdapter>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    (Arc::new(JsonAdapter::new(dir.path())), dir)
}

fn duckdb_adapter() -> (Arc<dyn TypiQLAdapter>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let adapter = DuckDbAdapter::new(dir.path().join("data.duckdb")).unwrap();
    (Arc::new(adapter), dir)
}

fn store(default: (Arc<dyn TypiQLAdapter>, tempfile::TempDir), secondary: Option<(Arc<dyn TypiQLAdapter>, tempfile::TempDir)>) -> Store {
    let mut adapters: AdapterMap = HashMap::new();
    adapters.insert("default", default.0.clone());
    let secondary_dir = secondary.map(|(adapter, dir)| {
        adapters.insert("secondary", adapter);
        dir
    });
    if secondary_dir.is_none() {
        adapters.insert("secondary", default.0);
    }
    Store {
        adapters,
        keep_alive: Box::new((default.1, secondary_dir)),
    }
}

/// The SQL Server test server, from `TYPIQL_TEST_MSSQL` (an ADO.NET-style
/// connection string without a database). Each store gets its own new
/// database, so tests never see each other's rows.
fn mssql_adapter() -> (Arc<dyn TypiQLAdapter>, Box<dyn Any + Send>) {
    let conn = std::env::var("TYPIQL_TEST_MSSQL").expect("TYPIQL_TEST_MSSQL is set (see conformance_suite!'s `requires`)");
    let database = format!("typiql_test_{}", uuid::Uuid::new_v4().simple());
    let adapter = typiql_adapter_mssql::MssqlAdapter::with_database(&conn, &database, true).unwrap();
    (Arc::new(adapter), Box::new(DropDatabase { conn, database }))
}

/// Drops a test's SQL Server database when its harness goes away, so a run
/// doesn't leave hundreds behind (the test server has little memory).
struct DropDatabase {
    conn: String,
    database: String,
}

impl Drop for DropDatabase {
    fn drop(&mut self) {
        let (conn, database) = (self.conn.clone(), self.database.clone());
        // Drop can't await; do it on a thread with its own small runtime.
        let _ = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            if let Err(e) = rt.block_on(typiql_adapter_mssql::MssqlAdapter::drop_database(&conn, &database)) {
                eprintln!("couldn't drop test database {database}: {e}");
            }
        })
        .join();
    }
}

/// Everything on SQL Server.
pub fn mssql_store() -> Store {
    let (adapter, keep) = mssql_adapter();
    let mut adapters: AdapterMap = HashMap::new();
    adapters.insert("default", adapter.clone());
    adapters.insert("secondary", adapter);
    Store { adapters, keep_alive: keep }
}

/// `Item`/`Note` on SQL Server, `Owner` on JSON.
pub fn mssql_then_json_store() -> Store {
    let (adapter, keep) = mssql_adapter();
    let (json, dir) = json_adapter();
    let mut adapters: AdapterMap = HashMap::new();
    adapters.insert("default", adapter);
    adapters.insert("secondary", json);
    Store { adapters, keep_alive: Box::new((keep, dir)) }
}

fn mongo_adapter() -> (Arc<dyn TypiQLAdapter>, Box<dyn Any + Send>) {
    let uri = std::env::var("TYPIQL_TEST_MONGO").expect("TYPIQL_TEST_MONGO is set (see conformance_suite!'s `requires`)");
    let database = format!("typiql_test_{}", uuid::Uuid::new_v4().simple());
    let adapter = typiql_adapter_mongo::MongoAdapter::new(&uri, &database);
    (Arc::new(adapter), Box::new(DropMongoDatabase { uri, database }))
}

/// Drops a test's Mongo database when its harness goes away.
struct DropMongoDatabase {
    uri: String,
    database: String,
}

impl Drop for DropMongoDatabase {
    fn drop(&mut self) {
        let (uri, database) = (self.uri.clone(), self.database.clone());
        let _ = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            if let Err(e) = rt.block_on(typiql_adapter_mongo::MongoAdapter::drop_database(&uri, &database)) {
                eprintln!("couldn't drop test database {database}: {e}");
            }
        })
        .join();
    }
}

/// Everything on MongoDB.
pub fn mongo_store() -> Store {
    let (adapter, keep) = mongo_adapter();
    let mut adapters: AdapterMap = HashMap::new();
    adapters.insert("default", adapter.clone());
    adapters.insert("secondary", adapter);
    Store { adapters, keep_alive: keep }
}

/// `Item`/`Note` on MongoDB, `Owner` on JSON.
pub fn mongo_then_json_store() -> Store {
    let (adapter, keep) = mongo_adapter();
    let (json, dir) = json_adapter();
    let mut adapters: AdapterMap = HashMap::new();
    adapters.insert("default", adapter);
    adapters.insert("secondary", json);
    Store { adapters, keep_alive: Box::new((keep, dir)) }
}

/// Everything on the JSON adapter.
pub fn json_store() -> Store {
    store(json_adapter(), None)
}

/// Everything on the DuckDB adapter.
pub fn duckdb_store() -> Store {
    store(duckdb_adapter(), None)
}

/// `Item`/`Note` on JSON, `Owner` on DuckDB.
pub fn json_then_duckdb_store() -> Store {
    store(json_adapter(), Some(duckdb_adapter()))
}

/// `Item`/`Note` on DuckDB, `Owner` on JSON.
pub fn duckdb_then_json_store() -> Store {
    store(duckdb_adapter(), Some(json_adapter()))
}

// ── harness ─────────────────────────────────────────────────────────────────

pub struct Harness {
    pub schema: schema::Schema,
    /// For the few guarantees GraphQL can't reach (e.g. keys, which clients
    /// never choose) — checked directly at the adapter.
    pub adapters: AdapterMap,
    _store: Box<dyn Any + Send>,
}

impl Harness {
    pub fn new(store: Store) -> Self {
        Harness {
            schema: schema::build_typiql_schema(store.adapters.clone()),
            adapters: store.adapters,
            _store: store.keep_alive,
        }
    }

    /// A harness over a schema the caller built (e.g. with its own logger).
    pub fn with_schema(schema: schema::Schema, store: Store) -> Self {
        Harness { schema, adapters: store.adapters, _store: store.keep_alive }
    }

    /// Runs a GraphQL operation, anonymously or as `as_user`.
    pub async fn try_run(&self, as_user: Option<&TestUser>, query: &str) -> Result<Value, String> {
        let mut req = async_graphql::Request::new(query);
        if let Some(u) = as_user {
            req = with_user(req, u.clone());
        }
        let res = self.schema.execute(req).await;
        match res.errors.first() {
            Some(e) => Err(e.message.clone()),
            None => Ok(res.data.into_json().unwrap()),
        }
    }

    pub async fn run(&self, query: &str) -> Value {
        self.try_run(None, query)
            .await
            .unwrap_or_else(|e| panic!("{query}\n  -> {e}"))
    }

    pub async fn run_as(&self, as_user: &TestUser, query: &str) -> Value {
        self.try_run(Some(as_user), query)
            .await
            .unwrap_or_else(|e| panic!("{query}\n  -> {e}"))
    }

    /// Adds the standard four items and two owners; returns item ids by name.
    pub async fn seed(&self) -> HashMap<&'static str, String> {
        // Keys are adapter-assigned; read them back from `add`.
        let mut owner_ids = HashMap::new();
        for name in ["Olive", "Otto"] {
            let added = self
                .run(&format!(r#"mutation {{ addOwner(values: {{ name: "{name}" }}) {{ id }} }}"#))
                .await;
            owner_ids.insert(name, added["addOwner"]["id"].as_str().unwrap().to_string());
        }
        let (o1, o2) = (&owner_ids["Olive"], &owner_ids["Otto"]);

        let items = [
            ("A", format!(r#"{{ name: "Alpha", note: "x", count: 1, maybeCount: 10, price: 1.5, active: true, tags: ["red", "blue"], size: LARGE, dims: {{ width: 1.0, label: "a" }}, events: [{{ kind: "in", at: 1 }}, {{ kind: "out", at: 5 }}], owner: "{o1}" }}"#)),
            ("B", format!(r#"{{ name: "Beta", count: 2, price: 2.5, active: false, tags: [], size: SMALL, events: [{{ kind: "in", at: 2 }}], owner: "{o1}" }}"#)),
            ("C", format!(r#"{{ name: "Gamma", note: "y", count: 3, maybeCount: 30, price: 3.5, active: true, tags: ["green"], size: LARGE, events: [{{ kind: "out", at: 3 }}, {{ kind: "in", at: 9 }}], owner: "{o2}" }}"#)),
            ("D", r#"{ name: "alphabet_50%", count: 4, price: 4.5, active: false, tags: [], size: SMALL, events: [] }"#.to_string()),
        ];
        let mut ids = HashMap::new();
        for (label, values) in items {
            let added = self
                .run(&format!("mutation {{ addItem(values: {values}) {{ id }} }}"))
                .await;
            ids.insert(label, added["addItem"]["id"].as_str().unwrap().to_string());
        }
        ids.insert("o1", o1.clone());
        ids.insert("o2", o2.clone());
        ids
    }

    /// The item labels (`"A"`..`"D"`) a `getItems(<args>)` query returns.
    pub async fn items_where(&self, ids: &HashMap<&'static str, String>, args: &str) -> BTreeSet<&'static str> {
        let query = if args.is_empty() {
            "{ getItems { id } }".to_string()
        } else {
            format!("{{ getItems({args}) {{ id }} }}")
        };
        let data = self.run(&query).await;
        data["getItems"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| {
                let id = row["id"].as_str().unwrap();
                *ids.iter()
                    .find(|(_, v)| v.as_str() == id)
                    .map(|(k, _)| k)
                    .unwrap_or_else(|| panic!("unknown id {id}"))
            })
            .collect()
    }
}

impl Harness {
    /// The item labels a `getItems(<args>)` query returns, in order.
    pub async fn items_in_order(&self, ids: &HashMap<&'static str, String>, args: &str) -> Vec<&'static str> {
        let data = self.run(&format!("{{ getItems({args}) {{ id }} }}")).await;
        data["getItems"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| {
                let id = row["id"].as_str().unwrap();
                *ids.iter().find(|(_, v)| v.as_str() == id).map(|(k, _)| k).unwrap()
            })
            .collect()
    }
}

pub fn labels(l: &[&'static str]) -> BTreeSet<&'static str> {
    l.iter().copied().collect()
}

// ── cases ───────────────────────────────────────────────────────────────────

pub mod cases {
    use super::*;

    const ITEM_FIELDS: &str = "id name note count maybeCount price active tags size dims { width label } owner";

    pub async fn add_then_read_back(h: &Harness) {
        let added = h
            .run(&format!(
                r#"mutation {{ addItem(values: {{ name: "Alpha", count: 7, price: 1.25, active: true, tags: ["a", "b"], size: LARGE, dims: {{ width: 2.5, label: "box" }} }}) {{ {ITEM_FIELDS} }} }}"#
            ))
            .await;
        let item = &added["addItem"];
        let id = item["id"].as_str().expect("adapter assigns a key");
        assert!(!id.is_empty());
        let expected = json!({
            "id": id, "name": "Alpha", "note": null, "count": 7, "maybeCount": null,
            "price": 1.25, "active": true, "tags": ["a", "b"], "size": "LARGE",
            "dims": { "width": 2.5, "label": "box" }, "owner": null,
        });
        assert_eq!(item, &expected);

        let got = h.run(&format!(r#"{{ getItem(id: "{id}") {{ {ITEM_FIELDS} }} }}"#)).await;
        assert_eq!(got["getItem"], expected);
        let listed = h.run(&format!("{{ getItems {{ {ITEM_FIELDS} }} }}")).await;
        assert_eq!(listed["getItems"], json!([expected]));
    }

    pub async fn update_patches_only_what_was_sent(h: &Harness) {
        let ids = h.seed().await;
        let a = &ids["A"];
        // Omitted: untouched. Explicit null: cleared. Value: set.
        let updated = h
            .run(&format!(
                r#"mutation {{ updateItem(id: "{a}", update: {{ note: null, count: 11 }}) {{ name note count maybeCount tags }} }}"#
            ))
            .await;
        let expected = json!({ "name": "Alpha", "note": null, "count": 11, "maybeCount": 10, "tags": ["red", "blue"] });
        assert_eq!(updated["updateItem"], expected);
        let reread = h.run(&format!(r#"{{ getItem(id: "{a}") {{ name note count maybeCount tags }} }}"#)).await;
        assert_eq!(reread["getItem"], expected);

        let missing = h.run(r#"mutation { updateItem(id: "nope", update: { count: 1 }) { id } }"#).await;
        assert_eq!(missing["updateItem"], Value::Null);
    }

    pub async fn remove_deletes_one(h: &Harness) {
        let ids = h.seed().await;
        let b = &ids["B"];
        let removed = h.run(&format!(r#"mutation {{ removeItem(id: "{b}") {{ name }} }}"#)).await;
        assert_eq!(removed["removeItem"]["name"], "Beta");
        assert_eq!(h.run(&format!(r#"{{ getItem(id: "{b}") {{ id }} }}"#)).await["getItem"], Value::Null);
        assert_eq!(h.items_where(&ids, "").await, labels(&["A", "C", "D"]));
        let again = h.run(&format!(r#"mutation {{ removeItem(id: "{b}") {{ name }} }}"#)).await;
        assert_eq!(again["removeItem"], Value::Null);
    }

    /// Every operator, on fields that always hold a value — where every
    /// adapter must agree on one obvious answer.
    pub async fn filter_operators(h: &Harness) {
        let ids = h.seed().await;
        let cases: &[(&str, &[&'static str])] = &[
            // String
            (r#"name: "Beta""#, &["B"]),
            (r#"name_not: "Beta""#, &["A", "C", "D"]),
            (r#"name_in: ["Alpha", "Gamma"]"#, &["A", "C"]),
            (r#"name_notIn: ["Alpha", "Gamma"]"#, &["B", "D"]),
            (r#"name_in: []"#, &[]),
            (r#"name_notIn: []"#, &["A", "B", "C", "D"]),
            (r#"name_gt: "Beta""#, &["C", "D"]),
            (r#"name_lte: "Beta""#, &["A", "B"]),
            // Pattern operators ignore case (Mongo's `/i` regexes).
            (r#"name_startsWith: "Al""#, &["A", "D"]),
            (r#"name_startsWith: "AL""#, &["A", "D"]),
            (r#"name_endsWith: "A""#, &["A", "B", "C"]),
            (r#"name_contains: "PH""#, &["A", "D"]),
            (r#"name_notStartsWith: "al""#, &["B", "C"]),
            (r#"name_notEndsWith: "a""#, &["D"]),
            (r#"name_notContains: "ph""#, &["B", "C"]),
            // Equality and ordering don't.
            (r#"name: "beta""#, &[]),
            // LIKE metacharacters are literal text, not wildcards.
            (r#"name_contains: "_50%""#, &["D"]),
            (r#"name_contains: "%""#, &["D"]),
            (r#"name_startsWith: "Alph_""#, &[]),
            // Integer
            ("count: 4", &["D"]),
            ("count_not: 1", &["B", "C", "D"]),
            ("count_gt: 2", &["C", "D"]),
            ("count_gte: 2", &["B", "C", "D"]),
            ("count_lt: 2", &["A"]),
            ("count_lte: 2", &["A", "B"]),
            ("count_in: [1, 3]", &["A", "C"]),
            ("count_notIn: [1, 3]", &["B", "D"]),
            // Float
            ("price: 2.5", &["B"]),
            ("price_gt: 2.0", &["B", "C", "D"]),
            ("price_lte: 2.5", &["A", "B"]),
            // Enum
            ("size: LARGE", &["A", "C"]),
            ("size_not: LARGE", &["B", "D"]),
            // Boolean
            ("active: true", &["A", "C"]),
            ("active_not: true", &["B", "D"]),
            // Several filters AND together.
            ("count_gt: 1, active: true", &["C"]),
            (r#"name_endsWith: "a", count_lt: 3"#, &["A", "B"]),
        ];
        let mut failures = Vec::new();
        for (args, expected) in cases {
            let got = h.items_where(&ids, args).await;
            if got != labels(expected) {
                failures.push(format!("  getItems({args}): expected {expected:?}, got {got:?}"));
            }
        }
        assert!(failures.is_empty(), "filter mismatches:\n{}", failures.join("\n"));
    }

    /// Filters over fields with no value on some rows (B and D have no
    /// `note`/`maybeCount`), following the reference Mongo adapter:
    /// `ne`/`notIn` match a missing value (`$ne`/`$nin`); every other
    /// operator — the negated pattern ones included, being regexes, which
    /// only match strings — doesn't.
    pub const NULLABLE_FILTERS: &[(&str, &[&str])] = &[
        (r#"note: "x""#, &["A"]),
        (r#"note_not: "x""#, &["B", "C", "D"]),
        (r#"note_in: ["x", "y"]"#, &["A", "C"]),
        (r#"note_notIn: ["x"]"#, &["B", "C", "D"]),
        (r#"note_gt: "a""#, &["A", "C"]),
        (r#"note_startsWith: "X""#, &["A"]),
        (r#"note_notStartsWith: "x""#, &["C"]),
        (r#"note_notEndsWith: "x""#, &["C"]),
        (r#"note_notContains: "x""#, &["C"]),
        ("maybeCount: 10", &["A"]),
        ("maybeCount_not: 10", &["B", "C", "D"]),
        ("maybeCount_lt: 20", &["A"]),
        ("maybeCount_gte: 0", &["A", "C"]),
        ("maybeCount_notIn: [10]", &["B", "C", "D"]),
    ];

    pub async fn filters_on_missing_values(h: &Harness) {
        let ids = h.seed().await;
        // A value cleared by update (explicit null) behaves like one never set.
        let b = &ids["B"];
        h.run(&format!(r#"mutation {{ updateItem(id: "{b}", update: {{ note: null, maybeCount: null }}) {{ id }} }}"#)).await;
        let mut failures = Vec::new();
        for (args, expected) in NULLABLE_FILTERS {
            let got = h.items_where(&ids, args).await;
            if got != expected.iter().copied().collect() {
                failures.push(format!("  getItems({args}): expected {expected:?}, got {got:?}"));
            }
        }
        assert!(failures.is_empty(), "filter mismatches:\n{}", failures.join("\n"));
    }

    /// Has-one (batched) and has-many relations, including across adapters
    /// and across renamed (`data_name`) keys.
    pub async fn relations(h: &Harness) {
        let ids = h.seed().await;
        let data = h
            .run("{ getItems { name ownerRecord { name } } getOwners { name items { name } } }")
            .await;
        let mut items: Vec<(String, Value)> = data["getItems"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| (i["name"].as_str().unwrap().to_string(), i["ownerRecord"].clone()))
            .collect();
        items.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            items,
            vec![
                ("Alpha".into(), json!({ "name": "Olive" })),
                ("Beta".into(), json!({ "name": "Olive" })),
                ("Gamma".into(), json!({ "name": "Otto" })),
                ("alphabet_50%".into(), Value::Null),
            ]
        );

        let mut owners: Vec<(String, BTreeSet<String>)> = data["getOwners"]
            .as_array()
            .unwrap()
            .iter()
            .map(|o| {
                let names = o["items"].as_array().unwrap().iter().map(|i| i["name"].as_str().unwrap().to_string()).collect();
                (o["name"].as_str().unwrap().to_string(), names)
            })
            .collect();
        owners.sort();
        assert_eq!(
            owners,
            vec![
                ("Olive".into(), ["Alpha".to_string(), "Beta".to_string()].into()),
                ("Otto".into(), ["Gamma".to_string()].into()),
            ]
        );

        // Filtering by a relation's local field.
        assert_eq!(h.items_where(&ids, &format!(r#"owner: "{}""#, ids["o1"])).await, labels(&["A", "B"]));
    }

    /// Relation conditions with any operator and literal values, and a
    /// relation's own sort / start / limit — batched (has-one) and not
    /// (has-many), possibly across adapters.
    pub async fn relation_conditions_and_sorting(h: &Harness) {
        h.seed().await;
        let data = h
            .run(r#"{ getOwners(_orderBy: "name") {
                name
                expensiveItems { name }
                topItem { name }
                latestItem { name }
                secondItem { name }
            } }"#)
            .await;
        assert_eq!(
            data["getOwners"],
            json!([
                {
                    "name": "Olive",
                    "expensiveItems": [{ "name": "Beta" }],
                    "topItem": [{ "name": "Beta" }],
                    "latestItem": { "name": "Beta" },
                    "secondItem": { "name": "Alpha" },
                },
                {
                    "name": "Otto",
                    "expensiveItems": [{ "name": "Gamma" }],
                    "topItem": [{ "name": "Gamma" }],
                    "latestItem": { "name": "Gamma" },
                    "secondItem": null,
                },
            ])
        );
    }

    /// Join-mode relations (a `data_name` on the relation): same values as
    /// loading them, plus sorting by related fields — Mongo's rule through
    /// has-many (smallest value ascending, largest descending) — and paging
    /// over that sort. Rows not read through a list (a mutation's result)
    /// still resolve, by loading.
    pub async fn joined_relations(h: &Harness) {
        let ids = h.seed().await;
        let order = |args: &'static str| h.items_in_order(&ids, args);

        let joined = h.run(r#"{ getItems(_orderBy: "name") { ownerJoined { name } ownerRecord { name } } }"#).await;
        for item in joined["getItems"].as_array().unwrap() {
            assert_eq!(item["ownerJoined"], item["ownerRecord"], "{joined}");
        }
        let owners = h.run(r#"{ getOwners(_orderBy: "name") { name itemsJoined { name } } }"#).await;
        assert_eq!(
            owners["getOwners"],
            json!([
                { "name": "Olive", "itemsJoined": [{ "name": "Alpha" }, { "name": "Beta" }] },
                { "name": "Otto", "itemsJoined": [{ "name": "Gamma" }] },
            ])
        );

        // Sorting by a joined has-one field; no owner sorts first ascending.
        assert_eq!(order(r#"_orderBy: "ownerJoined.name, name""#).await, ["D", "A", "B", "C"]);
        assert_eq!(order(r#"_orderBy_desc: "ownerJoined.name, name""#).await, ["C", "B", "A", "D"]);
        assert_eq!(order(r#"_orderBy: "ownerJoined.name, name", _start: 1, _limit: 2"#).await, ["A", "B"]);
        let info = h.run(r#"{ getItemsPageInfo(_orderBy: "ownerJoined.name", _start: 2, _limit: 2) { totalResults page totalPages } }"#).await;
        assert_eq!(info["getItemsPageInfo"], json!({ "totalResults": 4, "page": 2, "totalPages": 2 }));
        let first = h.run(r#"{ getItem(_orderBy_desc: "ownerJoined.name") { name } }"#).await;
        assert_eq!(first["getItem"]["name"], "Gamma");

        // Through a has-many join: Olive's counts are 1 and 2, Otto's 3.
        let by = |args: &'static str| async move {
            h.run(&format!("{{ getOwners({args}) {{ name }} }}")).await["getOwners"].clone()
        };
        assert_eq!(by(r#"_orderBy: "itemsJoined.count""#).await, json!([{ "name": "Olive" }, { "name": "Otto" }]));
        assert_eq!(by(r#"_orderBy_desc: "itemsJoined.count""#).await, json!([{ "name": "Otto" }, { "name": "Olive" }]));

        let err = h.try_run(None, r#"{ getItems(_orderBy: "ownerJoined.nope") { id } }"#).await.unwrap_err();
        assert_eq!(err, "Unknown sort field `ownerJoined.nope`");

        // Not read through a list: the relation still resolves.
        let o2 = &ids["o2"];
        let added = h
            .run(&format!(r#"mutation {{ addItem(values: {{ name: "New", count: 9, price: 0, active: true, tags: [], size: SMALL, owner: "{o2}" }}) {{ ownerJoined {{ name }} }} }}"#))
            .await;
        assert_eq!(added["addItem"]["ownerJoined"], json!({ "name": "Otto" }));
    }

    pub async fn renamed_fields_filter_and_key(h: &Harness) {
        let ids = h.seed().await;
        let found = h.run(r#"{ getOwners(name_startsWith: "Ol") { id name } }"#).await;
        assert_eq!(found["getOwners"], json!([{ "id": ids["o1"], "name": "Olive" }]));
        let o2 = &ids["o2"];
        let renamed = h
            .run(&format!(r#"mutation {{ updateOwner(id: "{o2}", update: {{ name: "Ottoline" }}) {{ name }} }}"#))
            .await;
        assert_eq!(renamed["updateOwner"]["name"], "Ottoline");
        let got = h.run(&format!(r#"{{ getOwner(id: "{o2}") {{ name }} }}"#)).await;
        assert_eq!(got["getOwner"]["name"], "Ottoline");
    }

    /// Parallel writes must all land — no writer may overwrite another's.
    pub async fn concurrent_writes_are_all_kept(h: &Harness) {
        let adds = (0..100).map(|i| {
            let schema = h.schema.clone();
            tokio::spawn(async move {
                let q = format!(r#"mutation {{ addItem(values: {{ name: "n{i}", count: {i}, price: 0, active: true, tags: [], size: SMALL }}) {{ id }} }}"#);
                let res = schema.execute(q).await;
                assert!(res.errors.is_empty(), "{:?}", res.errors);
                res.data.into_json().unwrap()["addItem"]["id"].as_str().unwrap().to_string()
            })
        });
        let mut ids = Vec::new();
        for a in adds.collect::<Vec<_>>() {
            ids.push(a.await.unwrap());
        }
        let listed = h.run("{ getItems { id } }").await;
        assert_eq!(listed["getItems"].as_array().unwrap().len(), 100, "rows lost to concurrent adds");

        let updates = ids.iter().enumerate().map(|(i, id)| {
            let schema = h.schema.clone();
            let q = format!(r#"mutation {{ updateItem(id: "{id}", update: {{ count: {} }}) {{ id }} }}"#, i + 1000);
            tokio::spawn(async move {
                let res = schema.execute(q).await;
                assert!(res.errors.is_empty(), "{:?}", res.errors);
            })
        });
        for u in updates.collect::<Vec<_>>() {
            u.await.unwrap();
        }
        let updated = h.run("{ getItems(count_gte: 1000) { id } }").await;
        assert_eq!(updated["getItems"].as_array().unwrap().len(), 100, "updates lost to concurrent writes");
    }

    /// Keys are unique (Mongo's `_id` is). Clients never choose keys, so
    /// this is checked at the adapter — where hand-written resolvers and
    /// bulk loads (e.g. recording frames, which bring their own ids) write.
    pub async fn duplicate_keys_are_rejected(h: &Harness) {
        let adapter = &h.adapters["default"];
        let row = |id: &str, name: &str| json!({ "id": id, "name": name, "count": 1, "price": 0.0, "active": true, "tags": [], "size": "Small", "events": [] });
        let items = || typiql::Location::from("items");

        adapter.add(items(), "id", row("k1", "first")).await.unwrap();
        let err = adapter.add(items(), "id", row("k1", "second")).await.unwrap_err();
        assert!(err.starts_with("Duplicate key"), "{err}");

        // A batch clashing with an existing row, or with itself, is rejected
        // whole: none of its rows are written.
        let err = adapter
            .add_many(items(), "id", vec![row("k2", "ok"), row("k1", "clash")])
            .await
            .unwrap_err();
        assert!(err.starts_with("Duplicate key"), "{err}");
        let err = adapter
            .add_many(items(), "id", vec![row("k3", "a"), row("k3", "b")])
            .await
            .unwrap_err();
        assert!(err.starts_with("Duplicate key"), "{err}");

        let names = h.run("{ getItems { id name } }").await;
        assert_eq!(names["getItems"], json!([{ "id": "k1", "name": "first" }]));

        // Generated keys never clash, and a clean batch goes in whole.
        adapter.add_many(items(), "id", vec![row("k4", "b1"), row("k5", "b2")]).await.unwrap();
        h.run(r#"mutation { addItem(values: { name: "gen", count: 1, price: 0, active: true, tags: [], size: SMALL }) { id } }"#).await;
        assert_eq!(h.run("{ getItems { id } }").await["getItems"].as_array().unwrap().len(), 4);
    }

    /// `_orderBy`/`_orderBy_desc`, with Mongo's ordering: rows without a
    /// value first ascending, strings by bytes (uppercase before
    /// lowercase), `false` before `true`. Ties are broken by key, and keys
    /// here are random — so tied rows are checked as a group.
    pub async fn sorting(h: &Harness) {
        let ids = h.seed().await;
        let order = |args: &'static str| h.items_in_order(&ids, args);
        let groups = |v: Vec<&'static str>, sizes: &[usize]| -> Vec<BTreeSet<&'static str>> {
            let mut out = Vec::new();
            let mut it = v.into_iter();
            for &n in sizes {
                out.push(it.by_ref().take(n).collect());
            }
            out
        };

        assert_eq!(order(r#"_orderBy: "name""#).await, ["A", "B", "C", "D"]);
        assert_eq!(order(r#"_orderBy_desc: "name""#).await, ["D", "C", "B", "A"]);
        assert_eq!(order(r#"_orderBy_desc: "count""#).await, ["D", "C", "B", "A"]);
        assert_eq!(order(r#"_orderBy: "price""#).await, ["A", "B", "C", "D"]);
        // Several keys: ascending keys first, then descending.
        assert_eq!(order(r#"_orderBy: "active", _orderBy_desc: "count""#).await, ["D", "B", "C", "A"]);
        assert_eq!(order(r#"_orderBy: "active, name""#).await, ["B", "D", "A", "C"]);
        // No value sorts first ascending, last descending.
        assert_eq!(
            groups(order(r#"_orderBy: "maybeCount""#).await, &[2, 1, 1]),
            [labels(&["B", "D"]), labels(&["A"]), labels(&["C"])]
        );
        assert_eq!(
            groups(order(r#"_orderBy_desc: "maybeCount""#).await, &[1, 1, 2]),
            [labels(&["C"]), labels(&["A"]), labels(&["B", "D"])]
        );
        // Enums sort by their stored form.
        assert_eq!(
            groups(order(r#"_orderBy: "size""#).await, &[2, 2]),
            [labels(&["A", "C"]), labels(&["B", "D"])]
        );
        // Sorting combines with filters.
        assert_eq!(order(r#"active: true, _orderBy_desc: "count""#).await, ["C", "A"]);
        // Renamed (`data_name`) fields sort by their GraphQL name.
        let owners = h.run(r#"{ getOwners(_orderBy_desc: "name") { name } }"#).await;
        assert_eq!(owners["getOwners"], json!([{ "name": "Otto" }, { "name": "Olive" }]));

        let err = h.try_run(None, r#"{ getItems(_orderBy: "nope") { id } }"#).await.unwrap_err();
        assert_eq!(err, "Unknown sort field `nope`");
        let err = h.try_run(None, r#"{ getItems(_orderBy: "ownerRecord") { id } }"#).await.unwrap_err();
        assert_eq!(err, "Unknown sort field `ownerRecord`");
    }

    /// `_start`/`_limit`, `<list>PageInfo`, and `get` honouring both.
    pub async fn paging(h: &Harness) {
        let ids = h.seed().await;
        let order = |args: &'static str| h.items_in_order(&ids, args);
        assert_eq!(order(r#"_orderBy: "count", _start: 1, _limit: 2"#).await, ["B", "C"]);
        assert_eq!(order(r#"_orderBy: "count", _start: 3"#).await, ["D"]);
        assert_eq!(order(r#"_orderBy: "count", _start: 9"#).await, Vec::<&str>::new());
        // `_limit: 0` means no limit, as in C#.
        assert_eq!(order(r#"_orderBy: "count", _limit: 0"#).await, ["A", "B", "C", "D"]);
        assert_eq!(
            h.try_run(None, "{ getItems(_start: -1) { id } }").await.unwrap_err(),
            "`_start` can't be negative"
        );

        let info = |args: &'static str| async move {
            h.run(&format!("{{ getItemsPageInfo({args}) {{ totalResults page totalPages }} }}"))
                .await["getItemsPageInfo"]
                .clone()
        };
        assert_eq!(info(r#"_orderBy: "count", _start: 2, _limit: 2"#).await, json!({ "totalResults": 4, "page": 2, "totalPages": 2 }));
        assert_eq!(info("_start: 3, _limit: 3").await, json!({ "totalResults": 4, "page": 2, "totalPages": 2 }));
        assert_eq!(info("active: true").await, json!({ "totalResults": 2, "page": 1, "totalPages": 1 }));
        assert_eq!(info(r#"name: "nobody", _limit: 5"#).await, json!({ "totalResults": 0, "page": 1, "totalPages": 1 }));

        // `get` is the first row after sorting and `_start`.
        let first = |args: &'static str| async move {
            h.run(&format!("{{ getItem({args}) {{ name }} }}")).await["getItem"]["name"].clone()
        };
        assert_eq!(first(r#"_orderBy_desc: "count""#).await, "alphabet_50%");
        assert_eq!(first(r#"_orderBy: "count", _start: 1"#).await, "Beta");
        assert_eq!(first(r#"active: false, _orderBy: "count""#).await, "Beta");
    }

    /// Upsert: update the matching row, or create it — from the update's
    /// values, the equality values that found it, and `set`.
    pub async fn upsert(h: &Harness) {
        let (alice, bob) = (user("alice"), user("bob"));
        let save = |bio: &str| format!(r#"mutation {{ updateMyProfile(update: {{ bio: "{bio}", visits: 1 }}) {{ id username bio visits seen }} }}"#);

        // First save creates the row, owned by the caller; the second
        // updates that same row.
        let first = h.run_as(&alice, &save("hello")).await["updateMyProfile"].clone();
        assert_eq!(first["username"], "alice");
        assert_eq!(first["bio"], "hello");
        assert!(first["seen"].as_str().unwrap().ends_with('Z'));
        let second = h.run_as(&alice, &save("updated")).await["updateMyProfile"].clone();
        assert_eq!(second["id"], first["id"]);
        assert_eq!(second["bio"], "updated");
        h.run_as(&bob, &save("bob's")).await;
        assert_eq!(h.run("{ getProfilesPageInfo { totalResults } }").await["getProfilesPageInfo"]["totalResults"], 2);
        assert_eq!(h.run_as(&alice, "{ myProfile { bio } }").await["myProfile"]["bio"], "updated");
        assert_eq!(
            h.try_run(None, &save("anon")).await.unwrap_err(),
            "Not authenticated",
            "an unresolvable scope never upserts an ownerless row"
        );

        // Found by an external id instead of the key.
        let import = |bio: &str| format!(r#"mutation {{ importProfile(externalId: "ext-1", update: {{ bio: "{bio}", visits: 5 }}) {{ id externalId bio }} }}"#);
        let created = h.run(&import("imported")).await["importProfile"].clone();
        assert_eq!(created["externalId"], "ext-1");
        let again = h.run(&import("re-imported")).await["importProfile"].clone();
        assert_eq!((again["id"].clone(), again["bio"].clone()), (created["id"].clone(), json!("re-imported")));

        // A keyed update: no row, no upsert → null; `_upsert: true` creates
        // it under the given key.
        let keyed = |upsert: &str| format!(r#"mutation {{ updateProfile(id: "p-9", update: {{ bio: "keyed", visits: 2 }}{upsert}) {{ id bio }} }}"#);
        assert_eq!(h.run(&keyed("")).await["updateProfile"], Value::Null);
        assert_eq!(h.run(&keyed(", _upsert: true")).await["updateProfile"], json!({ "id": "p-9", "bio": "keyed" }));
        assert_eq!(h.run(&keyed(", _upsert: true")).await["updateProfile"], json!({ "id": "p-9", "bio": "keyed" }));
        assert_eq!(h.run("{ getProfilesPageInfo { totalResults } }").await["getProfilesPageInfo"]["totalResults"], 4);

        // Declared upserts hide `_upsert`; a `by()` update takes no key.
        let sdl = h.schema.sdl();
        let line = sdl.lines().find(|l| l.trim_start().starts_with("updateMyProfile(")).unwrap();
        assert_eq!(line.trim(), "updateMyProfile(update: ProfileInput!): Profile");
        let line = sdl.lines().find(|l| l.trim_start().starts_with("importProfile(")).unwrap();
        assert_eq!(line.trim(), "importProfile(externalId: String!, update: ProfileInput!): Profile");
    }

    /// Bulk add / update / remove: every matching row, one call; a bulk
    /// change with no filter at all is refused rather than hitting every row.
    pub async fn bulk_operations(h: &Harness) {
        let titles = |v: &Value| -> BTreeSet<String> {
            v.as_array().unwrap().iter().map(|t| t["title"].as_str().unwrap().to_string()).collect()
        };
        let set = |l: &[&str]| -> BTreeSet<String> { l.iter().map(|s| s.to_string()).collect() };

        let added = h
            .run(r#"mutation { addTasks(items: [
                { title: "a", done: false, owner: "alice" },
                { title: "b", done: true, owner: "alice" },
                { title: "c", done: false, owner: "bob" },
                { title: "d", done: true, owner: "bob" }
            ]) { id title created } }"#)
            .await;
        let added = added["addTasks"].as_array().unwrap();
        assert_eq!(added.len(), 4);
        assert!(added.iter().all(|t| t["id"].as_str().is_some_and(|id| !id.is_empty())));
        assert!(added.iter().all(|t| t["created"].as_str().is_some_and(|c| c.ends_with('Z'))));

        let updated = h.run(r#"mutation { updateTasks(done: false, update: { done: true }) { title done } }"#).await;
        assert_eq!(titles(&updated["updateTasks"]), set(&["a", "c"]));
        assert_eq!(h.run("{ getTasks(done: true) { title } }").await["getTasks"].as_array().unwrap().len(), 4);
        let renamed = h.run(r#"mutation { updateTasks(title_in: ["a", "b"], update: { title: "ab" }) { title } }"#).await;
        assert_eq!(renamed["updateTasks"].as_array().unwrap().len(), 2);

        // No filter at all: refused, and nothing changed.
        let err = h.try_run(None, r#"mutation { updateTasks(update: { done: false }) { id } }"#).await.unwrap_err();
        assert_eq!(err, "`updateTasks` needs at least one filter — without one it would change every row");
        let err = h.try_run(None, r#"mutation { removeTasks { id } }"#).await.unwrap_err();
        assert_eq!(err, "`removeTasks` needs at least one filter — without one it would change every row");
        assert_eq!(h.run("{ getTasks { id } }").await["getTasks"].as_array().unwrap().len(), 4);

        // A scope is a filter: clearing "my" tasks touches only mine.
        let cleared = h.run_as(&user("alice"), r#"mutation { clearMyTasks { title } }"#).await;
        assert_eq!(titles(&cleared["clearMyTasks"]), set(&["ab"]));
        assert_eq!(titles(&h.run("{ getTasks { title } }").await["getTasks"]), set(&["c", "d"]));

        let removed = h.run(r#"mutation { removeTasks(title: "c") { title } }"#).await;
        assert_eq!(titles(&removed["removeTasks"]), set(&["c"]));
        assert_eq!(titles(&h.run("{ getTasks { title } }").await["getTasks"]), set(&["d"]));
        let none = h.run(r#"mutation { removeTasks(title: "nope") { title } }"#).await;
        assert_eq!(none["removeTasks"], json!([]));
    }

    /// Filters inside lists: `anyEq`/`anyNe` on lists of values (contains
    /// any / none of), and element filters on lists of objects — all of one
    /// filter's conditions on the same element; a position with no element
    /// checked as an empty one (so `_last: { kind_not }` matches no events,
    /// as C#'s `lastNot` did).
    pub async fn list_filters(h: &Harness) {
        let ids = h.seed().await;
        let cases: &[(&str, &[&'static str])] = &[
            (r#"tags_anyEq: ["red", "green"]"#, &["A", "C"]),
            (r#"tags_anyEq: []"#, &[]),
            (r#"tags_anyNe: ["red"]"#, &["B", "C", "D"]),
            (r#"events_any: { kind: "out" }"#, &["A", "C"]),
            (r#"events_any: { kind_startsWith: "O" }"#, &["A", "C"]),
            // Same element: C's "out" is at 3; A's is at 5.
            (r#"events_any: { kind: "out", at_lt: 4 }"#, &["C"]),
            (r#"events_any: { at_gte: 5 }"#, &["A", "C"]),
            (r#"events_first: { kind: "in" }"#, &["A", "B"]),
            (r#"events_last: { kind: "out" }"#, &["A"]),
            (r#"events_last: { kind_not: "out" }"#, &["B", "C", "D"]),
            (r#"events_atIndex: { index: 1, filter: { kind: "in" } }"#, &["C"]),
            (r#"events_atIndex: { index: -1, filter: { at_gt: 4 } }"#, &["A", "C"]),
            (r#"events_atIndex: { index: 5, filter: { kind: "in" } }"#, &[]),
            // With other filters and sorting.
            (r#"events_any: { kind: "in" }, count_gt: 1"#, &["B", "C"]),
        ];
        let mut failures = Vec::new();
        for (args, expected) in cases {
            let got = h.items_where(&ids, args).await;
            if got != labels(expected) {
                failures.push(format!("  getItems({args}): expected {expected:?}, got {got:?}"));
            }
        }
        assert!(failures.is_empty(), "list filter mismatches:\n{}", failures.join("\n"));
        assert_eq!(h.items_in_order(&ids, r#"events_any: { kind: "in" }, _orderBy_desc: "count""#).await, ["C", "B", "A"]);
        let info = h.run(r#"{ getItemsPageInfo(events_any: { kind: "out" }) { totalResults } }"#).await;
        assert_eq!(info["getItemsPageInfo"]["totalResults"], 2);
    }

    /// Logging: every error, plus successful calls of opted-in operations
    /// and relation fields — as rows of the log type, queryable like any
    /// other. Entries are written in the background, so this polls.
    pub async fn logging(h: &Harness) {
        async fn logs(h: &Harness, args: &str, want: usize) -> Vec<Value> {
            for _ in 0..100 {
                let rows = h
                    .run(&format!("{{ getLogs({args}) {{ operation kind user arguments result error path durationMs }} }}"))
                    .await["getLogs"]
                    .as_array()
                    .unwrap()
                    .clone();
                if rows.len() >= want {
                    return rows;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            panic!("expected {want} log entries for getLogs({args})");
        }
        h.seed().await;

        // An opted-in operation's success: arguments and result.
        h.run_as(&user("alice"), r#"mutation { addNote(values: { body: "logged" }) { id } }"#).await;
        let entry = &logs(h, r#"operation: "addNote", result_contains: """#, 1).await[0];
        assert_eq!((entry["kind"].as_str(), entry["user"].as_str()), (Some("add"), Some("alice")));
        assert!(entry["arguments"].as_str().unwrap().contains("logged"), "{entry}");
        assert!(entry["result"].as_str().unwrap().contains('"'), "{entry}");
        assert!(entry["durationMs"].as_f64().is_some());

        // Errors are always logged — refusals too — with their arguments,
        // from operations that didn't opt in as well.
        let _ = h.try_run(None, r#"mutation { addNote(values: { body: "anon" }) { id } }"#).await;
        let err = &logs(h, r#"operation: "addNote", error: "Not authenticated""#, 1).await[0];
        assert!(err["arguments"].as_str().unwrap().contains("anon"), "{err}");
        assert_eq!(err["user"], Value::Null);
        let _ = h.schema.execute(async_graphql::Request::new(r#"query Q($o: String) { renamed: getItems(_orderBy: $o) { id } }"#)
            .variables(async_graphql::Variables::from_json(json!({ "o": "nope" })))).await;
        let err = &logs(h, r#"operation: "getItems", error_contains: """#, 1).await[0];
        assert_eq!(err["kind"], "list");
        assert_eq!(err["error"], "Unknown sort field `nope`");
        assert_eq!(err["arguments"], r#"{"_orderBy":"nope"}"#, "variables are filled in");

        // An opted-in relation field.
        h.run(r#"{ getOwners { items { name } } }"#).await;
        let field = logs(h, r#"operation: "Owner.items""#, 2).await;
        assert!(field.iter().all(|e| e["kind"] == "field"));

        // Successful calls that didn't opt in aren't logged.
        let quiet = h.run(r#"{ getLogs(operation_in: ["getItems", "getOwners"], result_contains: "") { id } }"#).await;
        assert_eq!(quiet["getLogs"], json!([]));

        // Logs are queryable like any other type.
        let info = h.run(r#"{ getLogsPageInfo(error_contains: "") { totalResults } }"#).await;
        assert!(info["getLogsPageInfo"]["totalResults"].as_i64().unwrap() >= 2);
    }

    /// Integer keys: new rows are numbered by the adapter; get/update/remove
    /// by number; a keyed upsert creates that number, and numbering carries
    /// on after it; duplicates are refused.
    pub async fn integer_keys(h: &Harness) {
        let mut ids = Vec::new();
        for label in ["a", "b", "c"] {
            let added = h.run(&format!(r#"mutation {{ addCounter(values: {{ label: "{label}" }}) {{ id label }} }}"#)).await;
            ids.push(added["addCounter"]["id"].as_i64().expect("an integer key"));
        }
        assert!(ids.windows(2).all(|w| w[1] > w[0]), "increasing: {ids:?}");
        let (b, c) = (ids[1], ids[2]);

        assert_eq!(h.run(&format!("{{ getCounter(id: {b}) {{ label }} }}")).await["getCounter"]["label"], "b");
        assert_eq!(h.run(&format!("{{ getCounters(id_gt: {}) {{ id }} }}", ids[0])).await["getCounters"].as_array().unwrap().len(), 2);
        let updated = h.run(&format!(r#"mutation {{ updateCounter(id: {b}, update: {{ label: "B" }}) {{ id label }} }}"#)).await;
        assert_eq!(updated["updateCounter"], json!({ "id": b, "label": "B" }));
        assert_eq!(h.run(&format!("mutation {{ removeCounter(id: {c}) {{ id }} }}")).await["removeCounter"]["id"], c);
        assert_eq!(h.run(&format!("{{ getCounter(id: {c}) {{ id }} }}")).await["getCounter"], Value::Null);

        let explicit = h.run(r#"mutation { updateCounter(id: 100, update: { label: "hundred" }, _upsert: true) { id label } }"#).await;
        assert_eq!(explicit["updateCounter"], json!({ "id": 100, "label": "hundred" }));
        let after = h.run(r#"mutation { addCounter(values: { label: "next" }) { id } }"#).await;
        assert!(after["addCounter"]["id"].as_i64().unwrap() > 100, "{after}");

        let err = h.adapters["default"]
            .add(typiql::Location::from("counters"), "id", json!({ "id": 100, "label": "again" }))
            .await
            .unwrap_err();
        assert!(err.starts_with("Duplicate key"), "{err}");
        assert_eq!(h.run("{ getCounter(id: 100) { label } }").await["getCounter"]["label"], "hundred");
    }

    /// Aggregate types: groups (missing values form their own group), every
    /// function — sum of nothing is 0, avg/min/max of nothing null — then
    /// filters, sorting and paging over the groups. An empty source has no
    /// rows at all, as with Mongo's `$group`.
    pub async fn aggregates(h: &Harness) {
        let ids = h.seed().await;
        let (o1, o2) = (ids["o1"].clone(), ids["o2"].clone());
        let fields = "owner items noted total avgPrice firstName maxPrice";
        let rows = h.run(&format!("{{ getItemSummaries {{ {fields} }} }}")).await["getItemSummaries"].clone();
        let mut by_owner: HashMap<String, Value> = rows
            .as_array()
            .unwrap()
            .iter()
            .map(|r| (r["owner"].as_str().unwrap_or("none").to_string(), r.clone()))
            .collect();
        assert_eq!(by_owner.len(), 3, "{rows}");
        assert_eq!(
            by_owner.remove(&o1).unwrap(),
            json!({ "owner": o1, "items": 2, "noted": 1, "total": 3, "avgPrice": 2.0, "firstName": "Alpha", "maxPrice": 2.5 })
        );
        assert_eq!(
            by_owner.remove(&o2).unwrap(),
            json!({ "owner": o2, "items": 1, "noted": 1, "total": 3, "avgPrice": 3.5, "firstName": "Gamma", "maxPrice": 3.5 })
        );
        assert_eq!(
            by_owner.remove("none").unwrap(),
            json!({ "owner": null, "items": 1, "noted": 0, "total": 4, "avgPrice": 4.5, "firstName": "alphabet_50%", "maxPrice": 4.5 })
        );

        // Filters, sorting and paging apply to the groups.
        let owners = |args: &'static str| async move {
            h.run(&format!("{{ getItemSummaries({args}) {{ owner }} }}")).await["getItemSummaries"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r["owner"].as_str().map(str::to_string))
                .collect::<Vec<_>>()
        };
        assert_eq!(owners("items_gte: 2").await, [Some(o1.clone())]);
        assert_eq!(owners(r#"_orderBy_desc: "avgPrice""#).await, [None, Some(o2.clone()), Some(o1.clone())]);
        assert_eq!(owners(r#"_orderBy: "avgPrice", _start: 1, _limit: 1"#).await, [Some(o2.clone())]);
        let info = h.run("{ getItemSummariesPageInfo(total: 3) { totalResults } }").await;
        assert_eq!(info["getItemSummariesPageInfo"]["totalResults"], 2);

        let totals = h.run("{ itemTotals { items total revenue } }").await;
        assert_eq!(totals["itemTotals"], json!([{ "items": 4, "total": 10, "revenue": 12.0 }]));
        let empty = h.run("{ taskTotals { tasks nonsense } taskTotalsPageInfo { totalResults } }").await;
        assert_eq!(empty, json!({ "taskTotals": [], "taskTotalsPageInfo": { "totalResults": 0 } }));
    }

    pub async fn scope_set_and_roles(h: &Harness) {
        let (alice, bob) = (user("alice"), user("bob"));
        let add = |body: &str| format!(r#"mutation {{ addNote(values: {{ body: "{body}", owner: "mallory" }}) {{ id owner created }} }}"#);

        assert_eq!(h.try_run(None, &add("anon")).await.unwrap_err(), "Not authenticated");
        let mine = h.run_as(&alice, &add("alice's")).await;
        assert_eq!(mine["addNote"]["owner"], "alice");
        assert!(mine["addNote"]["created"].as_str().unwrap().ends_with('Z'));
        let alices = mine["addNote"]["id"].as_str().unwrap().to_string();
        let bobs = h.run_as(&bob, &add("bob's")).await["addNote"]["id"].as_str().unwrap().to_string();

        let listed = h.run_as(&alice, "{ myNotes { body } }").await;
        assert_eq!(listed["myNotes"], json!([{ "body": "alice's" }]));

        let steal = format!(r#"mutation {{ updateNote(id: "{bobs}", update: {{ body: "mine now" }}) {{ id }} }}"#);
        assert_eq!(h.run_as(&alice, &steal).await["updateNote"], Value::Null);
        let delete = format!(r#"mutation {{ removeNote(id: "{bobs}") {{ id }} }}"#);
        assert_eq!(h.run_as(&alice, &delete).await["removeNote"], Value::Null);
        assert_eq!(h.run_as(&bob, "{ myNotes { body } }").await["myNotes"], json!([{ "body": "bob's" }]));

        let edit = format!(r#"mutation {{ updateNote(id: "{alices}", update: {{ body: "edited" }}) {{ body owner }} }}"#);
        assert_eq!(h.run_as(&alice, &edit).await["updateNote"], json!({ "body": "edited", "owner": "alice" }));
        let drop = format!(r#"mutation {{ removeNote(id: "{alices}") {{ id }} }}"#);
        assert_eq!(h.run_as(&alice, &drop).await["removeNote"]["id"], alices.as_str());
        assert_eq!(h.run_as(&alice, "{ myNotes { body } }").await["myNotes"], json!([]));

        // A scoped list's PageInfo counts only what the scope allows.
        let info = h.run_as(&bob, "{ myNotesPageInfo { totalResults } }").await;
        assert_eq!(info["myNotesPageInfo"]["totalResults"], 1);
    }
}

/// Generates one `#[tokio::test]` per case, each against a fresh store from
/// `$store` (a `fn() -> Store`). With `requires = "ENV"`, each test is
/// skipped (with a note) unless that environment variable is set — for
/// adapters that need a running server.
#[macro_export]
macro_rules! conformance_suite {
    ($store:path) => {
        $crate::conformance_suite!($store, requires = "");
    };
    ($store:path, requires = $env:literal) => {
        $crate::conformance_suite!(@cases $store, $env;
            add_then_read_back,
            update_patches_only_what_was_sent,
            remove_deletes_one,
            filter_operators,
            filters_on_missing_values,
            list_filters,
            relations,
            relation_conditions_and_sorting,
            joined_relations,
            renamed_fields_filter_and_key,
            sorting,
            paging,
            upsert,
            integer_keys,
            aggregates,
            bulk_operations,
            logging,
            concurrent_writes_are_all_kept,
            duplicate_keys_are_rejected,
            scope_set_and_roles,
        );
    };
    (@cases $store:path, $env:literal; $($case:ident),* $(,)?) => {
        $(
            // Multi-threaded, so concurrency cases really run in parallel.
            #[::tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $case() {
                if !$env.is_empty() && ::std::env::var($env).is_err() {
                    eprintln!("skipped: set {} to run against that server", $env);
                    return;
                }
                let harness = $crate::Harness::new($store());
                $crate::cases::$case(&harness).await;
            }
        )*
    };
}
