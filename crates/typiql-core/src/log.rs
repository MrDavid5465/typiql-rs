//! Opt-in logging (C#'s `Logger` / per-function `log` flag).
//!
//! Logging is on when the schema includes a log type — a struct declared
//! with `#[typiql_log(adapter = "…")]`, an ordinary typiql type that's
//! stored in the adapter it names and queryable like any other. What gets
//! logged:
//!
//! - **Every error**, from any root operation — generated or hand-written —
//!   with its arguments (see [`ErrorLogExtension`]). Refusals by `roles`
//!   included.
//! - **Successful operations that opt in**: `ops(add(log), ...)`, every
//!   operation of a `#[typiql_type(log)]` type, and relation fields marked
//!   `#[typiql(log)]` — with their arguments and result.
//!
//! Entries go to a [`TypiQLLogger`]. The built-in one ([`StoreLogger`])
//! writes them as rows of the log type, in the background, so logging never
//! delays a response. An app can register its own instead with
//! [`enable_logging`].

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_graphql::extensions::{
    Extension, ExtensionContext, ExtensionFactory, NextExecute, NextParseQuery,
};
use async_graphql::parser::types::{DocumentOperations, ExecutableDocument, Selection};
use async_graphql::{Context, Response, SchemaBuilder, ServerResult, Variables};
use serde::Serialize;
use serde_json::{json, Value};

use crate::adapter::TypiQLAdapter;
use crate::auth::now_rfc3339;
use crate::auth::CurrentUser;

/// One log entry — the fields of every `#[typiql_log]` type.
#[derive(Debug, Clone, Serialize)]
pub struct LogEntry {
    /// RFC 3339, UTC.
    pub at: String,
    /// The request's user (`TypiQLUser::name`), if any.
    pub user: Option<String>,
    /// The GraphQL field: `getCars`, `addCar`, `Car.owner`, ...
    pub operation: String,
    /// `get`, `list`, `add`, `update`, `remove`, ..., `field` for a
    /// relation field, or `custom` for a hand-written resolver.
    pub kind: String,
    /// The arguments, as JSON text.
    pub arguments: String,
    /// The result, as JSON text — successful entries only.
    pub result: Option<String>,
    /// The error message — failed entries only.
    pub error: Option<String>,
    /// Where in the response the error occurred (`getCars.0.owner`).
    pub path: Option<String>,
    pub duration_ms: Option<f64>,
}

/// Where log entries go. Called on the request path, so it should hand
/// the entry off rather than do slow work itself (the built-in
/// [`StoreLogger`] spawns the write).
pub trait TypiQLLogger: Send + Sync + 'static {
    fn log(&self, entry: LogEntry);
}

/// The built-in logger: each entry becomes a row of the log type, written
/// through its adapter in the background.
pub struct StoreLogger {
    adapter: Arc<dyn TypiQLAdapter>,
    collection: &'static str,
    key_field: &'static str,
}

impl StoreLogger {
    pub fn new(
        adapter: Arc<dyn TypiQLAdapter>,
        collection: &'static str,
        key_field: &'static str,
    ) -> Self {
        StoreLogger {
            adapter,
            collection,
            key_field,
        }
    }
}

impl TypiQLLogger for StoreLogger {
    fn log(&self, entry: LogEntry) {
        let adapter = self.adapter.clone();
        let (collection, key_field) = (self.collection, self.key_field);
        let row = serde_json::to_value(&entry).unwrap_or(Value::Null);
        tokio::spawn(async move {
            if let Err(e) = adapter.add(collection.into(), key_field, row).await {
                eprintln!("typiql: couldn't write a log entry: {e}");
            }
        });
    }
}

/// The active logger, plus each root operation's kind (for error entries).
#[derive(Clone)]
pub struct LogSink {
    logger: Arc<dyn TypiQLLogger>,
    kinds: Arc<HashMap<&'static str, &'static str>>,
}

impl LogSink {
    pub fn new(logger: Arc<dyn TypiQLLogger>, kinds: HashMap<&'static str, &'static str>) -> Self {
        LogSink {
            logger,
            kinds: Arc::new(kinds),
        }
    }
}

/// Turns logging on for a schema being built, with `logger` — a custom one,
/// or the built-in [`StoreLogger`] (which `typiql_schema!` sets up by itself
/// when the schema includes a log type). Calling it again replaces the
/// logger. `kinds` maps root operation names to their kind — pass the
/// schema's `typiql_operation_kinds()`. The error-logging extension is
/// already installed by `typiql_schema_builder`, idle until a logger is set.
pub fn enable_logging<Q, M, S>(
    builder: SchemaBuilder<Q, M, S>,
    logger: Arc<dyn TypiQLLogger>,
    kinds: HashMap<&'static str, &'static str>,
) -> SchemaBuilder<Q, M, S> {
    builder.data(LogSink::new(logger, kinds))
}

/// Logs a successful opted-in operation or field — called by generated
/// resolvers. A no-op unless logging is on.
pub fn log_success<R: Serialize>(
    ctx: &Context<'_>,
    operation: &str,
    kind: &str,
    started: Instant,
    result: &R,
) {
    let Some(sink) = ctx.data_opt::<LogSink>() else {
        return;
    };
    let field = ctx.field();
    let arguments: serde_json::Map<String, Value> = field
        .arguments()
        .unwrap_or_default()
        .into_iter()
        .map(|(name, value)| (name.to_string(), value.into_json().unwrap_or(Value::Null)))
        .collect();
    sink.logger.log(LogEntry {
        at: now_rfc3339(),
        user: ctx.data_opt::<CurrentUser>().map(|u| u.name().to_string()),
        operation: operation.to_string(),
        kind: kind.to_string(),
        arguments: Value::Object(arguments).to_string(),
        result: Some(serde_json::to_string(result).unwrap_or_default()),
        error: None,
        path: None,
        duration_ms: Some(started.elapsed().as_secs_f64() * 1000.0),
    });
}

/// Logs every error in a response, with the root field's arguments. One hook
/// per request (not per field), so it costs nothing measurable on large
/// results.
pub struct ErrorLogExtension;

impl ExtensionFactory for ErrorLogExtension {
    fn create(&self) -> Arc<dyn Extension> {
        Arc::new(ErrorLog::default())
    }
}

#[derive(Default)]
struct ErrorLog {
    parsed: Mutex<Option<(ExecutableDocument, Variables)>>,
}

#[async_trait::async_trait]
impl Extension for ErrorLog {
    async fn parse_query(
        &self,
        ctx: &ExtensionContext<'_>,
        query: &str,
        variables: &Variables,
        next: NextParseQuery<'_>,
    ) -> ServerResult<ExecutableDocument> {
        let document = next.run(ctx, query, variables).await?;
        if ctx.data_opt::<LogSink>().is_some() {
            *self.parsed.lock().unwrap() = Some((document.clone(), variables.clone()));
        }
        Ok(document)
    }

    async fn execute(
        &self,
        ctx: &ExtensionContext<'_>,
        operation_name: Option<&str>,
        next: NextExecute<'_>,
    ) -> Response {
        let started = Instant::now();
        let response = next.run(ctx, operation_name).await;
        let Some(sink) = ctx.data_opt::<LogSink>() else {
            return response;
        };
        if response.errors.is_empty() {
            return response;
        }
        let user = ctx.data_opt::<CurrentUser>().map(|u| u.name().to_string());
        let parsed = self.parsed.lock().unwrap().clone();
        for error in &response.errors {
            let root = error.path.first().and_then(|segment| match segment {
                async_graphql::PathSegment::Field(name) => Some(name.clone()),
                _ => None,
            });
            let (operation, arguments) = match (&root, &parsed) {
                (Some(root), Some((document, variables))) => {
                    root_field(document, variables, operation_name, root)
                }
                (Some(root), None) => (root.clone(), json!({})),
                (None, _) => ("(request)".to_string(), json!({})),
            };
            let path = (!error.path.is_empty()).then(|| {
                error
                    .path
                    .iter()
                    .map(|s| match s {
                        async_graphql::PathSegment::Field(f) => f.clone(),
                        async_graphql::PathSegment::Index(i) => i.to_string(),
                    })
                    .collect::<Vec<_>>()
                    .join(".")
            });
            sink.logger.log(LogEntry {
                at: now_rfc3339(),
                user: user.clone(),
                kind: sink
                    .kinds
                    .get(operation.as_str())
                    .copied()
                    .unwrap_or("custom")
                    .to_string(),
                operation,
                arguments: arguments.to_string(),
                result: None,
                error: Some(error.message.clone()),
                path,
                duration_ms: Some(started.elapsed().as_secs_f64() * 1000.0),
            });
        }
        response
    }
}

/// The root field a response key refers to (by alias or name), and its
/// arguments with variables filled in.
fn root_field(
    document: &ExecutableDocument,
    variables: &Variables,
    operation_name: Option<&str>,
    response_key: &str,
) -> (String, Value) {
    let operation = match &document.operations {
        DocumentOperations::Single(op) => Some(op),
        DocumentOperations::Multiple(ops) => match operation_name {
            Some(name) => ops
                .iter()
                .find(|(n, _)| n.as_str() == name)
                .map(|(_, op)| op),
            None => ops.values().next(),
        },
    };
    let Some(operation) = operation else {
        return (response_key.to_string(), json!({}));
    };
    for selection in &operation.node.selection_set.node.items {
        if let Selection::Field(field) = &selection.node {
            let field = &field.node;
            if field.response_key().node.as_str() != response_key {
                continue;
            }
            let arguments: serde_json::Map<String, Value> = field
                .arguments
                .iter()
                .map(|(name, value)| {
                    let value = value
                        .node
                        .clone()
                        .into_const_with(|var| {
                            Ok::<_, std::convert::Infallible>(
                                variables.get(&var).cloned().unwrap_or_default(),
                            )
                        })
                        .ok()
                        .and_then(|v| v.into_json().ok())
                        .unwrap_or(Value::Null);
                    (name.node.to_string(), value)
                })
                .collect();
            return (field.name.node.to_string(), Value::Object(arguments));
        }
    }
    (response_key.to_string(), json!({}))
}
