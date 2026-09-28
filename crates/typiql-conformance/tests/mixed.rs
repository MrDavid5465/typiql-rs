//! The conformance suite with `Owner` on a different adapter from the types
//! relating to it — relations must cross adapters transparently.

mod json_then_duckdb {
    typiql_conformance::conformance_suite!(typiql_conformance::json_then_duckdb_store);
}

mod duckdb_then_json {
    typiql_conformance::conformance_suite!(typiql_conformance::duckdb_then_json_store);
}
