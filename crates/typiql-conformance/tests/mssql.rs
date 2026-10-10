//! The conformance suite on SQL Server — needs a server:
//! `TYPIQL_TEST_MSSQL="server=tcp:127.0.0.1,14330;user=sa;password=...;TrustServerCertificate=true"`.
//! Each test creates its own database.

mod mssql {
    typiql_conformance::conformance_suite!(typiql_conformance::mssql_store, requires = "TYPIQL_TEST_MSSQL");
}

/// A relation crossing from SQL Server to JSON.
mod mssql_then_json {
    typiql_conformance::conformance_suite!(typiql_conformance::mssql_then_json_store, requires = "TYPIQL_TEST_MSSQL");
}

/// C#'s stored-procedure model: a type whose rows come from a procedure,
/// its key fields (in order) the parameters; read-only.
mod procedures {
    use std::collections::HashMap;
    use std::sync::Arc;

    use serde_json::json;
    use typiql::{AdapterMap, TypiQLAdapter};
    use typiql_adapter_mssql::MssqlAdapter;

    #[typiql::typiql_type(procedure = "dbo.SalesByRegion", ops(get, list))]
    pub struct SalesSummary {
        #[typiql(key)]
        pub region: String,
        #[typiql(key)]
        pub year: i64,
        pub total: f64,
        pub sales: i64,
    }

    mod schema {
        use super::*;
        typiql::typiql_schema!(SalesSummary);
    }

    #[tokio::test]
    async fn procedure_types_run_their_procedure() {
        let Ok(conn) = std::env::var("TYPIQL_TEST_MSSQL") else {
            eprintln!("skipped: set TYPIQL_TEST_MSSQL to run against SQL Server");
            return;
        };
        let database = format!("typiql_test_{}", uuid::Uuid::new_v4().simple());
        let db = MssqlAdapter::with_database(&conn, &database, true).unwrap();
        db.execute(
            "CREATE TABLE sales (region NVARCHAR(50), year INT, amount FLOAT);
             INSERT INTO sales VALUES ('west', 2024, 10), ('west', 2024, 5.5), ('west', 2023, 1), ('east', 2024, 7);",
        )
        .await
        .unwrap();
        db.execute(
            "CREATE PROCEDURE dbo.SalesByRegion @region NVARCHAR(50), @year INT AS
             SELECT region, year, SUM(amount) AS total, COUNT(*) AS sales FROM sales
             WHERE region = @region AND year = @year GROUP BY region, year",
        )
        .await
        .unwrap();

        let adapter: Arc<dyn TypiQLAdapter> = Arc::new(db.clone());
        let mut adapters: AdapterMap = HashMap::new();
        adapters.insert("default", adapter.clone());
        let schema = schema::build_typiql_schema(adapters);
        let run = |q: &'static str| {
            let schema = schema.clone();
            async move {
                let res = schema.execute(q).await;
                assert!(res.errors.is_empty(), "{q}: {:?}", res.errors);
                res.data.into_json().unwrap()
            }
        };

        // Key fields are the parameters, in order.
        let west = run(r#"{ getSalesSummaries(region: "west", year: 2024) { region year total sales } }"#).await;
        assert_eq!(west["getSalesSummaries"], json!([{ "region": "west", "year": 2024, "total": 15.5, "sales": 2 }]));
        // Other filters don't apply, as in C#.
        let ignored = run(r#"{ getSalesSummaries(region: "west", year: 2024, total_gt: 100) { total } }"#).await;
        assert_eq!(ignored["getSalesSummaries"], json!([{ "total": 15.5 }]));
        let one = run(r#"{ getSalesSummary(region: "east", year: 2024) { total } }"#).await;
        assert_eq!(one["getSalesSummary"], json!({ "total": 7.0 }));
        let info = run(r#"{ getSalesSummariesPageInfo(region: "west", year: 2023) { totalResults } }"#).await;
        assert_eq!(info["getSalesSummariesPageInfo"]["totalResults"], 1);

        // Read-only, and no table was created for it.
        let err = adapter
            .add("sales_summaries".into(), "region", json!({ "region": "x", "year": 1, "total": 0, "sales": 0 }))
            .await
            .unwrap_err();
        assert!(err.contains("read-only"), "{err}");

        drop(schema);
        drop(adapter);
        drop(db);
        MssqlAdapter::drop_database(&conn, &database).await.unwrap();
    }
}
