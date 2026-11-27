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
        let (j, d) = (
            json.items_where(&json_ids, args).await,
            duck.items_where(&duck_ids, args).await,
        );
        if j != d {
            differences.push(format!("  getItems({args}): json {j:?}, duckdb {d:?}"));
        }
    }
    assert!(
        differences.is_empty(),
        "adapters disagree:\n{}",
        differences.join("\n")
    );
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
    db.register_type(
        "parents",
        "id",
        &[
            ("id", "VARCHAR NOT NULL"),
            ("p", "VARCHAR"),
            ("n", "BIGINT"),
        ],
    );
    db.register_type(
        "kids",
        "id",
        &[
            ("id", "VARCHAR NOT NULL"),
            ("k", "VARCHAR"),
            ("n", "BIGINT"),
        ],
    );
    let parents = [
        ("1", Some("ab"), 2),
        ("2", Some("a%_\\"), 5),
        ("3", None, 1),
        ("4", Some("Zz"), 3),
    ];
    for (id, p, n) in parents {
        db.add("parents".into(), "id", json!({ "id": id, "p": p, "n": n }))
            .await
            .unwrap();
    }
    let kids = [
        "ab", "abc", "xab", "AB", "a%_\\x", "ya%_\\", "a_b", "zz", "", "q",
    ];
    for (i, k) in kids.iter().enumerate() {
        db.add(
            "kids".into(),
            "id",
            json!({ "id": format!("k{i}"), "k": k, "n": i }),
        )
        .await
        .unwrap();
    }
    db.add("kids".into(), "id", json!({ "id": "knull", "n": 99 }))
        .await
        .unwrap();

    let ops = [
        FilterOp::Eq,
        FilterOp::Ne,
        FilterOp::Gt,
        FilterOp::Gte,
        FilterOp::Lt,
        FilterOp::Lte,
        FilterOp::StartsWith,
        FilterOp::EndsWith,
        FilterOp::Contains,
        FilterOp::NotStartsWith,
        FilterOp::NotEndsWith,
        FilterOp::NotContains,
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
                conditions: vec![JoinCondition {
                    field: "k".into(),
                    op,
                    value: JoinValue::Parent("p".into()),
                }],
                sort: vec![SortKey {
                    field: "n".into(),
                    desc: true,
                }],
                skip: if many { 1 } else { 0 },
                limit: if many { Some(3) } else { None },
            };
            for sort in [
                vec![SortKey {
                    field: "kids.n".into(),
                    desc: false,
                }],
                vec![SortKey {
                    field: "kids.n".into(),
                    desc: true,
                }],
                vec![SortKey {
                    field: "id".into(),
                    desc: false,
                }],
            ] {
                let q = ListQuery {
                    sort,
                    key_field: "id".into(),
                    joins: vec![join.clone()],
                    skip: 1,
                    limit: Some(2),
                    ..Default::default()
                };
                let sql = db.query("parents".into(), q.clone()).await;
                let mem = typiql::query_in_memory(&db, "parents".into(), q.clone()).await;
                assert_eq!(sql, mem, "{op:?} many={many} sort={:?}", q.sort);
                assert!(
                    !sql.is_empty() || mem.is_empty(),
                    "SQL returned nothing for {op:?}"
                );
            }
        }
    }
}

/// The same join parity on SQL Server (needs `TYPIQL_TEST_MSSQL`): its SQL
/// joins must match the in-memory ones exactly — including `[`, which LIKE
/// treats as a wildcard there.
#[tokio::test]
async fn mssql_sql_joins_match_in_memory_joins() {
    use serde_json::json;
    use typiql::{FilterOp, Join, JoinCondition, JoinValue, ListQuery, SortKey, TypiQLAdapter};

    let Ok(conn) = std::env::var("TYPIQL_TEST_MSSQL") else {
        eprintln!("skipped: set TYPIQL_TEST_MSSQL to run against SQL Server");
        return;
    };
    let database = format!("typiql_test_{}", uuid::Uuid::new_v4().simple());
    let db = typiql_adapter_mssql::MssqlAdapter::with_database(&conn, &database, true).unwrap();
    db.register_type(
        "parents",
        "id",
        &[
            ("id", "VARCHAR NOT NULL"),
            ("p", "VARCHAR"),
            ("n", "BIGINT"),
        ],
    );
    db.register_type(
        "kids",
        "id",
        &[
            ("id", "VARCHAR NOT NULL"),
            ("k", "VARCHAR"),
            ("n", "BIGINT"),
        ],
    );
    let parents = [
        ("1", Some("ab"), 2),
        ("2", Some("a%_\\["), 5),
        ("3", None, 1),
        ("4", Some("Zz"), 3),
    ];
    for (id, p, n) in parents {
        db.add("parents".into(), "id", json!({ "id": id, "p": p, "n": n }))
            .await
            .unwrap();
    }
    let kids = [
        "ab", "abc", "xab", "AB", "a%_\\[x", "ya%_\\[", "a_b", "zz", "", "q", "a[b]",
    ];
    for (i, k) in kids.iter().enumerate() {
        db.add(
            "kids".into(),
            "id",
            json!({ "id": format!("k{i:02}"), "k": k, "n": i }),
        )
        .await
        .unwrap();
    }
    db.add("kids".into(), "id", json!({ "id": "knull", "n": 99 }))
        .await
        .unwrap();

    let ops = [
        FilterOp::Eq,
        FilterOp::Ne,
        FilterOp::Gt,
        FilterOp::Gte,
        FilterOp::Lt,
        FilterOp::Lte,
        FilterOp::StartsWith,
        FilterOp::EndsWith,
        FilterOp::Contains,
        FilterOp::NotStartsWith,
        FilterOp::NotEndsWith,
        FilterOp::NotContains,
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
                conditions: vec![JoinCondition {
                    field: "k".into(),
                    op,
                    value: JoinValue::Parent("p".into()),
                }],
                sort: vec![SortKey {
                    field: "n".into(),
                    desc: true,
                }],
                skip: if many { 1 } else { 0 },
                limit: if many { Some(3) } else { None },
            };
            for sort in [
                vec![SortKey {
                    field: "kids.n".into(),
                    desc: false,
                }],
                vec![SortKey {
                    field: "kids.n".into(),
                    desc: true,
                }],
                vec![SortKey {
                    field: "id".into(),
                    desc: false,
                }],
            ] {
                let q = ListQuery {
                    sort,
                    key_field: "id".into(),
                    joins: vec![join.clone()],
                    skip: 1,
                    limit: Some(2),
                    ..Default::default()
                };
                let sql = db.query("parents".into(), q.clone()).await;
                let mem = typiql::query_in_memory(&db, "parents".into(), q.clone()).await;
                assert_eq!(sql, mem, "{op:?} many={many} sort={:?}", q.sort);
            }
        }
    }
    drop(db);
    typiql_adapter_mssql::MssqlAdapter::drop_database(&conn, &database)
        .await
        .unwrap();
}

/// MongoDB (needs `TYPIQL_TEST_MONGO`): every filter operator run as a Mongo
/// query must select exactly what the in-memory reference does, over values
/// where the two could part — missing and null, case, regex metacharacters,
/// integers vs floats, strings vs numbers, lists — and joins must match the
/// in-memory ones.
#[tokio::test]
async fn mongo_queries_match_in_memory() {
    use serde_json::{json, Value};
    use typiql::{
        FieldFilter, FilterOp, Join, JoinCondition, JoinValue, ListQuery, SortKey, TypiQLAdapter,
    };

    let Ok(uri) = std::env::var("TYPIQL_TEST_MONGO") else {
        eprintln!("skipped: set TYPIQL_TEST_MONGO to run against MongoDB");
        return;
    };
    let database = format!("typiql_test_{}", uuid::Uuid::new_v4().simple());
    let db = typiql_adapter_mongo::MongoAdapter::new(&uri, &database);
    db.register_type(
        "rows",
        "id",
        &[
            ("id", "VARCHAR NOT NULL"),
            ("s", "VARCHAR"),
            ("n", "DOUBLE"),
            ("tags", "JSON"),
        ],
    );
    let rows = [
        json!({ "id": "r0", "s": "ab", "n": 1, "tags": ["x", "y"] }),
        json!({ "id": "r1", "s": "AB", "n": 1.5, "tags": [] }),
        json!({ "id": "r2", "s": "a.b*", "n": -3, "tags": ["y"] }),
        json!({ "id": "r3", "s": null, "n": null, "tags": null }),
        json!({ "id": "r4" }),
        json!({ "id": "r5", "s": "x\nab", "n": 10, "tags": ["z"] }),
        json!({ "id": "r6", "s": "", "n": 0, "tags": ["x"] }),
        json!({ "id": "r7", "s": "Ünïcode", "n": 2.0 }),
        json!({ "id": "r8", "s": "(a)[b]", "n": 1e20 }),
    ];
    for r in &rows {
        db.add("rows".into(), "id", r.clone()).await.unwrap();
    }
    let all = db.get_many("rows".into(), Vec::new()).await;
    assert_eq!(all.len(), rows.len());

    let scalar_ops = [
        FilterOp::Eq,
        FilterOp::Ne,
        FilterOp::Gt,
        FilterOp::Gte,
        FilterOp::Lt,
        FilterOp::Lte,
        FilterOp::StartsWith,
        FilterOp::EndsWith,
        FilterOp::Contains,
        FilterOp::NotStartsWith,
        FilterOp::NotEndsWith,
        FilterOp::NotContains,
    ];
    let values = [
        json!("ab"),
        json!("a.b*"),
        json!("AB"),
        json!(""),
        json!("b"),
        json!("üN"),
        json!("(a)[b]"),
        json!(1),
        json!(1.5),
        json!(0),
        json!(null),
    ];
    let mut differences = Vec::new();
    let mut check = |filters: Vec<FieldFilter>, native: Vec<Value>| {
        let ids = |rows: &[Value]| -> Vec<String> {
            let mut ids: Vec<String> = rows
                .iter()
                .map(|r| r["id"].as_str().unwrap().to_string())
                .collect();
            ids.sort();
            ids
        };
        let reference: Vec<Value> = all
            .iter()
            .filter(|r| typiql::query::matches_all(r, &filters))
            .cloned()
            .collect();
        if ids(&native) != ids(&reference) {
            differences.push(format!(
                "  {filters:?}: mongo {:?}, memory {:?}",
                ids(&native),
                ids(&reference)
            ));
        }
    };
    for op in scalar_ops {
        for field in ["s", "n"] {
            for value in &values {
                let filters = vec![FieldFilter {
                    field: field.into(),
                    op,
                    value: value.clone(),
                }];
                let native = db.get_many("rows".into(), filters.clone()).await;
                check(filters, native);
            }
        }
    }
    for op in [FilterOp::In, FilterOp::NotIn] {
        for value in [
            json!(["ab", 1]),
            json!([]),
            json!([null]),
            json!(["AB", "x\nab"]),
        ] {
            for field in ["s", "n"] {
                let filters = vec![FieldFilter {
                    field: field.into(),
                    op,
                    value: value.clone(),
                }];
                let native = db.get_many("rows".into(), filters.clone()).await;
                check(filters, native);
            }
        }
    }
    for op in [FilterOp::AnyIn, FilterOp::NoneIn] {
        for value in [json!(["x"]), json!(["y", "z"]), json!([]), json!(["nope"])] {
            let filters = vec![FieldFilter {
                field: "tags".into(),
                op,
                value: value.clone(),
            }];
            let native = db.get_many("rows".into(), filters.clone()).await;
            check(filters, native);
        }
    }
    assert!(
        differences.is_empty(),
        "mongo and memory disagree:\n{}",
        differences.join("\n")
    );

    // Joins: Mongo's `$lookup` against the in-memory join.
    db.register_type(
        "parents",
        "id",
        &[
            ("id", "VARCHAR NOT NULL"),
            ("p", "VARCHAR"),
            ("n", "BIGINT"),
        ],
    );
    db.register_type(
        "kids",
        "id",
        &[
            ("id", "VARCHAR NOT NULL"),
            ("k", "VARCHAR"),
            ("n", "BIGINT"),
        ],
    );
    for (id, p, n) in [
        ("1", Some("ab"), 2),
        ("2", Some("a.*\\["), 5),
        ("3", None, 1),
        ("4", Some("Zz"), 3),
    ] {
        db.add("parents".into(), "id", json!({ "id": id, "p": p, "n": n }))
            .await
            .unwrap();
    }
    db.add("parents".into(), "id", json!({ "id": "5", "n": 4 }))
        .await
        .unwrap();
    for (i, k) in [
        "ab", "abc", "xab", "AB", "a.*\\[x", "ya.*\\[", "a_b", "zz", "", "q", "a[b]",
    ]
    .iter()
    .enumerate()
    {
        db.add(
            "kids".into(),
            "id",
            json!({ "id": format!("k{i:02}"), "k": k, "n": i }),
        )
        .await
        .unwrap();
    }
    db.add("kids".into(), "id", json!({ "id": "knull", "n": 99 }))
        .await
        .unwrap();
    for op in scalar_ops {
        for many in [true, false] {
            let join = Join {
                name: "kids".into(),
                graphql_name: "kids".into(),
                target: "kids".into(),
                target_adapter: "default",
                target_key: "id".into(),
                many,
                conditions: vec![JoinCondition {
                    field: "k".into(),
                    op,
                    value: JoinValue::Parent("p".into()),
                }],
                sort: vec![SortKey {
                    field: "n".into(),
                    desc: true,
                }],
                skip: if many { 1 } else { 0 },
                limit: if many { Some(3) } else { None },
            };
            for sort in [
                vec![SortKey {
                    field: "kids.n".into(),
                    desc: false,
                }],
                vec![SortKey {
                    field: "kids.n".into(),
                    desc: true,
                }],
                vec![SortKey {
                    field: "id".into(),
                    desc: false,
                }],
            ] {
                let q = ListQuery {
                    sort,
                    key_field: "id".into(),
                    joins: vec![join.clone()],
                    skip: 1,
                    limit: Some(3),
                    ..Default::default()
                };
                let native = db.query("parents".into(), q.clone()).await;
                let mem = typiql::query_in_memory(&db, "parents".into(), q.clone()).await;
                assert_eq!(native, mem, "{op:?} many={many} sort={:?}", q.sort);
            }
        }
    }
    drop(db);
    typiql_adapter_mongo::MongoAdapter::drop_database(&uri, &database)
        .await
        .unwrap();
}
