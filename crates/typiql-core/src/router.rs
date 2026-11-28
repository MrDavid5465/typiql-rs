use std::convert::Infallible;
use std::task::{Context, Poll};
use std::time::Duration;

use async_graphql::http::{
    create_multipart_mixed_stream, is_accept_multipart_mixed, playground_source,
    GraphQLPlaygroundConfig,
};
use async_graphql::{BatchRequest, Executor, ObjectType, SubscriptionType};
use async_graphql_axum::{rejection::GraphQLRejection, GraphQLBatchRequest, GraphQLRequest};
use axum::{
    body::Body,
    extract::FromRequest,
    http::{Request, Response},
    response::Html,
    routing::get,
    Router,
};
use futures_util::{future::BoxFuture, StreamExt};

use crate::auth::{with_user, TypiQLUser};

async fn make_playground(graphql_path: String) -> Html<String> {
    Html(playground_source(GraphQLPlaygroundConfig::new(
        &graphql_path,
    )))
}

/// GraphQL handler that uses a short multipart heartbeat (1 s) so idle
/// subscription streams don't stall for the default 30 s between keep-alive
/// ticks. Events are still delivered immediately; the heartbeat only affects
/// how quickly a browser ReadableStream reader wakes up on an idle connection.
#[derive(Clone)]
struct TypiQLGraphQL<E> {
    executor: E,
    attach: AttachFn,
}

/// Copies whatever per-request state typiql needs (today: the app's user,
/// see `TypiQLRouter::with_user`) from the HTTP request's extensions into a
/// GraphQL request.
type AttachFn = fn(&http::Extensions, async_graphql::Request) -> async_graphql::Request;

fn attach_nothing(_: &http::Extensions, req: async_graphql::Request) -> async_graphql::Request {
    req
}

fn attach_user<U: TypiQLUser>(
    ext: &http::Extensions,
    req: async_graphql::Request,
) -> async_graphql::Request {
    match ext.get::<U>() {
        Some(user) => with_user(req, user.clone()),
        None => req,
    }
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
                let stream = executor.execute_stream(attach(&ext, gql_req.0), None);
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
                    BatchRequest::Single(r) => BatchRequest::Single(attach(&ext, r)),
                    BatchRequest::Batch(rs) => {
                        BatchRequest::Batch(rs.into_iter().map(|r| attach(&ext, r)).collect())
                    }
                };
                let resp = async_graphql_axum::GraphQLResponse(executor.execute_batch(batch).await);
                Ok(axum::response::IntoResponse::into_response(resp))
            }
        })
    }
}

/// Returns an axum `Router` that serves:
///   POST `/graphql`      — GraphQL endpoint (HTTP + multipart subscriptions)
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
        }
    }

    /// Makes the app's user type `U` available to GraphQL: whenever an
    /// incoming request's extensions hold a `U` (put there by the app's own
    /// auth middleware), it's attached to the GraphQL request — as `U` for
    /// custom resolvers, and as [`crate::CurrentUser`] for generated
    /// `roles`/`scope`/`set` checks. A request without one runs anonymously.
    pub fn with_user<U: TypiQLUser>(mut self) -> Self {
        self.attach = attach_user::<U>;
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
                },
            )
            .route("/playground", get(move || make_playground(path.clone())))
    }
}
