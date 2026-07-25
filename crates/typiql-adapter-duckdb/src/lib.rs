use async_trait::async_trait;
use duckdb::{params_from_iter, types::Value as DValue, Connection, Row};
use parking_lot::{Mutex, RwLock};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use typiql_core::{FieldFilter, FilterOp, Location, TypiQLAdapter};

/// DuckDB-backed adapter for typiql — one shared `.duckdb` file holding many
/// named tables (mirroring `JsonAdapter`'s "one file, many tables" model),
/// with the same `Location::At` independent-file override `JsonAdapter`
/// supports. Column schemas are derived entirely at compile time by
/// `#[typiql_type(adapter = "duckdb")]` (see `TypiQLType::duckdb_columns()`)
/// and handed to this adapter once per type via `register_type` — called by
/// `typiql_schema!`'s generated `build_typiql_schema`, before any query
/// runs. `CREATE TABLE IF NOT EXISTS` is then issued lazily, the first time
/// a given (file, table) pair is actually touched.
///
/// The `duckdb` crate's API is fully synchronous, so every `TypiQLAdapter`
/// method wraps its work in `tokio::task::spawn_blocking`.
type TableSchemas = HashMap<String, Vec<(String, String)>>;

#[derive(Clone)]
pub struct DuckDbAdapter {
    default_path: PathBuf,
    connections: Arc<Mutex<HashMap<PathBuf, Arc<Mutex<Connection>>>>>,
    schemas: Arc<RwLock<TableSchemas>>,
    ensured: Arc<Mutex<HashSet<(PathBuf, String)>>>,
}

impl DuckDbAdapter {
    /// Opens (creating if absent) the adapter's default `.duckdb` file.
    /// Eager rather than lazy so a bad path/permission problem surfaces at
    /// startup instead of on the first query.
    pub fn new(db_path: impl AsRef<Path>) -> Result<Self, String> {
        let default_path = db_path.as_ref().to_path_buf();
        let adapter = Self {
            default_path: default_path.clone(),
            connections: Arc::new(Mutex::new(HashMap::new())),
            schemas: Arc::new(RwLock::new(HashMap::new())),
            ensured: Arc::new(Mutex::new(HashSet::new())),
        };
        adapter.connection_for(&default_path)?;
        Ok(adapter)
    }

    /// Same tilde-expansion `JsonAdapter` applies to `Location::At` paths —
    /// those are built from user-facing path strings, not this adapter's
    /// own `default_path`.
    fn expand_tilde(path: &Path) -> PathBuf {
        let s = path.to_string_lossy();
        if let Some(rest) = s.strip_prefix("~/") {
            if let Some(home) = std::env::var_os("HOME") {
                return PathBuf::from(home).join(rest);
            }
        } else if s == "~" {
            if let Some(home) = std::env::var_os("HOME") {
                return PathBuf::from(home);
            }
        }
        path.to_path_buf()
    }

    fn resolve(&self, location: &Location) -> (PathBuf, String) {
        match location {
            Location::Named(table) => (self.default_path.clone(), table.clone()),
            Location::At { file, table } => (Self::expand_tilde(file), table.clone()),
        }
    }

    fn connection_for(&self, path: &Path) -> Result<Arc<Mutex<Connection>>, String> {
        let mut conns = self.connections.lock();
        if let Some(c) = conns.get(path) {
            return Ok(c.clone());
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let conn = Connection::open(path).map_err(|e| e.to_string())?;
        let arc = Arc::new(Mutex::new(conn));
        conns.insert(path.to_path_buf(), arc.clone());
        Ok(arc)
    }

    /// Lazily issues `CREATE TABLE IF NOT EXISTS` the first time a given
    /// (file, table) pair is touched, using the column schema `register_type`
    /// recorded for this table name. A table queried before its type was
    /// ever registered (shouldn't happen via the normal `typiql_schema!`
    /// path) is simply left absent — subsequent reads/writes fail the same
    /// way querying an unknown SQL table would.
    fn ensure_table(&self, path: &Path, table: &str, conn: &Connection) {
        let key = (path.to_path_buf(), table.to_string());
        if self.ensured.lock().contains(&key) {
            return;
        }
        if let Some(columns) = self.schemas.read().get(table).cloned() {
            let cols_sql: Vec<String> = columns
                .iter()
                .map(|(name, ty)| format!("\"{}\" {}", name, ddl_type(ty)))
                .collect();
            let sql = format!(
                "CREATE TABLE IF NOT EXISTS \"{}\" ({})",
                table,
                cols_sql.join(", ")
            );
            let _ = conn.execute_batch(&sql);
        }
        self.ensured.lock().insert(key);
    }

    fn columns_for(&self, table: &str) -> Vec<(String, String)> {
        self.schemas.read().get(table).cloned().unwrap_or_default()
    }

    fn get_many_blocking(
        &self,
        location: Location,
        filters: Vec<FieldFilter>,
        limit: Option<usize>,
    ) -> Vec<Value> {
        let (path, table) = self.resolve(&location);
        let Ok(conn_arc) = self.connection_for(&path) else {
            return vec![];
        };
        let conn = conn_arc.lock();
        self.ensure_table(&path, &table, &conn);
        let columns = self.columns_for(&table);
        if columns.is_empty() {
            return vec![];
        }
        let (where_sql, params) = build_where_and(&filters, &columns);
        query_rows(&conn, &table, &columns, &where_sql, params, limit)
    }

    fn get_many_matching_any_blocking(
        &self,
        location: Location,
        or_groups: Vec<Vec<FieldFilter>>,
    ) -> Vec<Value> {
        if or_groups.is_empty() {
            return vec![];
        }
        let (path, table) = self.resolve(&location);
        let Ok(conn_arc) = self.connection_for(&path) else {
            return vec![];
        };
        let conn = conn_arc.lock();
        self.ensure_table(&path, &table, &conn);
        let columns = self.columns_for(&table);
        if columns.is_empty() {
            return vec![];
        }
        let (where_sql, params) = build_where_or_groups(&or_groups, &columns);
        query_rows(&conn, &table, &columns, &where_sql, params, None)
    }

    fn add_blocking(
        &self,
        location: Location,
        key_field: &str,
        mut values: Value,
    ) -> Result<Value, String> {
        let (path, table) = self.resolve(&location);
        let conn_arc = self.connection_for(&path)?;
        let conn = conn_arc.lock();
        self.ensure_table(&path, &table, &conn);
        let columns = self.columns_for(&table);
        if columns.is_empty() {
            return Err(format!(
                "duckdb adapter: table {:?} has no registered schema — was its type registered via typiql_schema!?",
                table
            ));
        }

        assign_id_if_missing(&mut values, key_field);

        let row_values = row_values_for_insert(&values, &columns);
        let sql = format!(
            "INSERT INTO \"{}\" ({}) VALUES ({})",
            table,
            select_columns_sql(&columns),
            vec!["?"; columns.len()].join(", "),
        );
        conn.execute(&sql, params_from_iter(row_values))
            .map_err(|e| e.to_string())?;
        Ok(values)
    }

    fn add_many_blocking(
        &self,
        location: Location,
        key_field: &str,
        values: Vec<Value>,
    ) -> Result<Vec<Value>, String> {
        let (path, table) = self.resolve(&location);
        let conn_arc = self.connection_for(&path)?;
        let conn = conn_arc.lock();
        self.ensure_table(&path, &table, &conn);
        let columns = self.columns_for(&table);
        if columns.is_empty() {
            return Err(format!(
                "duckdb adapter: table {:?} has no registered schema — was its type registered via typiql_schema!?",
                table
            ));
        }

        let mut out = Vec::with_capacity(values.len());
        {
            let mut appender = conn.appender(&table).map_err(|e| e.to_string())?;
            for mut v in values {
                assign_id_if_missing(&mut v, key_field);
                let row_values = row_values_for_insert(&v, &columns);
                appender
                    .append_row(duckdb::appender_params_from_iter(row_values))
                    .map_err(|e| e.to_string())?;
                out.push(v);
            }
            appender.flush().map_err(|e| e.to_string())?;
        }
        Ok(out)
    }

    fn update_blocking(
        &self,
        location: Location,
        key_field: &str,
        key_value: &str,
        patch: Value,
    ) -> Option<Value> {
        let (path, table) = self.resolve(&location);
        let conn_arc = self.connection_for(&path).ok()?;
        let conn = conn_arc.lock();
        self.ensure_table(&path, &table, &conn);
        let columns = self.columns_for(&table);
        if columns.is_empty() {
            return None;
        }

        let key_ty = column_type(&columns, key_field);
        let key_param = json_to_dvalue(&Value::String(key_value.to_string()), key_ty);

        if let Some(patch_map) = patch.as_object() {
            let mut set_cols: Vec<String> = Vec::new();
            let mut set_params: Vec<DValue> = Vec::new();
            for (name, ty) in &columns {
                if name == key_field {
                    continue;
                }
                if let Some(v) = patch_map.get(name) {
                    set_cols.push(format!("\"{}\" = ?", name));
                    set_params.push(json_to_dvalue(v, base_type(ty)));
                }
            }
            if !set_cols.is_empty() {
                let sql = format!(
                    "UPDATE \"{}\" SET {} WHERE \"{}\" = ?",
                    table,
                    set_cols.join(", "),
                    key_field
                );
                let mut all_params = set_params;
                all_params.push(key_param.clone());
                let changed = conn.execute(&sql, params_from_iter(all_params)).ok()?;
                if changed == 0 {
                    return None;
                }
            }
        }

        let sql = format!(
            "SELECT {} FROM \"{}\" WHERE \"{}\" = ? LIMIT 1",
            select_columns_sql(&columns),
            table,
            key_field,
        );
        conn.query_row(&sql, params_from_iter(vec![key_param]), |row| {
            Ok(row_to_json(row, &columns))
        })
        .ok()
    }

    fn remove_blocking(
        &self,
        location: Location,
        key_field: &str,
        key_value: &str,
    ) -> Option<Value> {
        let (path, table) = self.resolve(&location);
        let conn_arc = self.connection_for(&path).ok()?;
        let conn = conn_arc.lock();
        self.ensure_table(&path, &table, &conn);
        let columns = self.columns_for(&table);
        if columns.is_empty() {
            return None;
        }

        let key_ty = column_type(&columns, key_field);
        let key_param = json_to_dvalue(&Value::String(key_value.to_string()), key_ty);

        let sel_sql = format!(
            "SELECT {} FROM \"{}\" WHERE \"{}\" = ? LIMIT 1",
            select_columns_sql(&columns),
            table,
            key_field,
        );
        let removed = conn
            .query_row(&sel_sql, params_from_iter(vec![key_param.clone()]), |row| {
                Ok(row_to_json(row, &columns))
            })
            .ok()?;

        let del_sql = format!("DELETE FROM \"{}\" WHERE \"{}\" = ?", table, key_field);
        conn.execute(&del_sql, params_from_iter(vec![key_param]))
            .ok()?;

        Some(removed)
    }

    fn set_table_blocking(&self, location: Location, rows: Vec<Value>) {
        let (path, table) = self.resolve(&location);
        let Ok(conn_arc) = self.connection_for(&path) else {
            return;
        };
        let conn = conn_arc.lock();
        self.ensure_table(&path, &table, &conn);
        let columns = self.columns_for(&table);
        if columns.is_empty() {
            return;
        }

        let _ = conn.execute(&format!("DELETE FROM \"{}\"", table), []);
        if rows.is_empty() {
            return;
        }
        let appender_result = conn.appender(&table);
        if let Ok(mut appender) = appender_result {
            for v in &rows {
                let row_values = row_values_for_insert(v, &columns);
                let _ = appender.append_row(duckdb::appender_params_from_iter(row_values));
            }
            let _ = appender.flush();
        }
    }
}

#[async_trait]
impl TypiQLAdapter for DuckDbAdapter {
    async fn get_one(&self, location: Location, key_field: &str, key_value: &str) -> Option<Value> {
        let filters = vec![FieldFilter {
            field: key_field.to_string(),
            op: FilterOp::Eq,
            value: Value::String(key_value.to_string()),
        }];
        let this = self.clone();
        tokio::task::spawn_blocking(move || this.get_many_blocking(location, filters, Some(1)))
            .await
            .unwrap_or_default()
            .into_iter()
            .next()
    }

    async fn get_many(&self, location: Location, filters: Vec<FieldFilter>) -> Vec<Value> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || this.get_many_blocking(location, filters, None))
            .await
            .unwrap_or_default()
    }

    async fn get_many_matching_any(
        &self,
        location: Location,
        or_groups: Vec<Vec<FieldFilter>>,
    ) -> Vec<Value> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || {
            this.get_many_matching_any_blocking(location, or_groups)
        })
        .await
        .unwrap_or_default()
    }

    async fn add(
        &self,
        location: Location,
        key_field: &str,
        values: Value,
    ) -> Result<Value, String> {
        let this = self.clone();
        let key_field = key_field.to_string();
        tokio::task::spawn_blocking(move || this.add_blocking(location, &key_field, values))
            .await
            .map_err(|e| e.to_string())?
    }

    async fn add_many(
        &self,
        location: Location,
        key_field: &str,
        values: Vec<Value>,
    ) -> Result<Vec<Value>, String> {
        let this = self.clone();
        let key_field = key_field.to_string();
        tokio::task::spawn_blocking(move || this.add_many_blocking(location, &key_field, values))
            .await
            .map_err(|e| e.to_string())?
    }

    async fn update(
        &self,
        location: Location,
        key_field: &str,
        key_value: &str,
        update: Value,
    ) -> Option<Value> {
        let this = self.clone();
        let key_field = key_field.to_string();
        let key_value = key_value.to_string();
        tokio::task::spawn_blocking(move || {
            this.update_blocking(location, &key_field, &key_value, update)
        })
        .await
        .ok()
        .flatten()
    }

    async fn remove(&self, location: Location, key_field: &str, key_value: &str) -> Option<Value> {
        let this = self.clone();
        let key_field = key_field.to_string();
        let key_value = key_value.to_string();
        tokio::task::spawn_blocking(move || this.remove_blocking(location, &key_field, &key_value))
            .await
            .ok()
            .flatten()
    }

    async fn set_table(&self, location: Location, rows: Vec<Value>) {
        let this = self.clone();
        let _ = tokio::task::spawn_blocking(move || this.set_table_blocking(location, rows)).await;
    }

    fn register_type(&self, table: &str, columns: &[(&'static str, &'static str)]) {
        let owned: Vec<(String, String)> = columns
            .iter()
            .map(|(n, t)| (n.to_string(), t.to_string()))
            .collect();
        self.schemas.write().insert(table.to_string(), owned);
    }
}

// ── column type helpers ──────────────────────────────────────────────────

/// `TypiQLType::duckdb_columns()` marks the "nested/enum/Vec, JSON-serialize
/// it" fallback case (see typiql-macros' `duckdb_type_for`) as `"JSON"`
/// rather than `"VARCHAR"`, even though both are stored as a DuckDB VARCHAR
/// column — the distinction only matters to this adapter, to know whether a
/// text column's contents should round-trip as a plain JSON string or be
/// parsed back into a real JSON value/object. `ddl_type` translates the
/// marker to an actual DuckDB column type for `CREATE TABLE`.
fn ddl_type(ty: &str) -> String {
    if let Some(rest) = ty.strip_prefix("JSON") {
        format!("VARCHAR{}", rest)
    } else {
        ty.to_string()
    }
}

fn base_type(ty: &str) -> &str {
    ty.strip_suffix(" NOT NULL").unwrap_or(ty)
}

fn column_type<'a>(columns: &'a [(String, String)], name: &str) -> &'a str {
    columns
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, t)| base_type(t))
        .unwrap_or("VARCHAR")
}

fn select_columns_sql(columns: &[(String, String)]) -> String {
    columns
        .iter()
        .map(|(n, _)| format!("\"{}\"", n))
        .collect::<Vec<_>>()
        .join(", ")
}

fn assign_id_if_missing(values: &mut Value, key_field: &str) {
    let needs_id = values
        .get(key_field)
        .and_then(Value::as_str)
        .map(str::is_empty)
        .unwrap_or(true);
    if needs_id {
        if let Value::Object(map) = values {
            map.insert(
                key_field.to_string(),
                Value::String(uuid::Uuid::new_v4().to_string()),
            );
        }
    }
}

// ── JSON <-> DuckDB value conversion ─────────────────────────────────────

fn json_to_dvalue(v: &Value, base: &str) -> DValue {
    if v.is_null() {
        return DValue::Null;
    }
    match base {
        "JSON" => DValue::Text(serde_json::to_string(v).unwrap_or_default()),
        "BOOLEAN" => DValue::Boolean(v.as_bool().unwrap_or(false)),
        "DOUBLE" => DValue::Double(v.as_f64().unwrap_or(0.0)),
        "TINYINT" => DValue::TinyInt(v.as_i64().unwrap_or(0) as i8),
        "SMALLINT" => DValue::SmallInt(v.as_i64().unwrap_or(0) as i16),
        "INTEGER" => DValue::Int(v.as_i64().unwrap_or(0) as i32),
        "BIGINT" => DValue::BigInt(v.as_i64().unwrap_or(0)),
        "HUGEINT" => DValue::HugeInt(v.as_i64().unwrap_or(0) as i128),
        "UTINYINT" => DValue::UTinyInt(v.as_u64().unwrap_or(0) as u8),
        "USMALLINT" => DValue::USmallInt(v.as_u64().unwrap_or(0) as u16),
        "UINTEGER" => DValue::UInt(v.as_u64().unwrap_or(0) as u32),
        "UBIGINT" => DValue::UBigInt(v.as_u64().unwrap_or(0)),
        "UHUGEINT" => DValue::UHugeInt(v.as_u64().unwrap_or(0) as u128),
        // "VARCHAR" and anything unrecognized: plain text, preferring the
        // value's own string form (falling back to its JSON form for a
        // scalar-mismatch, defensive against a filter value that doesn't
        // match the column's declared Rust type).
        _ => DValue::Text(
            v.as_str()
                .map(str::to_string)
                .unwrap_or_else(|| v.to_string()),
        ),
    }
}

fn dvalue_to_json(v: DValue, base: &str) -> Value {
    match v {
        DValue::Null => Value::Null,
        DValue::Boolean(b) => Value::from(b),
        DValue::TinyInt(n) => Value::from(n),
        DValue::SmallInt(n) => Value::from(n),
        DValue::Int(n) => Value::from(n),
        DValue::BigInt(n) => Value::from(n),
        DValue::HugeInt(n) => Value::from(n as i64),
        DValue::UTinyInt(n) => Value::from(n),
        DValue::USmallInt(n) => Value::from(n),
        DValue::UInt(n) => Value::from(n),
        DValue::UBigInt(n) => Value::from(n),
        DValue::UHugeInt(n) => Value::from(n as u64),
        DValue::Float(f) => Value::from(f as f64),
        DValue::Double(f) => Value::from(f),
        DValue::Text(s) => {
            if base == "JSON" {
                serde_json::from_str(&s).unwrap_or(Value::String(s))
            } else {
                Value::from(s)
            }
        }
        _ => Value::Null,
    }
}

fn row_to_json(row: &Row<'_>, columns: &[(String, String)]) -> Value {
    let mut map = serde_json::Map::new();
    for (i, (name, ty)) in columns.iter().enumerate() {
        let raw: DValue = row.get(i).unwrap_or(DValue::Null);
        map.insert(name.clone(), dvalue_to_json(raw, base_type(ty)));
    }
    Value::Object(map)
}

fn row_values_for_insert(values: &Value, columns: &[(String, String)]) -> Vec<DValue> {
    columns
        .iter()
        .map(|(name, ty)| {
            let v = values.get(name).cloned().unwrap_or(Value::Null);
            json_to_dvalue(&v, base_type(ty))
        })
        .collect()
}

// ── filter -> SQL translation ────────────────────────────────────────────

/// Escapes `LIKE`-pattern metacharacters in a filter's needle so
/// `StartsWith`/`EndsWith`/`Contains` behave like plain substring matching
/// (matching `JsonAdapter`'s `str::starts_with`/etc. semantics) rather than
/// letting `%`/`_` in user-provided text act as SQL wildcards.
fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

fn render_filter(f: &FieldFilter, columns: &[(String, String)]) -> (String, Vec<DValue>) {
    let col_ty = column_type(columns, &f.field);
    let field_sql = format!("\"{}\"", f.field);
    match f.op {
        FilterOp::Eq => (
            format!("{} = ?", field_sql),
            vec![json_to_dvalue(&f.value, col_ty)],
        ),
        FilterOp::Ne => (
            format!("{} != ?", field_sql),
            vec![json_to_dvalue(&f.value, col_ty)],
        ),
        FilterOp::Gt => (
            format!("{} > ?", field_sql),
            vec![json_to_dvalue(&f.value, col_ty)],
        ),
        FilterOp::Gte => (
            format!("{} >= ?", field_sql),
            vec![json_to_dvalue(&f.value, col_ty)],
        ),
        FilterOp::Lt => (
            format!("{} < ?", field_sql),
            vec![json_to_dvalue(&f.value, col_ty)],
        ),
        FilterOp::Lte => (
            format!("{} <= ?", field_sql),
            vec![json_to_dvalue(&f.value, col_ty)],
        ),
        FilterOp::In => in_clause(&field_sql, &f.value, col_ty, true),
        FilterOp::NotIn => in_clause(&field_sql, &f.value, col_ty, false),
        FilterOp::StartsWith => like_clause(&field_sql, &f.value, true, |n| format!("{}%", n)),
        FilterOp::NotStartsWith => like_clause(&field_sql, &f.value, false, |n| format!("{}%", n)),
        FilterOp::EndsWith => like_clause(&field_sql, &f.value, true, |n| format!("%{}", n)),
        FilterOp::NotEndsWith => like_clause(&field_sql, &f.value, false, |n| format!("%{}", n)),
        FilterOp::Contains => like_clause(&field_sql, &f.value, true, |n| format!("%{}%", n)),
        FilterOp::NotContains => like_clause(&field_sql, &f.value, false, |n| format!("%{}%", n)),
    }
}

fn in_clause(
    field_sql: &str,
    value: &Value,
    col_ty: &str,
    positive: bool,
) -> (String, Vec<DValue>) {
    let arr = value.as_array().cloned().unwrap_or_default();
    if arr.is_empty() {
        // Empty IN/NOT IN lists aren't valid SQL — an empty `in` never
        // matches anything, an empty `not_in` never excludes anything.
        return (
            (if positive { "1 = 0" } else { "1 = 1" }).to_string(),
            vec![],
        );
    }
    let placeholders = vec!["?"; arr.len()].join(", ");
    let params = arr.iter().map(|v| json_to_dvalue(v, col_ty)).collect();
    let op = if positive { "IN" } else { "NOT IN" };
    (format!("{} {} ({})", field_sql, op, placeholders), params)
}

fn like_clause(
    field_sql: &str,
    value: &Value,
    positive: bool,
    pattern: impl Fn(&str) -> String,
) -> (String, Vec<DValue>) {
    let needle = value.as_str().unwrap_or("");
    let sql_pattern = pattern(&escape_like(needle));
    let op = if positive { "LIKE" } else { "NOT LIKE" };
    (
        format!("{} {} ? ESCAPE '\\'", field_sql, op),
        vec![DValue::Text(sql_pattern)],
    )
}

fn build_where_and(filters: &[FieldFilter], columns: &[(String, String)]) -> (String, Vec<DValue>) {
    if filters.is_empty() {
        return ("1 = 1".to_string(), vec![]);
    }
    let mut clauses = Vec::new();
    let mut params = Vec::new();
    for f in filters {
        let (clause, p) = render_filter(f, columns);
        clauses.push(clause);
        params.extend(p);
    }
    (clauses.join(" AND "), params)
}

fn build_where_or_groups(
    or_groups: &[Vec<FieldFilter>],
    columns: &[(String, String)],
) -> (String, Vec<DValue>) {
    if or_groups.is_empty() {
        return ("1 = 0".to_string(), vec![]);
    }
    let mut group_clauses = Vec::new();
    let mut params = Vec::new();
    for group in or_groups {
        let (clause, p) = build_where_and(group, columns);
        group_clauses.push(format!("({})", clause));
        params.extend(p);
    }
    (group_clauses.join(" OR "), params)
}

fn query_rows(
    conn: &Connection,
    table: &str,
    columns: &[(String, String)],
    where_sql: &str,
    params: Vec<DValue>,
    limit: Option<usize>,
) -> Vec<Value> {
    let limit_sql = limit.map(|n| format!(" LIMIT {}", n)).unwrap_or_default();
    let sql = format!(
        "SELECT {} FROM \"{}\" WHERE {}{}",
        select_columns_sql(columns),
        table,
        where_sql,
        limit_sql,
    );
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return vec![];
    };
    let Ok(rows_iter) = stmt.query_map(params_from_iter(params), |row| {
        Ok(row_to_json(row, columns))
    }) else {
        return vec![];
    };
    rows_iter.filter_map(Result::ok).collect()
}
