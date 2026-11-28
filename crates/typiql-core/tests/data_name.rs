//! `data_name` is adapter-side only: the store sees it everywhere, GraphQL
//! never does.

extern crate typiql_core as typiql;

mod common;

use std::collections::HashMap;
use std::sync::Arc;

use common::MemAdapter;
use serde::Serialize;
use serde_json::{json, Value};
use typiql::{with_user, AdapterMap, TypiQLAdapter, TypiQLType, TypiQLUser};

#[derive(Serialize, Clone)]
struct AppUser {
    username: String,
}

impl TypiQLUser for AppUser {
    fn name(&self) -> &str {
        &self.username
    }
    fn has_role(&self, _: &str) -> bool {
        true
    }
}

#[typiql::typiql_type(ops(
    get,
    list,
    add(set(owner = "@currentUser")),
    update(scope(owner = "@currentUser")),
))]
pub struct Vehicle {
    #[typiql(key, data_name = "_id")]
    pub id: String,
    #[typiql(data_name = "MAKE_NAME")]
    pub make: String,
    // Has-one relations need an `Option` local field.
    #[typiql(data_name = "OwnerId")]
    pub owner: Option<String>,
    #[typiql(relation(local = "owner", foreign = "id"))]
    pub driver: Option<Driver>,
}

#[typiql::typiql_type]
pub struct Driver {
    #[typiql(key, data_name = "_id")]
    pub id: String,
    #[typiql(data_name = "full_name")]
    pub name: String,
    #[typiql(relation(local = "id", foreign = "owner"))]
    pub vehicles: Vec<Vehicle>,
}

mod schema {
    use super::*;
    typiql::typiql_schema!(Vehicle, Driver);
    pub type TypiQLSchema = async_graphql::Schema<TypiQLQuery, TypiQLMutation, TypiQLSubscription>;
}

async fn setup() -> (Arc<MemAdapter>, schema::TypiQLSchema) {
    let adapter = Arc::new(MemAdapter::default());
    adapter
        .add(
            "drivers".into(),
            "_id",
            json!({ "_id": "alice", "full_name": "Alice" }),
        )
        .await
        .unwrap();
    let mut map: AdapterMap = HashMap::new();
    map.insert("default", adapter.clone() as Arc<dyn TypiQLAdapter>);
    (adapter, schema::build_typiql_schema(map))
}

async fn run(schema: &schema::TypiQLSchema, query: &str) -> Value {
    let req = with_user(
        async_graphql::Request::new(query),
        AppUser {
            username: "alice".into(),
        },
    );
    let res = schema.execute(req).await;
    assert!(res.errors.is_empty(), "{query}: {:?}", res.errors);
    res.data.into_json().unwrap()
}

#[test]
fn trait_reports_storage_names() {
    assert_eq!(Vehicle::key_field(), "_id");
    assert_eq!(Vehicle::data_name("make"), "MAKE_NAME");
    assert_eq!(Vehicle::data_name("unrenamed"), "unrenamed");
}

#[tokio::test]
async fn graphql_uses_field_names_the_store_uses_data_names() {
    let (adapter, schema) = setup().await;

    let sdl = schema.sdl();
    for storage in ["MAKE_NAME", "OwnerId", "_id", "full_name"] {
        assert!(
            !sdl.contains(storage),
            "{storage} leaked into the schema:\n{sdl}"
        );
    }

    let added = run(
        &schema,
        r#"mutation { addVehicle(values: { make: "Mazda" }) { id make owner } }"#,
    )
    .await;
    let id = added["addVehicle"]["id"].as_str().unwrap().to_string();
    assert_eq!(added["addVehicle"]["make"], "Mazda");
    assert_eq!(added["addVehicle"]["owner"], "alice");

    let stored = adapter.tables.lock().unwrap()["vehicles"][0].clone();
    assert_eq!(
        stored,
        json!({ "_id": id, "MAKE_NAME": "Mazda", "OwnerId": "alice" })
    );

    // Update: scoped by the stored owner column, patch written by data_name.
    let updated = run(
        &schema,
        &format!(r#"mutation {{ updateVehicle(id: "{id}", update: {{ make: "Mazda MX-5" }}) {{ make }} }}"#),
    )
    .await;
    assert_eq!(updated["updateVehicle"]["make"], "Mazda MX-5");
    assert_eq!(
        adapter.tables.lock().unwrap()["vehicles"][0]["MAKE_NAME"],
        "Mazda MX-5"
    );

    // Filters, by GraphQL name.
    let found = run(&schema, r#"{ getVehicles(make: "Mazda MX-5") { id } }"#).await;
    assert_eq!(found["getVehicles"].as_array().unwrap().len(), 1);
    let one = run(
        &schema,
        &format!(r#"{{ getVehicle(id: "{id}") {{ make }} }}"#),
    )
    .await;
    assert_eq!(one["getVehicle"]["make"], "Mazda MX-5");

    // Relations name the target's fields the GraphQL way, both directions.
    let rel = run(
        &schema,
        "{ getVehicles { driver { name } } getDrivers { name vehicles { make } } }",
    )
    .await;
    assert_eq!(
        rel,
        json!({
            "getVehicles": [{ "driver": { "name": "Alice" } }],
            "getDrivers": [{ "name": "Alice", "vehicles": [{ "make": "Mazda MX-5" }] }],
        })
    );
}
