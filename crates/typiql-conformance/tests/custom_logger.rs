//! An app's own `TypiQLLogger` replaces the built-in one — entries go there
//! instead of the log type's rows, each error exactly once.

use std::sync::{Arc, Mutex};

use typiql::{LogEntry, TypiQLLogger};
use typiql_conformance::{json_store, schema, user, Harness};

#[derive(Default)]
struct Captured(Mutex<Vec<LogEntry>>);

impl TypiQLLogger for Captured {
    fn log(&self, entry: LogEntry) {
        self.0.lock().unwrap().push(entry);
    }
}

#[tokio::test]
async fn custom_logger_replaces_the_built_in_one() {
    let store = json_store();
    let captured = Arc::new(Captured::default());
    let built = typiql::enable_logging(
        schema::typiql_schema_builder(store.adapters.clone()),
        captured.clone(),
        schema::typiql_operation_kinds(),
    )
    .finish();
    let h = Harness::with_schema(built, store);

    h.run_as(
        &user("alice"),
        r#"mutation { addNote(values: { body: "hi" }) { id } }"#,
    )
    .await;
    let _ = h
        .try_run(None, r#"{ getItems(_orderBy: "nope") { id } }"#)
        .await;

    let entries = captured.0.lock().unwrap().clone();
    let ops: Vec<(&str, bool)> = entries
        .iter()
        .map(|e| (e.operation.as_str(), e.error.is_some()))
        .collect();
    assert_eq!(
        ops,
        [("addNote", false), ("getItems", true)],
        "{entries:#?}"
    );
    assert_eq!(
        h.run("{ getLogs { id } }").await["getLogs"],
        serde_json::json!([]),
        "nothing went to the built-in store"
    );
}
