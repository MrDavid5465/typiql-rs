//! Queries whose answer rests on a semantics choice (filters over fields
//! that are null on some rows) must give the same answer on every adapter
//! — otherwise moving a type between adapters changes results. Each suite
//! run also checks the expected answers; this compares adapters directly,
//! so a new adapter's divergence shows up side by side.

use typiql_conformance::{cases::NULLABLE_FILTERS, duckdb_store, json_store, Harness};

#[tokio::test]
async fn nullable_filters_agree_across_adapters() {
    let (json, duck) = (Harness::new(json_store()), Harness::new(duckdb_store()));
    let (json_ids, duck_ids) = (json.seed().await, duck.seed().await);
    let mut differences = Vec::new();
    for (args, _) in NULLABLE_FILTERS {
        let (j, d) = (json.items_where(&json_ids, args).await, duck.items_where(&duck_ids, args).await);
        if j != d {
            differences.push(format!("  getItems({args}): json {j:?}, duckdb {d:?}"));
        }
    }
    assert!(differences.is_empty(), "adapters disagree:\n{}", differences.join("\n"));
}

/// Every operator in a join condition against a parent's field: DuckDB's
/// SQL join must return exactly what the in-memory join does, including
/// LIKE metacharacters and backslashes in the parent's value, missing
/// parent values, and sorting / paging by the joined rows.
#[tokio::test]
async fn duckdb_sql_joins_match_in_memory_joins() {
    use serde_json::json;
    use typiql::{FilterOp, Join, JoinCondition, JoinValue, ListQuery, SortKey, TypiQLAdapter};

    let dir = tempfile::tempdir().unwrap();
    let db = typiql_adapter_duckdb::DuckDbAdapter::new(dir.path().join("j.duckdb")).unwrap();
    db.register_type("parents", "id", &[("id", "VARCHAR NOT NULL"), ("p", "VARCHAR"), ("n", "BIGINT")]);
    db.register_type("kids", "id", &[("id", "VARCHAR NOT NULL"), ("k", "VARCHAR"), ("n", "BIGINT")]);
    let parents = [("1", Some("ab"), 2), ("2", Some("a%_\\"), 5), ("3", None, 1), ("4", Some("Zz"), 3)];
    for (id, p, n) in parents {
        db.add("parents".into(), "id", json!({ "id": id, "p": p, "n": n })).await.unwrap();
    }
    let kids = ["ab", "abc", "xab", "AB", "a%_\\x", "ya%_\\", "a_b", "zz", "", "q"];
    for (i, k) in kids.iter().enumerate() {
        db.add("kids".into(), "id", json!({ "id": format!("k{i}"), "k": k, "n": i })).await.unwrap();
    }
    db.add("kids".into(), "id", json!({ "id": "knull", "n": 99 })).await.unwrap();

    let ops = [
        FilterOp::Eq, FilterOp::Ne, FilterOp::Gt, FilterOp::Gte, FilterOp::Lt, FilterOp::Lte,
        FilterOp::StartsWith, FilterOp::EndsWith, FilterOp::Contains,
        FilterOp::NotStartsWith, FilterOp::NotEndsWith, FilterOp::NotContains,
    ];
    for op in ops {
        for many in [true, false] {
            let join = Join {
                name: "kids".into(),
                graphql_name: "kids".into(),
                target: "kids".into(),
                target_adapter: "default",
                target_key: "id".into(),
                many,
                conditions: vec![JoinCondition { field: "k".into(), op, value: JoinValue::Parent("p".into()) }],
                sort: vec![SortKey { field: "n".into(), desc: true }],
                skip: if many { 1 } else { 0 },
                limit: if many { Some(3) } else { None },
            };
            for sort in [
                vec![SortKey { field: "kids.n".into(), desc: false }],
                vec![SortKey { field: "kids.n".into(), desc: true }],
                vec![SortKey { field: "id".into(), desc: false }],
            ] {
                let q = ListQuery { sort, key_field: "id".into(), joins: vec![join.clone()], skip: 1, limit: Some(2), ..Default::default() };
                let sql = db.query("parents".into(), q.clone()).await;
                let mem = typiql::query_in_memory(&db, "parents".into(), q.clone()).await;
                assert_eq!(sql, mem, "{op:?} many={many} sort={:?}", q.sort);
                assert!(!sql.is_empty() || mem.is_empty(), "SQL returned nothing for {op:?}");
            }
        }
    }
}
