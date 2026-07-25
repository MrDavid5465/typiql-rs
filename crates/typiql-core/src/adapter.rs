use async_trait::async_trait;
use serde_json::Value;
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
}

/// One filter condition: `field <op> value`. `In`/`NotIn` expect `value` to
/// be a JSON array; every other operator expects a scalar.
#[derive(Debug, Clone)]
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
    fn register_type(&self, _table: &str, _columns: &[(&'static str, &'static str)]) {}
}
