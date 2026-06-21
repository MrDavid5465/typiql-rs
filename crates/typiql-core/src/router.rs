use std::convert::Infallible;
use std::task::{Context, Poll};
use std::time::Duration;

use async_graphql::{Executor, ObjectType, SubscriptionType};
use async_graphql::http::{
    create_multipart_mixed_stream, is_accept_multipart_mixed, playground_source,
    GraphQLPlaygroundConfig,
};
use async_graphql_axum::{GraphQLBatchRequest, GraphQLRequest, rejection::GraphQLRejection};
use axum::{
    body::Body,
    extract::FromRequest,
    http::{Request, Response},
    response::Html,
    routing::get,
    Router,
};
use futures_util::{future::BoxFuture, StreamExt};

async fn make_playground(graphql_path: String) -> Html<String> {
    Html(playground_source(
        GraphQLPlaygroundConfig::new(&graphql_path),
    ))
}

/// GraphQL handler that uses a short multipart heartbeat (1 s) so idle
/// subscription streams don't stall for the default 30 s between keep-alive
/// ticks. Events are still delivered immediately; the heartbeat only affects
/// how quickly a browser ReadableStream reader wakes up on an idle connection.
#[derive(Clone)]
struct TypiQLGraphQL<E> {
    executor: E,
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
                let stream = executor.execute_stream(gql_req.0, None);
                let body = Body::from_stream(
                    create_multipart_mixed_stream(stream, Duration::from_secs(1))
                        .map(Ok::<_, std::io::Error>),
                );
                Ok(Response::builder()
                    .header("content-type", "multipart/mixed; boundary=graphql")
                    .body(body)
                    .expect("BUG: invalid response"))
            } else {
                let batch = match GraphQLBatchRequest::<GraphQLRejection>::from_request(req, &()).await
                {
                    Ok(r) => r,
                    Err(e) => return Ok(axum::response::IntoResponse::into_response(e)),
                };
                let resp = async_graphql_axum::GraphQLResponse(executor.execute_batch(batch.0).await);
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
pub fn typiql_router<Q, M, S>(schema: async_graphql::Schema<Q, M, S>, graphql_path: &str) -> Router
where
    Q: ObjectType + 'static,
    M: ObjectType + 'static,
    S: SubscriptionType + 'static,
{
    let path = graphql_path.to_string();
    Router::new()
        .route_service("/graphql", TypiQLGraphQL { executor: schema })
        .route("/playground", get(move || make_playground(path.clone())))
}
