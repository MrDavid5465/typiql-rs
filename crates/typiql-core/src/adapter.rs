use async_trait::async_trait;
use serde_json::Value;

use crate::query::{matches_all, sort_and_page, AggregateSpec, ListQuery};

/// The in-memory implementation of [`TypiQLAdapter::query`]: filter via
/// `get_many`, attach each join's related rows (fetched from this same
/// adapter in one `get_many_matching_any` per join), then sort and page.
/// The trait's default; adapters that push some of this into their store
/// can still fall back to it for what they don't.
pub async fn query_in_memory<A: TypiQLAdapter + ?Sized>(
    adapter: &A,
    location: Location,
    query: ListQuery,
) -> Vec<Value> {
    let mut rows = adapter.get_many(location, query.filters.clone()).await;
    for join in &query.joins {
        let candidates = adapter
            .get_many_matching_any(join.target.as_str().into(), join.candidate_groups(&rows))
            .await;
        join.attach(&mut rows, &candidates);
    }
    sort_and_page(rows, &query)
}
use std::path::PathBuf;

/// Where a type's data lives — file = schema (a namespace holding one or more
/// named tables), top-level JSON property = table. `Named` is the common
/// case: resolved via the adapter's own default rules (for `JsonAdapter`,
/// a table of that name inside the shared `data.json`). `At` bypasses that
/// default entirely — an explicit file (independent of the adapter's data
/// dir) and table name, used for per-record-scoped storage (e.g. a
/// dashboard's own `.dashboard.json`, located via a sibling field's value
/// rather than a fixed global location).
///
/// `dyn TypiQLAdapter` trait objects can't have generic methods, so trait
/// methods take `Location` directly rather than `impl Into<Location>` —
/// existing call sites passing a bare collection name need an explicit
/// `.into()` (`"cars".into()`), which the `From<&str>`/`From<String>` impls
/// below make a one-token change.
#[derive(Debug, Clone)]
pub enum Location {
    Named(String),
    At { file: PathBuf, table: String },
}

impl From<&str> for Location {
    fn from(s: &str) -> Self {
        Location::Named(s.to_string())
    }
}

impl From<String> for Location {
    fn from(s: String) -> Self {
        Location::Named(s)
    }
}

impl From<&String> for Location {
    fn from(s: &String) -> Self {
        Location::Named(s.clone())
    }
}

/// Comparison operator for a single filter condition. Generated per-field by
/// `#[typiql_type]` (one GraphQL argument per applicable operator, e.g.
/// `count`/`count_ne`/`count_gt`/... for a numeric field), and reused as the
/// building block for relation-field matching (see `RelationLoader`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum FilterOp {
    Eq,
    Ne,
    Gt,
    Gte,
    Lt,
    Lte,
    In,
    NotIn,
    StartsWith,
    EndsWith,
    NotStartsWith,
    NotEndsWith,
    Contains,
    NotContains,
    /// A list of values contains any of `value` (an array) — C#'s `anyEq`.
    AnyIn,
    /// A list of values contains none of `value` (an array) — C#'s `anyNe`.
    NoneIn,
    /// An element of a list of objects matches: `value` is an
    /// [`crate::query::ElementMatch`] (which element, and the conditions it
    /// must meet — all on that one element).
    ElemMatch,
}

/// One filter condition: `field <op> value`. `In`/`NotIn` expect `value` to
/// be a JSON array; every other operator expects a scalar.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FieldFilter {
    pub field: String,
    pub op: FilterOp,
    pub value: Value,
}

#[async_trait]
pub trait TypiQLAdapter: Send + Sync {
    async fn get_one(&self, location: Location, key_field: &str, key_value: &str) -> Option<Value>;

    /// All filters are AND-ed together.
    async fn get_many(&self, location: Location, filters: Vec<FieldFilter>) -> Vec<Value>;

    /// A sorted, paged read (C#'s `_orderBy`/`_orderBy_desc`/`_start`/
    /// `_limit`). The default filters through `get_many`, then sorts and
    /// pages in memory with the reference (Mongo) ordering; adapters that
    /// can push this into their store should, with identical results.
    async fn query(&self, location: Location, query: ListQuery) -> Vec<Value> {
        query_in_memory(self, location, query).await
    }

    /// An aggregate type's rows (see [`AggregateSpec`]), then `query`'s
    /// filters, sorting and paging applied to them. The default groups the
    /// source's rows in memory; SQL adapters push it into `GROUP BY`.
    async fn aggregate(&self, spec: AggregateSpec, query: ListQuery) -> Vec<Value> {
        let rows = self.get_many(spec.source.as_str().into(), Vec::new()).await;
        let rows: Vec<Value> = spec
            .compute(rows)
            .into_iter()
            .filter(|r| matches_all(r, &query.filters))
            .collect();
        sort_and_page(rows, &query)
    }

    /// How many rows an aggregate type has after `filters`.
    async fn aggregate_count(&self, spec: AggregateSpec, filters: Vec<FieldFilter>) -> usize {
        let query = ListQuery {
            filters,
            ..Default::default()
        };
        self.aggregate(spec, query).await.len()
    }

    /// How many rows match — for `<list>PageInfo`. The default counts
    /// `get_many`'s rows.
    async fn count(&self, location: Location, filters: Vec<FieldFilter>) -> usize {
        self.get_many(location, filters).await.len()
    }

    /// Batched relation lookup: `or_groups` is one AND-group of conditions
    /// per parent row needing a related record, matched against `location`
    /// in a single pass — `(field1=a AND field2=b) OR (field1=c AND
    /// field2=d) OR ...`. This is what lets a list of parents fetching a
    /// relation field avoid N+1 lookups: the caller collects every parent's
    /// condition set across one GraphQL execution and issues exactly one
    /// call here.
    async fn get_many_matching_any(
        &self,
        location: Location,
        or_groups: Vec<Vec<FieldFilter>>,
    ) -> Vec<Value>;

    /// Inserts one row, assigning a UUID key if it has none. A key already
    /// in the table is an error (`Duplicate key …`), never a second row.
    async fn add(
        &self,
        location: Location,
        key_field: &str,
        values: Value,
    ) -> Result<Value, String>;

    /// Bulk insert — appends every row in `values`, auto-assigning a UUID
    /// key for any row missing one (same convention as `add`). Distinct from
    /// `set_table`: this appends to existing rows rather than replacing the
    /// whole table's contents. Default implementation loops over `add` one
    /// row at a time; adapters with a real bulk-load path (e.g. DuckDB's
    /// `Appender`) should override this.
    async fn add_many(
        &self,
        location: Location,
        key_field: &str,
        values: Vec<Value>,
    ) -> Result<Vec<Value>, String> {
        let mut out = Vec::with_capacity(values.len());
        for v in values {
            out.push(self.add(location.clone(), key_field, v).await?);
        }
        Ok(out)
    }

    async fn update(
        &self,
        location: Location,
        key_field: &str,
        key_value: &str,
        update: Value,
    ) -> Option<Value>;

    async fn remove(&self, location: Location, key_field: &str, key_value: &str) -> Option<Value>;

    /// Applies one patch to every row keyed by `keys` (C#'s UpdateMany),
    /// returning the updated rows. The default updates them one at a time;
    /// adapters that can do it in one write should.
    async fn update_many(
        &self,
        location: Location,
        key_field: &str,
        keys: Vec<String>,
        patch: Value,
    ) -> Vec<Value> {
        let mut out = Vec::with_capacity(keys.len());
        for key in keys {
            if let Some(v) = self
                .update(location.clone(), key_field, &key, patch.clone())
                .await
            {
                out.push(v);
            }
        }
        out
    }

    /// Removes every row keyed by `keys` (C#'s RemoveMany), returning them.
    /// The default removes them one at a time.
    async fn remove_many(
        &self,
        location: Location,
        key_field: &str,
        keys: Vec<String>,
    ) -> Vec<Value> {
        let mut out = Vec::with_capacity(keys.len());
        for key in keys {
            if let Some(v) = self.remove(location.clone(), key_field, &key).await {
                out.push(v);
            }
        }
        out
    }

    /// Replaces a table's entire contents. `add`/`update`/`remove` are all
    /// keyed operations, which assumes every row has a stable identifying
    /// field — not true for a "dumb" type's content table (e.g. a
    /// dashboard's own `.dashboard.json`), which always holds at most one
    /// row and has no key field of its own. Used only for that case.
    async fn set_table(&self, location: Location, rows: Vec<Value>);

    /// Called once per registered type at schema-build time (see
    /// `typiql_schema!`'s generated `build_typiql_schema`), before any query
    /// runs — lets an adapter that needs a fixed schema up front (e.g.
    /// DuckDB's `CREATE TABLE`) set one up generically from the type's own
    /// `TypiQLType::duckdb_columns()`, with no per-type hand-written DDL.
    /// Synchronous and default-no-op: most adapters (JSON, and any future
    /// schemaless store) have no fixed schema and never override this.
    /// `key_field` is the type's key (storage name) — keys must be unique,
    /// and an adapter with a fixed schema can enforce that up front (e.g. a
    /// unique index) rather than per write.
    fn register_type(
        &self,
        _table: &str,
        _key_field: &str,
        _columns: &[(&'static str, &'static str)],
    ) {
    }

    /// Called after `register_type` for a type declared with
    /// `#[typiql_type(procedure = "...")]` (C#'s stored-procedure model): its
    /// rows come from running `procedure` with `params` (the key fields'
    /// storage names, in order), and it's read-only. Only meaningful to a
    /// SQL adapter; the default ignores it, so elsewhere the type is an
    /// ordinary table.
    fn register_procedure(&self, _table: &str, _procedure: &str, _params: &[&'static str]) {}
}
