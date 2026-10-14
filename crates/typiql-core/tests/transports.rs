//! The router's transports, against a real server: queries and mutations
//! over HTTP, subscriptions over WebSockets (`graphql-transport-ws`, and the
//! legacy `graphql-ws`) and over multipart HTTP — with the same auth on each.

extern crate typiql_core as typiql;

mod common;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::http::{HeaderName, HeaderValue, Request};
use common::MemAdapter;
use futures_util::{SinkExt, StreamExt};
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper_util::rt::TokioIo;
use serde::Serialize;
use serde_json::{json, Value};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};
use typiql::{AdapterMap, TypiQLAdapter, TypiQLRouter, TypiQLUser};

const PATH: &str = "/typiql/graphql";
const WAIT: Duration = Duration::from_secs(5);

#[derive(Serialize, Clone)]
struct AppUser {
    username: String,
    roles: Vec<String>,
}

impl TypiQLUser for AppUser {
    fn name(&self) -> &str {
        &self.username
    }
    fn has_role(&self, role: &str) -> bool {
        self.roles.iter().any(|r| r == role)
    }
}

fn app_user(name: &str, roles: &str) -> AppUser {
    AppUser {
        username: name.into(),
        roles: roles.split(',').filter(|r| !r.is_empty()).map(String::from).collect(),
    }
}

#[typiql::typiql_type]
pub struct Project {
    #[typiql(key)]
    pub id: String,
    pub name: String,
}

#[typiql::typiql_type]
pub struct Task {
    #[typiql(key)]
    pub id: String,
    pub title: String,
}

/// Only `User`s may subscribe, and each sees only their own documents.
#[typiql::typiql_type(ops(
    add(set(owner = "@currentUser")),
    subscribe(roles("User"), scope(owner = "@currentUser")),
))]
pub struct Doc {
    #[typiql(key)]
    pub id: String,
    pub owner: String,
    pub title: String,
}

mod schema {
    use super::*;
    typiql::typiql_schema!(Project, Task, Doc);
}

fn schema() -> async_graphql::Schema<schema::TypiQLQuery, schema::TypiQLMutation, schema::TypiQLSubscription> {
    let mut map: AdapterMap = HashMap::new();
    map.insert("default", Arc::new(MemAdapter::default()) as Arc<dyn TypiQLAdapter>);
    schema::build_typiql_schema(map)
}

/// The app's own auth, standing in for JWT/cookie middleware: the user is
/// named by an `x-test-user` header (roles in `x-test-roles`) or a
/// `user=<name>` cookie (role `User`).
async fn test_auth(mut req: axum::extract::Request, next: axum::middleware::Next) -> axum::response::Response {
    if let Some(user) = user_from(req.headers()) {
        req.extensions_mut().insert(user);
    }
    next.run(req).await
}

fn user_from(headers: &axum::http::HeaderMap) -> Option<AppUser> {
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    match header("x-test-user") {
        Some(name) => Some(app_user(name, header("x-test-roles").unwrap_or_default())),
        None => header("cookie")
            .and_then(|c| c.split(';').find_map(|kv| kv.trim().strip_prefix("user=")))
            .map(|name| app_user(name, "User")),
    }
}

/// `connectionParams` auth: `{ "token": "<name>:<roles>" }`; `"bad"` is refused.
async fn token_auth(params: Value) -> async_graphql::Result<Option<AppUser>> {
    match params.get("token").and_then(Value::as_str) {
        None => Ok(None),
        Some("bad") => Err("Invalid token".into()),
        Some(token) => {
            let (name, roles) = token.split_once(':').unwrap_or((token, ""));
            Ok(Some(app_user(name, roles)))
        }
    }
}

async fn serve(router: axum::Router) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    addr
}

/// An app as a consumer writes it: the typiql router nested under `/typiql`,
/// behind the app's own auth middleware.
async fn start(configure: impl FnOnce(TypiQLRouter<schema::TypiQLQuery, schema::TypiQLMutation, schema::TypiQLSubscription>) -> TypiQLRouter<schema::TypiQLQuery, schema::TypiQLMutation, schema::TypiQLSubscription>) -> SocketAddr {
    let typiql = configure(TypiQLRouter::new(schema(), PATH).with_user::<AppUser>()).build();
    serve(axum::Router::new().nest("/typiql", typiql).layer(axum::middleware::from_fn(test_auth))).await
}

// --- HTTP ---------------------------------------------------------------

async fn http(addr: SocketAddr, req: Request<Full<Bytes>>) -> hyper::Response<hyper::body::Incoming> {
    let io = TokioIo::new(TcpStream::connect(addr).await.unwrap());
    let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await.unwrap();
    tokio::spawn(conn);
    sender.send_request(req).await.unwrap()
}

fn post(addr: SocketAddr, body: Value, headers: &[(&str, &str)]) -> Request<Full<Bytes>> {
    let mut req = Request::post(PATH)
        .header("host", addr.to_string())
        .header("content-type", "application/json");
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    req.body(Full::new(Bytes::from(body.to_string()))).unwrap()
}

async fn gql(addr: SocketAddr, query: &str, headers: &[(&str, &str)]) -> Value {
    let res = http(addr, post(addr, json!({ "query": query }), headers)).await;
    let body = res.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&body).unwrap()
}

// --- Subscriptions, over either transport --------------------------------

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>;

enum Sub {
    Ws { socket: Box<Ws>, legacy: bool },
    Multipart { body: hyper::body::Incoming, buf: String },
}

/// One event's `data`, or the first error message the server sent for it.
type Event = Result<Value, String>;

impl Sub {
    /// Opens a `graphql-transport-ws` (or, `legacy`, `graphql-ws`) socket,
    /// waiting for `connection_ack`. Fails with the reason a refused
    /// connection gives.
    async fn connect(
        addr: SocketAddr,
        legacy: bool,
        headers: &[(&str, &str)],
        params: Option<Value>,
    ) -> Result<Sub, String> {
        let mut req = format!("ws://{addr}{PATH}").into_client_request().unwrap();
        let protocol = if legacy { "graphql-ws" } else { "graphql-transport-ws" };
        req.headers_mut().insert("sec-websocket-protocol", HeaderValue::from_static(protocol));
        for (k, v) in headers {
            req.headers_mut().insert(HeaderName::from_bytes(k.as_bytes()).unwrap(), HeaderValue::from_str(v).unwrap());
        }
        let (socket, res) = tokio_tungstenite::connect_async(req).await.map_err(|e| e.to_string())?;
        assert_eq!(res.headers()["sec-websocket-protocol"], protocol);
        let mut sub = Sub::Ws { socket: Box::new(socket), legacy };
        let mut init = json!({ "type": "connection_init" });
        if let Some(params) = params {
            init["payload"] = params;
        }
        sub.send(init).await;
        match sub.recv().await? {
            m if m["type"] == "connection_ack" => Ok(sub),
            m => Err(format!("no connection_ack: {m}")),
        }
    }

    async fn ws(addr: SocketAddr, query: &str, headers: &[(&str, &str)], params: Option<Value>) -> Sub {
        let mut sub = Sub::connect(addr, false, headers, params).await.unwrap();
        sub.subscribe(query).await;
        sub
    }

    async fn subscribe(&mut self, query: &str) {
        let Sub::Ws { legacy, .. } = self else { unreachable!() };
        let kind = if *legacy { "start" } else { "subscribe" };
        self.send(json!({ "id": "1", "type": kind, "payload": { "query": query } })).await;
        settle().await;
    }

    async fn multipart(addr: SocketAddr, query: &str, headers: &[(&str, &str)]) -> Sub {
        let mut all = vec![("accept", r#"multipart/mixed; boundary="graphql"; subscriptionSpec="1.0""#)];
        all.extend_from_slice(headers);
        let res = http(addr, post(addr, json!({ "query": query }), &all)).await;
        assert!(res.headers()["content-type"].to_str().unwrap().starts_with("multipart/mixed"));
        settle().await;
        Sub::Multipart { body: res.into_body(), buf: String::new() }
    }

    async fn send(&mut self, msg: Value) {
        let Sub::Ws { socket, .. } = self else { unreachable!() };
        socket.send(Message::text(msg.to_string())).await.unwrap();
    }

    /// The next protocol message (WebSocket only); a close frame becomes an
    /// error carrying its reason.
    async fn recv(&mut self) -> Result<Value, String> {
        let Sub::Ws { socket, .. } = self else { unreachable!() };
        loop {
            match tokio::time::timeout(WAIT, socket.next()).await {
                Err(_) => return Err("timed out".into()),
                Ok(None) => return Err("socket ended".into()),
                Ok(Some(Err(e))) => return Err(e.to_string()),
                Ok(Some(Ok(Message::Text(t)))) => return Ok(serde_json::from_str(&t).unwrap()),
                Ok(Some(Ok(Message::Close(frame)))) => {
                    return Err(frame.map(|f| f.reason.to_string()).unwrap_or_default())
                }
                Ok(Some(Ok(_))) => {}
            }
        }
    }

    /// The next event, whichever transport.
    async fn next(&mut self) -> Event {
        let payload = match self {
            Sub::Ws { .. } => loop {
                let msg = self.recv().await?;
                match msg["type"].as_str().unwrap() {
                    "next" | "data" => break msg["payload"].clone(),
                    "ping" => self.send(json!({ "type": "pong" })).await,
                    "ka" | "pong" => {}
                    _ => return Err(msg.to_string()),
                }
            },
            Sub::Multipart { body, buf } => loop {
                if let Some(part) = take_part(buf) {
                    break part;
                }
                let frame = tokio::time::timeout(WAIT, body.frame())
                    .await
                    .map_err(|_| "timed out".to_string())?
                    .ok_or("stream ended")?
                    .unwrap();
                if let Ok(data) = frame.into_data() {
                    buf.push_str(std::str::from_utf8(&data).unwrap());
                }
            },
        };
        match payload["errors"].get(0) {
            Some(e) => Err(e["message"].as_str().unwrap().to_string()),
            None => Ok(payload["data"].clone()),
        }
    }

    /// The next event about one of `names` — other tests share the
    /// process-wide broker, so their events can arrive here too.
    async fn next_about(&mut self, names: &[&str]) -> Event {
        loop {
            let data = self.next().await?;
            let text = data.to_string();
            if names.iter().any(|n| text.contains(&format!("\"{n}\""))) {
                return Ok(data);
            }
        }
    }
}

/// Removes and returns the first complete non-heartbeat part of a
/// `multipart/mixed; boundary=graphql` body.
fn take_part(buf: &mut String) -> Option<Value> {
    const HEAD: &str = "--graphql\r\nContent-Type: application/json\r\n\r\n";
    loop {
        let start = buf.find(HEAD)? + HEAD.len();
        let end = start + buf[start..].find("\r\n")?;
        let part = buf[start..end].to_string();
        buf.drain(..end + 2);
        if part != "{}" {
            return Some(serde_json::from_str(&part).unwrap());
        }
    }
}

/// Lets a just-sent subscription reach the broker before anything is
/// published.
async fn settle() {
    tokio::time::sleep(Duration::from_millis(150)).await;
}

fn unique(prefix: &str) -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    format!("{prefix}-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed))
}

async fn add_project(addr: SocketAddr, name: &str) {
    let res = gql(addr, &format!(r#"mutation {{ addProject(values: {{ name: "{name}" }}) {{ id }} }}"#), &[]).await;
    assert!(res["errors"].is_null(), "{res}");
}

async fn add_task(addr: SocketAddr, title: &str) {
    let res = gql(addr, &format!(r#"mutation {{ addTask(values: {{ title: "{title}" }}) {{ id }} }}"#), &[]).await;
    assert!(res["errors"].is_null(), "{res}");
}

async fn add_doc(addr: SocketAddr, owner: &str, title: &str) {
    let q = format!(r#"mutation {{ addDoc(values: {{ title: "{title}", owner: "x" }}) {{ id }} }}"#);
    let res = gql(addr, &q, &[("x-test-user", owner)]).await;
    assert!(res["errors"].is_null(), "{res}");
}

const DOCS: &str = "subscription { docChanged { operationName value { owner title } } }";

// --- WebSockets -----------------------------------------------------------

#[tokio::test]
async fn websocket_streams_a_single_field() {
    let addr = start(|r| r).await;
    let name = unique("single");
    let mut sub = Sub::ws(addr, "subscription { projectChanged { operationName value { name } } }", &[], None).await;

    add_project(addr, &name).await;
    assert_eq!(
        sub.next_about(&[&name]).await.unwrap(),
        json!({ "projectChanged": { "operationName": "add", "value": { "name": name } } })
    );
}

#[tokio::test]
async fn websocket_streams_an_aliased_multi_field_subscription() {
    let addr = start(|r| r).await;
    let (project, task) = (unique("multi-project"), unique("multi-task"));
    let mut sub = Sub::ws(
        addr,
        "subscription { a: projectChanged { value { name } } b: taskChanged { value { title } } }",
        &[],
        None,
    )
    .await;

    add_project(addr, &project).await;
    add_task(addr, &task).await;
    let mut got = vec![
        sub.next_about(&[&project, &task]).await.unwrap(),
        sub.next_about(&[&project, &task]).await.unwrap(),
    ];
    got.sort_by_key(|v| v.to_string());
    assert_eq!(
        got,
        vec![
            json!({ "a": { "value": { "name": project } } }),
            json!({ "b": { "value": { "title": task } } }),
        ]
    );
}

#[tokio::test]
async fn legacy_graphql_ws_protocol_is_served_too() {
    let addr = start(|r| r).await;
    let name = unique("legacy");
    let mut sub = Sub::connect(addr, true, &[], None).await.unwrap();
    sub.subscribe("subscription { projectChanged { value { name } } }").await;

    add_project(addr, &name).await;
    assert_eq!(
        sub.next_about(&[&name]).await.unwrap(),
        json!({ "projectChanged": { "value": { "name": name } } })
    );
}

#[tokio::test]
async fn several_subscriptions_share_one_socket() {
    let addr = start(|r| r).await;
    let (project, task) = (unique("shared-project"), unique("shared-task"));
    let mut sub = Sub::ws(addr, "subscription { projectChanged { value { name } } }", &[], None).await;
    sub.send(json!({ "id": "2", "type": "subscribe", "payload": { "query": "subscription { taskChanged { value { title } } }" } }))
        .await;
    settle().await;

    add_project(addr, &project).await;
    add_task(addr, &task).await;
    let mut seen = Vec::new();
    while seen.len() < 2 {
        let msg = sub.recv().await.unwrap();
        let text = msg.to_string();
        if msg["type"] == "next" && (text.contains(&project) || text.contains(&task)) {
            seen.push((msg["id"].as_str().unwrap().to_string(), text.contains(&project)));
        }
    }
    seen.sort();
    assert_eq!(seen, vec![("1".to_string(), true), ("2".to_string(), false)]);
}

// --- Auth parity ------------------------------------------------------------

/// The same three subscribers — anonymous, a user without the role, and
/// `owner` with it — get the same outcome over any transport.
async fn check_auth(addr: SocketAddr, open: impl AsyncFn(Option<(&str, &str)>) -> Sub) {
    let (owner, other) = (unique("owner"), unique("other"));

    let mut anonymous = open(None).await;
    assert_eq!(anonymous.next().await.unwrap_err(), "Not authenticated");
    let mut no_role = open(Some((&owner, ""))).await;
    assert_eq!(no_role.next().await.unwrap_err(), "Access denied");

    let mut sub = open(Some((&owner, "User"))).await;
    add_doc(addr, &other, "theirs").await;
    add_doc(addr, &owner, "mine").await;
    assert_eq!(
        sub.next_about(&[&owner, &other]).await.unwrap(),
        json!({ "docChanged": { "operationName": "add", "value": { "owner": owner, "title": "mine" } } })
    );
}

#[tokio::test]
async fn multipart_subscriptions_enforce_roles_and_scope() {
    let addr = start(|r| r).await;
    check_auth(addr, async |user: Option<(&str, &str)>| match user {
        None => Sub::multipart(addr, DOCS, &[]).await,
        Some((name, roles)) => Sub::multipart(addr, DOCS, &[("x-test-user", name), ("x-test-roles", roles)]).await,
    })
    .await;
}

#[tokio::test]
async fn websocket_subscriptions_get_the_upgrade_requests_user() {
    let addr = start(|r| r).await;
    check_auth(addr, async |user: Option<(&str, &str)>| match user {
        None => Sub::ws(addr, DOCS, &[], None).await,
        Some((name, roles)) => Sub::ws(addr, DOCS, &[("x-test-user", name), ("x-test-roles", roles)], None).await,
    })
    .await;
}

#[tokio::test]
async fn websocket_subscriptions_get_a_cookie_user() {
    let addr = start(|r| r).await;
    let (owner, other) = (unique("cookie-owner"), unique("cookie-other"));
    let cookie = format!("theme=dark; user={owner}");
    let mut sub = Sub::ws(addr, DOCS, &[("cookie", &cookie)], None).await;
    add_doc(addr, &other, "theirs").await;
    add_doc(addr, &owner, "mine").await;
    assert_eq!(sub.next_about(&[&owner, &other]).await.unwrap()["docChanged"]["value"]["owner"], owner);
}

#[tokio::test]
async fn websocket_subscriptions_get_a_connection_params_user() {
    let addr = start(|r| r.with_connection_auth(token_auth)).await;
    check_auth(addr, async |user: Option<(&str, &str)>| match user {
        None => Sub::ws(addr, DOCS, &[], Some(json!({}))).await,
        Some((name, roles)) => Sub::ws(addr, DOCS, &[], Some(json!({ "token": format!("{name}:{roles}") }))).await,
    })
    .await;
}

#[tokio::test]
async fn connection_params_user_overrides_the_upgrade_requests() {
    let addr = start(|r| r.with_connection_auth(token_auth)).await;
    let (header_user, token_user) = (unique("header"), unique("token"));
    let mut sub = Sub::ws(
        addr,
        DOCS,
        &[("x-test-user", &header_user), ("x-test-roles", "User")],
        Some(json!({ "token": format!("{token_user}:User") })),
    )
    .await;
    add_doc(addr, &header_user, "header's").await;
    add_doc(addr, &token_user, "token's").await;
    assert_eq!(
        sub.next_about(&[&header_user, &token_user]).await.unwrap()["docChanged"]["value"]["owner"],
        token_user
    );
}

#[tokio::test]
async fn connection_params_without_credentials_fall_back_to_the_upgrade_requests_user() {
    let addr = start(|r| r.with_connection_auth(token_auth)).await;
    let owner = unique("fallback");
    let mut sub = Sub::ws(addr, DOCS, &[("x-test-user", &owner), ("x-test-roles", "User")], None).await;
    add_doc(addr, &owner, "mine").await;
    assert_eq!(sub.next_about(&[&owner]).await.unwrap()["docChanged"]["value"]["owner"], owner);
}

#[tokio::test]
async fn bad_connection_params_refuse_the_connection() {
    let addr = start(|r| r.with_connection_auth(token_auth)).await;
    let err = Sub::connect(addr, false, &[], Some(json!({ "token": "bad" }))).await.err().unwrap();
    assert_eq!(err, "Invalid token");
}

// --- Everything else is unchanged ---------------------------------------

#[tokio::test]
async fn multipart_still_streams_aliased_subscriptions() {
    let addr = start(|r| r).await;
    let (project, task) = (unique("mp-project"), unique("mp-task"));
    let mut sub = Sub::multipart(
        addr,
        "subscription { a: projectChanged { value { name } } b: taskChanged { value { title } } }",
        &[],
    )
    .await;
    add_project(addr, &project).await;
    add_task(addr, &task).await;
    let mut got = vec![
        sub.next_about(&[&project, &task]).await.unwrap(),
        sub.next_about(&[&project, &task]).await.unwrap(),
    ];
    got.sort_by_key(|v| v.to_string());
    assert_eq!(
        got,
        vec![
            json!({ "a": { "value": { "name": project } } }),
            json!({ "b": { "value": { "title": task } } }),
        ]
    );
}

#[tokio::test]
async fn http_queries_mutations_get_and_playground_still_work() {
    let addr = start(|r| r).await;
    let name = unique("http");
    add_project(addr, &name).await;

    let posted = gql(addr, &format!(r#"{{ getProjects(name: "{name}") {{ name }} }}"#), &[]).await;
    assert_eq!(posted["data"]["getProjects"], json!([{ "name": name }]));

    let query = format!(r#"{{ getProjects(name: "{name}") {{ name }} }}"#);
    let mut url = String::from(PATH);
    url.push_str("?query=");
    for b in query.bytes() {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' => url.push(b as char),
            _ => url.push_str(&format!("%{b:02X}")),
        }
    }
    let get = Request::get(url).header("host", addr.to_string()).body(Full::default()).unwrap();
    let res = http(addr, get).await;
    let body: Value = serde_json::from_slice(&res.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(body["data"]["getProjects"], json!([{ "name": name }]));

    let playground = Request::get("/typiql/playground").header("host", addr.to_string()).body(Full::default()).unwrap();
    let res = http(addr, playground).await;
    assert_eq!(res.status(), 200);
    let html = res.into_body().collect().await.unwrap().to_bytes();
    assert!(std::str::from_utf8(&html).unwrap().contains(PATH));
}

#[tokio::test]
async fn typiql_router_serves_websockets_too() {
    let addr = serve(axum::Router::new().nest("/typiql", typiql::typiql_router(schema(), PATH))).await;
    let name = unique("plain");
    let mut sub = Sub::ws(addr, "subscription { projectChanged { value { name } } }", &[], None).await;
    add_project(addr, &name).await;
    assert_eq!(sub.next_about(&[&name]).await.unwrap(), json!({ "projectChanged": { "value": { "name": name } } }));
}

#[tokio::test]
async fn websockets_can_be_turned_off() {
    let addr = start(|r| r.without_websockets()).await;
    assert!(Sub::connect(addr, false, &[], None).await.is_err());
    // Multipart is unaffected.
    let name = unique("off");
    let mut sub = Sub::multipart(addr, "subscription { projectChanged { value { name } } }", &[]).await;
    add_project(addr, &name).await;
    assert_eq!(sub.next_about(&[&name]).await.unwrap(), json!({ "projectChanged": { "value": { "name": name } } }));
}
