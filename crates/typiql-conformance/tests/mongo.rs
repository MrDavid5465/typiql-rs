//! The conformance suite on MongoDB — the reference adapter's semantics,
//! checked against a real server: `TYPIQL_TEST_MONGO="mongodb://127.0.0.1:27017"`.
//! Each test creates its own database.

mod mongo {
    typiql_conformance::conformance_suite!(
        typiql_conformance::mongo_store,
        requires = "TYPIQL_TEST_MONGO"
    );
}

/// A relation crossing from MongoDB to JSON.
mod mongo_then_json {
    typiql_conformance::conformance_suite!(
        typiql_conformance::mongo_then_json_store,
        requires = "TYPIQL_TEST_MONGO"
    );
}

/// C#'s keys: a key stored as `_id` is an ObjectId, read and matched as its
/// hex string — including documents C# wrote, and relations (batched and
/// joined, both ways) that hold the hex string on the other side.
mod object_ids {
    use std::collections::HashMap;
    use std::sync::Arc;

    use futures_util::TryStreamExt;
    use mongodb::bson::{doc, oid::ObjectId, Document};
    use serde_json::json;
    use typiql::{AdapterMap, TypiQLAdapter};
    use typiql_adapter_mongo::MongoAdapter;
    use typiql_conformance::{Harness, Store};

    #[tokio::test]
    async fn id_keys_are_object_ids() {
        let Ok(uri) = std::env::var("TYPIQL_TEST_MONGO") else {
            eprintln!("skipped: set TYPIQL_TEST_MONGO to run against MongoDB");
            return;
        };
        let database = format!("typiql_test_{}", uuid::Uuid::new_v4().simple());
        let adapter: Arc<dyn TypiQLAdapter> = Arc::new(MongoAdapter::new(&uri, &database));
        let mut adapters: AdapterMap = HashMap::new();
        adapters.insert("default", adapter.clone());
        adapters.insert("secondary", adapter);
        let h = Harness::new(Store {
            adapters,
            keep_alive: Box::new(()),
        });
        let client = mongodb::Client::with_uri_str(&uri).await.unwrap();
        let owners = client.database(&database).collection::<Document>("owners");

        // Assigned keys are ObjectIds.
        let ids = h.seed().await;
        let stored: Vec<Document> = owners
            .find(doc! {})
            .await
            .unwrap()
            .try_collect()
            .await
            .unwrap();
        assert_eq!(stored.len(), 2);
        for d in &stored {
            let id = d.get_object_id("_id").expect("an ObjectId key");
            assert!([&ids["o1"], &ids["o2"]].contains(&&id.to_hex()));
        }

        // A document as C# wrote it, and an item pointing at it by hex.
        let legacy = ObjectId::new();
        owners
            .insert_one(doc! { "_id": legacy, "DISPLAY_NAME": "Legacy" })
            .await
            .unwrap();
        let hex = legacy.to_hex();
        h.run(&format!(
            r#"mutation {{ addItem(values: {{ name: "Old", count: 7, price: 1.0, active: true, tags: [], size: SMALL, events: [], owner: "{hex}" }}) {{ id }} }}"#
        ))
        .await;

        let owner = h.run(&format!(r#"{{ getOwner(id: "{hex}") {{ id name items {{ name }} itemsJoined {{ name }} }} }}"#)).await;
        assert_eq!(
            owner["getOwner"],
            json!({ "id": hex, "name": "Legacy", "items": [{ "name": "Old" }], "itemsJoined": [{ "name": "Old" }] })
        );
        let items = h.run(&format!(r#"{{ getItems(owner: "{hex}") {{ ownerRecord {{ name }} ownerJoined {{ id name }} }} }}"#)).await;
        assert_eq!(
            items["getItems"],
            json!([{ "ownerRecord": { "name": "Legacy" }, "ownerJoined": { "id": hex, "name": "Legacy" } }])
        );
        let sorted = h
            .run(r#"{ getItems(_orderBy: "ownerJoined.name,name") { name } }"#)
            .await;
        assert_eq!(
            sorted["getItems"][0]["name"], "alphabet_50%",
            "no owner sorts first: {sorted}"
        );
        assert_eq!(sorted["getItems"][1]["name"], "Old", "{sorted}");

        let filtered = h
            .run(&format!(
                r#"{{ getOwners(id_in: ["{hex}", "{}"]) {{ name }} }}"#,
                ids["o1"]
            ))
            .await;
        assert_eq!(
            filtered["getOwners"].as_array().unwrap().len(),
            2,
            "{filtered}"
        );

        let updated = h.run(&format!(r#"mutation {{ updateOwner(id: "{hex}", update: {{ name: "Renamed" }}) {{ id name }} }}"#)).await;
        assert_eq!(
            updated["updateOwner"],
            json!({ "id": hex, "name": "Renamed" })
        );
        let removed = h
            .run(&format!(
                r#"mutation {{ removeOwner(id: "{hex}") {{ name }} }}"#
            ))
            .await;
        assert_eq!(removed["removeOwner"]["name"], "Renamed");
        assert_eq!(
            owners
                .count_documents(doc! { "_id": legacy })
                .await
                .unwrap(),
            0
        );

        drop(h);
        MongoAdapter::drop_database(&uri, &database).await.unwrap();
    }
}
