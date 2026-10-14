use std::convert::Infallible;
use std::future::Future;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use async_graphql::http::{
    create_multipart_mixed_stream, is_accept_multipart_mixed, playground_source,
    GraphQLPlaygroundConfig, ALL_WEBSOCKET_PROTOCOLS,
};
use async_graphql::{BatchRequest, Data, Executor, ObjectType, SubscriptionType};
use async_graphql_axum::{
    rejection::GraphQLRejection, GraphQLBatchRequest, GraphQLProtocol, GraphQLRequest,
    GraphQLWebSocket,
};
use axum::{
    body::Body,
    extract::{FromRequest, FromRequestParts, WebSocketUpgrade},
    http::{header, Method, Request, Response},
    response::{Html, IntoResponse},
    routing::get,
    Router,
};
use futures_util::{future::BoxFuture, FutureExt, StreamExt};

use crate::auth::{insert_user, TypiQLUser};

async fn make_playground(graphql_path: String) -> Html<String> {
    Html(playground_source(GraphQLPlaygroundConfig::new(
        &graphql_path,
    )))
}

/// GraphQL handler that uses a short multipart heartbeat (1 s) so idle
/// subscription streams don't stall for the default 30 s between keep-alive
/// ticks. Events are still delivered immediately; the heartbeat only affects
/// how quickly a browser ReadableStream reader wakes up on an idle connection.
///
/// The same service also accepts WebSocket upgrades (unless disabled, see
/// [`TypiQLRouter::without_websockets`]) and serves them with
/// async-graphql's `graphql-transport-ws` / legacy `graphql-ws` support.
#[derive(Clone)]
struct TypiQLGraphQL<E> {
    executor: E,
    attach: AttachFn,
    websockets: bool,
    connection_auth: Option<ConnectionAuth>,
}

/// Copies whatever per-request state typiql needs (today: the app's user,
/// see `TypiQLRouter::with_user`) from the HTTP request's extensions into a
/// GraphQL request's data — or, for a WebSocket, into the connection's data,
/// which every subscription on it sees.
type AttachFn = fn(&http::Extensions, &mut Data);

fn attach_nothing(_: &http::Extensions, _: &mut Data) {}

fn attach_user<U: TypiQLUser>(ext: &http::Extensions, data: &mut Data) {
    if let Some(user) = ext.get::<U>() {
        insert_user(data, user.clone());
    }
}

fn attach_request(attach: AttachFn, ext: &http::Extensions, mut req: async_graphql::Request) -> async_graphql::Request {
    attach(ext, &mut req.data);
    req
}

/// Turns a WebSocket's `connection_init` payload into connection data — see
/// [`TypiQLRouter::with_connection_auth`].
type ConnectionAuth =
    Arc<dyn Fn(serde_json::Value) -> BoxFuture<'static, async_graphql::Result<Data>> + Send + Sync>;

/// A WebSocket handshake: `GET` + `Upgrade: websocket` (HTTP/1.1), or an
/// extended `CONNECT` (HTTP/2, RFC 8441).
fn is_websocket_upgrade<B>(req: &Request<B>) -> bool {
    if req.method() == Method::CONNECT {
        return true;
    }
    req.method() == Method::GET
        && req
            .headers()
            .get(header::UPGRADE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
}

impl<E> tower::Service<Request<Body>> for TypiQLGraphQL<E>
where
    E: Executor + Clone,
{
    type Response = Response<Body>;
    type Error = Infallible;
    type Future = BoxFuture<'static, Result<Self::Response, Infallible>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let executor = self.executor.clone();
        let attach = self.attach;
        if self.websockets && is_websocket_upgrade(&req) {
            let connection_auth = self.connection_auth.clone();
            return Box::pin(async move {
                let (mut parts, _body) = req.into_parts();
                let protocol = match GraphQLProtocol::from_request_parts(&mut parts, &()).await {
                    Ok(p) => p,
                    Err(e) => return Ok(e.into_response()),
                };
                let upgrade = match WebSocketUpgrade::from_request_parts(&mut parts, &()).await {
                    Ok(u) => u,
                    Err(e) => return Ok(e.into_response()),
                };
                // The upgrade request went through the app's middleware like
                // any other request, so its user (from a cookie or header)
                // is attached exactly as for HTTP.
                let mut data = Data::default();
                attach(&parts.extensions, &mut data);
                Ok(upgrade
                    .protocols(ALL_WEBSOCKET_PROTOCOLS)
                    .on_upgrade(move |socket| {
                        let ws = GraphQLWebSocket::new(socket, executor, protocol).with_data(data);
                        match connection_auth {
                            Some(auth) => ws.on_connection_init(move |payload| auth(payload)).serve().boxed(),
                            None => ws.serve().boxed(),
                        }
                    })
                    .into_response())
            });
        }
        let ext = req.extensions().clone();
        Box::pin(async move {
            let is_multipart = req
                .headers()
                .get("accept")
                .and_then(|v| v.to_str().ok())
                .map(is_accept_multipart_mixed)
                .unwrap_or(false);

            if is_multipart {
                let gql_req = match GraphQLRequest::<GraphQLRejection>::from_request(req, &()).await
                {
                    Ok(r) => r,
                    Err(e) => return Ok(axum::response::IntoResponse::into_response(e)),
                };
                let stream = executor.execute_stream(attach_request(attach, &ext, gql_req.0), None);
                let body = Body::from_stream(
                    create_multipart_mixed_stream(stream, Duration::from_secs(1))
                        .map(Ok::<_, std::io::Error>),
                );
                Ok(Response::builder()
                    .header("content-type", "multipart/mixed; boundary=graphql")
                    .body(body)
                    .expect("BUG: invalid response"))
            } else {
                let batch =
                    match GraphQLBatchRequest::<GraphQLRejection>::from_request(req, &()).await {
                        Ok(r) => r,
                        Err(e) => return Ok(axum::response::IntoResponse::into_response(e)),
                    };
                let batch = match batch.0 {
                    BatchRequest::Single(r) => BatchRequest::Single(attach_request(attach, &ext, r)),
                    BatchRequest::Batch(rs) => BatchRequest::Batch(
                        rs.into_iter().map(|r| attach_request(attach, &ext, r)).collect(),
                    ),
                };
                let resp = async_graphql_axum::GraphQLResponse(executor.execute_batch(batch).await);
                Ok(axum::response::IntoResponse::into_response(resp))
            }
        })
    }
}

/// Returns an axum `Router` that serves:
///   POST `/graphql`      — GraphQL endpoint (HTTP + multipart subscriptions)
///   GET  `/graphql`      — the same over a query string, or, with
///                          `Upgrade: websocket`, subscriptions over a
///                          WebSocket (`graphql-transport-ws`, as the
///                          `graphql-ws` npm package speaks, or the legacy
///                          `graphql-ws` subprotocol of
///                          subscriptions-transport-ws)
///   GET  `/playground`   — GraphQL Playground UI
///
/// `graphql_path` is the absolute URL the Playground uses for its requests
/// (e.g. `"/typiql/graphql"` if you nest this router under `/typiql`).
///
/// Nest this under your chosen prefix in your application router:
/// ```rust,ignore
/// router.nest("/typiql", typiql_router(schema, "/typiql/graphql"))
/// ```
///
/// Shorthand for `TypiQLRouter::new(schema, graphql_path).build()` — an app
/// with users uses `TypiQLRouter` directly, adding `.with_user::<MyUser>()`.
pub fn typiql_router<Q, M, S>(schema: async_graphql::Schema<Q, M, S>, graphql_path: &str) -> Router
where
    Q: ObjectType + 'static,
    M: ObjectType + 'static,
    S: SubscriptionType + 'static,
{
    TypiQLRouter::new(schema, graphql_path).build()
}

/// Builder for the typiql axum router — see [`typiql_router`] for what it
/// serves.
///
/// ```rust,ignore
/// let typiql = TypiQLRouter::new(schema, "/typiql/graphql")
///     .with_user::<MyUser>()
///     .build();
/// router
///     .nest("/typiql", typiql)
///     // The app's own auth (JWT, integrated auth, ...) inserts `MyUser`
///     // into the request extensions; typiql never authenticates.
///     .layer(axum::middleware::from_fn(my_auth_middleware))
/// ```
pub struct TypiQLRouter<Q, M, S> {
    schema: async_graphql::Schema<Q, M, S>,
    graphql_path: String,
    attach: AttachFn,
    websockets: bool,
    connection_auth: Option<ConnectionAuth>,
}

impl<Q, M, S> TypiQLRouter<Q, M, S>
where
    Q: ObjectType + 'static,
    M: ObjectType + 'static,
    S: SubscriptionType + 'static,
{
    pub fn new(schema: async_graphql::Schema<Q, M, S>, graphql_path: &str) -> Self {
        TypiQLRouter {
            schema,
            graphql_path: graphql_path.to_string(),
            attach: attach_nothing,
            websockets: true,
            connection_auth: None,
        }
    }

    /// Makes the app's user type `U` available to GraphQL: whenever an
    /// incoming request's extensions hold a `U` (put there by the app's own
    /// auth middleware), it's attached to the GraphQL request — as `U` for
    /// custom resolvers, and as [`crate::CurrentUser`] for generated
    /// `roles`/`scope`/`set` checks. A request without one runs anonymously.
    ///
    /// A WebSocket upgrade request passes through the app's middleware too,
    /// so a user it establishes (from a cookie, or a header a non-browser
    /// client can set) is attached to every subscription on that socket —
    /// subscriptions' `roles`/`scope` behave exactly as over HTTP. Browsers
    /// can't set headers on a WebSocket; for token auth there, see
    /// [`Self::with_connection_auth`].
    pub fn with_user<U: TypiQLUser>(mut self) -> Self {
        self.attach = attach_user::<U>;
        self
    }

    /// Authenticates WebSocket connections from their `connection_init`
    /// payload — what a `graphql-ws` client sends as `connectionParams`
    /// (e.g. `{ authorization: "Bearer …" }`), since a browser can't set
    /// headers on a WebSocket.
    ///
    /// `auth` gets the payload (`{}` if the client sent none) and returns:
    /// - `Ok(Some(user))` — every subscription on the socket runs as `user`
    ///   (overriding any user the upgrade request carried);
    /// - `Ok(None)` — no credentials in the payload: the upgrade request's
    ///   user (see [`Self::with_user`]) applies, or none;
    /// - `Err(_)` — bad credentials: the connection is refused (closed
    ///   before `connection_ack`).
    ///
    /// ```rust,ignore
    /// TypiQLRouter::new(schema, "/typiql/graphql")
    ///     .with_user::<MyUser>()
    ///     .with_connection_auth(|params: serde_json::Value| async move {
    ///         match params["authorization"].as_str() {
    ///             Some(token) => verify_jwt(token).map(Some), // -> Result<Option<MyUser>>
    ///             None => Ok(None),
    ///         }
    ///     })
    /// ```
    pub fn with_connection_auth<U, F, Fut>(mut self, auth: F) -> Self
    where
        U: TypiQLUser,
        F: Fn(serde_json::Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = async_graphql::Result<Option<U>>> + Send + 'static,
    {
        self.connection_auth = Some(Arc::new(move |payload| {
            let user = auth(payload);
            async move {
                let mut data = Data::default();
                if let Some(user) = user.await? {
                    insert_user(&mut data, user);
                }
                Ok(data)
            }
            .boxed()
        }));
        self
    }

    /// Serves subscriptions over multipart HTTP only: WebSocket upgrades on
    /// the GraphQL path are no longer accepted (they're handled as plain
    /// `GET`s, which fail for lack of a query).
    pub fn without_websockets(mut self) -> Self {
        self.websockets = false;
        self
    }

    pub fn build(self) -> Router {
        let path = self.graphql_path;
        Router::new()
            .route_service(
                "/graphql",
                TypiQLGraphQL {
                    executor: self.schema,
                    attach: self.attach,
                    websockets: self.websockets,
                    connection_auth: self.connection_auth,
                },
            )
            .route("/playground", get(move || make_playground(path.clone())))
    }
}
