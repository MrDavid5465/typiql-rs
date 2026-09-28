//! Optional auth: `roles`, `scope`, `set` and `name` on declared operations,
//! driven by an app-defined user type.

extern crate typiql_core as typiql;

mod common;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_graphql::futures_util::StreamExt;
use serde::Serialize;
use serde_json::{json, Value};
use common::MemAdapter;
use typiql::{with_user, AdapterMap, TypiQLAdapter, TypiQLUser};

/// The app's own user type — whatever its auth produces.
#[derive(Serialize, Clone)]
struct AppUser {
    username: String,
    groups: Vec<String>,
    region: String,
}

impl TypiQLUser for AppUser {
    fn name(&self) -> &str {
        &self.username
    }
    fn has_role(&self, role: &str) -> bool {
        self.groups.iter().any(|g| g == role)
    }
}

fn user(name: &str, groups: &[&str]) -> AppUser {
    AppUser {
        username: name.into(),
        groups: groups.iter().map(|g| g.to_string()).collect(),
        region: "west".into(),
    }
}

/// Cars are never listed directly — only through `my { cars }` — and can
/// only be changed by their owner.
#[typiql::typiql_type(ops(
    add(roles("User"), set(owner = "@currentUser", region = "@currentUser.region", created_at = "@now")),
    update(roles("User"), scope(owner = "@currentUser")),
    remove(roles("User"), scope(owner = "@currentUser")),
    subscribe(scope(owner = "@currentUser")),
))]
pub struct Car {
    #[typiql(key)]
    pub id: String,
    pub owner: String,
    pub make: String,
    pub region: Option<String>,
    pub created_at: Option<String>,
}

#[typiql::typiql_type(ops(
    get(name = "my", scope(username = "@currentUser")),
    list(name = "users", roles("Admin")),
))]
pub struct Person {
    #[typiql(key)]
    pub username: String,
    pub display_name: String,
    #[typiql(relation(local = "username", foreign = "owner"))]
    pub cars: Vec<Car>,
}

mod schema {
    use super::*;
    typiql::typiql_schema!(Car, Person);
}

type Schema = async_graphql::Schema<schema::TypiQLQuery, schema::TypiQLMutation, schema::TypiQLSubscription>;

async fn setup() -> Schema {
    let adapter = Arc::new(MemAdapter::default());
    for name in ["alice", "bob"] {
        adapter
            .add("persons".into(), "username", json!({ "username": name, "display_name": name }))
            .await
            .unwrap();
    }
    let mut map: AdapterMap = HashMap::new();
    map.insert("default", adapter as Arc<dyn TypiQLAdapter>);
    schema::build_typiql_schema(map)
}

async fn run(schema: &Schema, as_user: Option<&AppUser>, query: &str) -> Result<Value, String> {
    let mut req = async_graphql::Request::new(query);
    if let Some(u) = as_user {
        req = with_user(req, u.clone());
    }
    let res = schema.execute(req).await;
    match res.errors.first() {
        Some(e) => Err(e.message.clone()),
        None => Ok(res.data.into_json().unwrap()),
    }
}

async fn add_car(schema: &Schema, as_user: &AppUser, make: &str) -> String {
    let data = run(
        schema,
        Some(as_user),
        &format!(r#"mutation {{ addCar(values: {{ make: "{make}", owner: "mallory" }}) {{ id }} }}"#),
    )
    .await
    .unwrap();
    data["addCar"]["id"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn roles_gate_operations() {
    let schema = setup().await;
    let add = r#"mutation { addCar(values: { make: "Miata" }) { id } }"#;
    assert_eq!(run(&schema, None, add).await.unwrap_err(), "Not authenticated");
    assert_eq!(run(&schema, Some(&user("alice", &[])), add).await.unwrap_err(), "Access denied");
    assert!(run(&schema, Some(&user("alice", &["User"])), add).await.is_ok());

    let users = "{ users { username } }";
    assert_eq!(run(&schema, Some(&user("alice", &["User"])), users).await.unwrap_err(), "Access denied");
    let admins = run(&schema, Some(&user("root", &["Admin"])), users).await.unwrap();
    assert_eq!(admins["users"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn set_overrides_client_values() {
    let schema = setup().await;
    let alice = user("alice", &["User"]);
    let data = run(
        &schema,
        Some(&alice),
        r#"mutation { addCar(values: { make: "Miata", owner: "mallory", region: "east" }) { owner region createdAt } }"#,
    )
    .await
    .unwrap();
    assert_eq!(data["addCar"]["owner"], "alice");
    assert_eq!(data["addCar"]["region"], "west");
    assert!(data["addCar"]["createdAt"].as_str().unwrap().ends_with('Z'));
}

#[tokio::test]
async fn my_reads_only_the_current_users_cars() {
    let schema = setup().await;
    let (alice, bob) = (user("alice", &["User"]), user("bob", &["User"]));
    add_car(&schema, &alice, "Miata").await;
    add_car(&schema, &bob, "Civic").await;

    let data = run(&schema, Some(&alice), "{ my { username cars { make } } }").await.unwrap();
    assert_eq!(data, json!({ "my": { "username": "alice", "cars": [{ "make": "Miata" }] } }));

    // The scoped field isn't a client filter, so it can't be overridden.
    let err = run(&schema, Some(&alice), r#"{ my(username: "bob") { username } }"#).await;
    assert!(err.is_err());
    // An unauthenticated scope fails rather than matching everyone.
    assert_eq!(run(&schema, None, "{ my { username } }").await.unwrap_err(), "Not authenticated");
}

#[tokio::test]
async fn update_and_remove_are_scoped_to_the_owner() {
    let schema = setup().await;
    let (alice, bob) = (user("alice", &["User"]), user("bob", &["User"]));
    let bobs = add_car(&schema, &bob, "Civic").await;
    let alices = add_car(&schema, &alice, "Miata").await;

    let steal = format!(r#"mutation {{ updateCar(id: "{bobs}", update: {{ make: "Stolen" }}) {{ id }} }}"#);
    assert_eq!(run(&schema, Some(&alice), &steal).await.unwrap(), json!({ "updateCar": null }));
    let drop_it = format!(r#"mutation {{ removeCar(id: "{bobs}") {{ id }} }}"#);
    assert_eq!(run(&schema, Some(&alice), &drop_it).await.unwrap(), json!({ "removeCar": null }));
    let bob_sees = run(&schema, Some(&bob), "{ my { cars { make } } }").await.unwrap();
    assert_eq!(bob_sees["my"]["cars"], json!([{ "make": "Civic" }]));

    let own = format!(r#"mutation {{ updateCar(id: "{alices}", update: {{ make: "MX-5" }}) {{ make }} }}"#);
    assert_eq!(run(&schema, Some(&alice), &own).await.unwrap(), json!({ "updateCar": { "make": "MX-5" } }));
}

#[tokio::test]
async fn cars_have_no_direct_reads() {
    let sdl = setup().await.sdl();
    assert!(!sdl.contains("getCar"), "{sdl}");
    assert!(sdl.contains("my("), "{sdl}");
    assert!(!sdl.contains("my(username"), "{sdl}");
}

#[tokio::test]
async fn scoped_subscription_only_streams_own_records() {
    let schema = setup().await;
    // The broker is process-wide, so use users no other test publishes as.
    let (alice, bob) = (user("carol", &["User"]), user("dave", &["User"]));
    let mut stream = schema.execute_stream(with_user(
        async_graphql::Request::new("subscription { carChanged { value { owner make } } }"),
        alice.clone(),
    ));
    // Let the subscription register with the broker before publishing.
    let first = tokio::time::timeout(Duration::from_millis(50), stream.next()).await;
    assert!(first.is_err(), "nothing published yet, got {:?}", first.map(|r| r.map(|r| (r.data, r.errors))));

    add_car(&schema, &bob, "Civic").await;
    add_car(&schema, &alice, "Miata").await;
    let event = tokio::time::timeout(Duration::from_secs(1), stream.next())
        .await
        .expect("alice's event")
        .unwrap();
    assert!(event.errors.is_empty(), "{:?}", event.errors);
    assert_eq!(
        event.data.into_json().unwrap(),
        json!({ "carChanged": { "value": { "owner": "carol", "make": "Miata" } } })
    );
}

#[tokio::test]
async fn router_attaches_the_apps_user_from_request_extensions() {
    use axum::body::Body;
    use tower::ServiceExt;

    let app = typiql::TypiQLRouter::new(setup().await, "/graphql")
        .with_user::<AppUser>()
        .build()
        // Stands in for the app's own auth middleware (JWT, integrated auth).
        .layer(axum::middleware::from_fn(
            |mut req: axum::extract::Request, next: axum::middleware::Next| async move {
                if req.headers().contains_key("x-test-user") {
                    req.extensions_mut().insert(user("alice", &["User"]));
                }
                next.run(req).await
            },
        ));

    let ask = |authed: bool| {
        let mut req = axum::http::Request::post("/graphql")
            .header("content-type", "application/json");
        if authed {
            req = req.header("x-test-user", "1");
        }
        let req = req.body(Body::from(r#"{"query":"{ my { username } }"}"#)).unwrap();
        let app = app.clone();
        async move {
            let res = app.oneshot(req).await.unwrap();
            let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
            serde_json::from_slice::<Value>(&bytes).unwrap()
        }
    };

    assert_eq!(ask(true).await["data"], json!({ "my": { "username": "alice" } }));
    assert_eq!(ask(false).await["errors"][0]["message"], "Not authenticated");
}
