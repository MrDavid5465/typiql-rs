use async_trait::async_trait;
use duckdb::{params_from_iter, types::Value as DValue, Connection, Row};
use parking_lot::{Mutex, RwLock};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use typiql_core::query::matches_all;
use typiql_core::{is_element_filter, FieldFilter, FilterOp, Join, JoinValue, ListQuery, Location, TypiQLAdapter};

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
/// A table's (column name, column type) pairs.
type Columns = Vec<(String, String)>;
type TableSchemas = HashMap<String, Columns>;
/// Each registered table's key column.
type TableKeys = HashMap<String, String>;

#[derive(Clone)]
pub struct DuckDbAdapter {
    default_path: PathBuf,
    connections: Arc<Mutex<HashMap<PathBuf, Arc<Mutex<Connection>>>>>,
    schemas: Arc<RwLock<TableSchemas>>,
    keys: Arc<RwLock<TableKeys>>,
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
            keys: Arc::new(RwLock::new(HashMap::new())),
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
        // Keys are unique, as Mongo's `_id` is — enforced by DuckDB itself
        // through a unique (ART) index, which also makes key lookups cheap.
        // Also applied to tables created before this index existed; if one
        // already holds duplicate keys the index can't be built, and that's
        // reported rather than silently papered over.
        if let Some(key_field) = self.keys.read().get(table) {
            let sql = format!(
                "CREATE UNIQUE INDEX IF NOT EXISTS \"{0}__key\" ON \"{0}\" (\"{1}\")",
                table, key_field
            );
            if let Err(e) = conn.execute_batch(&sql) {
                eprintln!(
                    "typiql-adapter-duckdb: can't enforce unique `{key_field}` on `{table}` in {}: {e}",
                    path.display()
                );
            }
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
        // Filters that look inside a list can't run against its JSON text
        // column; they're applied to the rows SQL returns instead.
        let (in_memory, sql): (Vec<FieldFilter>, Vec<FieldFilter>) =
            filters.into_iter().partition(is_element_filter);
        let (where_sql, params) = build_where_and(&sql, &columns);
        if in_memory.is_empty() {
            let tail = limit.map(|n| format!(" LIMIT {n}")).unwrap_or_default();
            return query_rows(&conn, &table, &columns, &where_sql, params, &tail);
        }
        let rows = query_rows(&conn, &table, &columns, &where_sql, params, "")
            .into_iter()
            .filter(|row| matches_all(row, &in_memory));
        match limit {
            Some(n) => rows.take(n).collect(),
            None => rows.collect(),
        }
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
        if or_groups.iter().flatten().any(is_element_filter) {
            return query_rows(&conn, &table, &columns, "1 = 1", vec![], "")
                .into_iter()
                .filter(|row| or_groups.iter().any(|g| matches_all(row, g)))
                .collect();
        }
        let (where_sql, params) = build_where_or_groups(&or_groups, &columns);
        query_rows(&conn, &table, &columns, &where_sql, params, "")
    }

    /// Whether every join in `q` can be done in SQL here: the parent lives in
    /// this adapter's main file (where every registered table is), and no
    /// condition needs `in`/`not in` against a parent's (list) field.
    /// Anything else falls back to the in-memory join, which gives the same
    /// results.
    fn can_join_in_sql(&self, location: &Location, q: &ListQuery) -> bool {
        matches!(location, Location::Named(_))
            && q.joins.iter().all(|j| {
                self.schemas.read().contains_key(&j.target)
                    && j.conditions.iter().all(|c| {
                        !(matches!(c.value, JoinValue::Parent(_)) && matches!(c.op, FilterOp::In | FilterOp::NotIn))
                            && !matches!(c.op, FilterOp::AnyIn | FilterOp::NoneIn | FilterOp::ElemMatch)
                    })
            })
    }

    /// Sorting, paging and joins pushed into SQL, matching the reference
    /// (Mongo) semantics `typiql::query` implements in memory: rows without
    /// a value first ascending and last descending, the key as the
    /// tie-break, and — through a has-many join — each row's smallest value
    /// ascending, largest descending. Each join is a correlated subquery
    /// returning a STRUCT (has-one) or a LIST of them (has-many).
    fn query_blocking(&self, location: Location, q: ListQuery) -> Vec<Value> {
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

        let mut params: Vec<DValue> = Vec::new();
        let mut select = vec![select_columns_sql(&columns)];
        // (join name, has-many?, the target table's columns)
        let mut joined: Vec<(String, bool, Columns)> = Vec::new();
        for join in &q.joins {
            self.ensure_table(&path, &join.target, &conn);
            let target_columns = self.columns_for(&join.target);
            let (sql, p) = join_subquery(join, &target_columns);
            select.push(format!("({sql}) AS \"{}\"", join.name));
            params.extend(p);
            joined.push((join.name.clone(), join.many, target_columns));
        }
        let (where_sql, where_params) = build_where_and(&q.filters, &columns);
        params.extend(where_params);

        let has = |f: &str| columns.iter().any(|(n, _)| n == f);
        let mut order: Vec<String> = Vec::new();
        for key in &q.sort {
            let (dir, nulls, pick) = if key.desc {
                ("DESC", "NULLS LAST", "list_max")
            } else {
                ("ASC", "NULLS FIRST", "list_min")
            };
            let expr = match key.field.split_once('.') {
                None if has(&key.field) => format!("q.\"{}\"", key.field),
                // A field the table doesn't have is null on every row —
                // equal everywhere, as it is in memory.
                None => continue,
                Some((join, sub)) => match joined.iter().find(|(n, _, _)| n == join) {
                    Some((_, false, _)) => format!("q.\"{join}\".\"{sub}\""),
                    Some((_, true, _)) => format!("{pick}([x.\"{sub}\" FOR x IN q.\"{join}\"])"),
                    None => continue,
                },
            };
            order.push(format!("{expr} {dir} {nulls}"));
        }
        let mut tail = String::new();
        if !order.is_empty() {
            if has(&q.key_field) {
                order.push(format!("q.\"{}\" ASC", q.key_field));
            }
            tail.push_str(&format!(" ORDER BY {}", order.join(", ")));
        }
        if let Some(limit) = q.limit {
            tail.push_str(&format!(" LIMIT {limit}"));
        }
        if q.skip > 0 {
            tail.push_str(&format!(" OFFSET {}", q.skip));
        }

        let sql = format!(
            "SELECT * FROM (SELECT {} FROM \"{table}\" AS p WHERE {where_sql}) AS q{tail}",
            select.join(", ")
        );
        let Ok(mut stmt) = conn.prepare(&sql) else {
            return vec![];
        };
        let Ok(rows) = stmt.query_map(params_from_iter(params), |row| {
            let mut out = row_to_json(row, &columns);
            if let Value::Object(map) = &mut out {
                for (i, (name, many, target_columns)) in joined.iter().enumerate() {
                    let raw: DValue = row.get(columns.len() + i).unwrap_or(DValue::Null);
                    map.insert(name.clone(), joined_to_json(raw, *many, target_columns));
                }
            }
            Ok(out)
        }) else {
            return vec![];
        };
        rows.filter_map(Result::ok).collect()
    }

    fn count_blocking(&self, location: Location, filters: Vec<FieldFilter>) -> usize {
        if filters.iter().any(is_element_filter) {
            return self.get_many_blocking(location, filters, None).len();
        }
        let (path, table) = self.resolve(&location);
        let Ok(conn_arc) = self.connection_for(&path) else {
            return 0;
        };
        let conn = conn_arc.lock();
        self.ensure_table(&path, &table, &conn);
        let columns = self.columns_for(&table);
        if columns.is_empty() {
            return 0;
        }
        let (where_sql, params) = build_where_and(&filters, &columns);
        let sql = format!("SELECT COUNT(*) FROM \"{table}\" WHERE {where_sql}");
        conn.query_row(&sql, params_from_iter(params), |row| row.get::<_, i64>(0))
            .map(|n| n as usize)
            .unwrap_or(0)
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
            .map_err(|e| key_error(e, key_field, std::slice::from_ref(&values)))?;
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
        for mut v in values {
            assign_id_if_missing(&mut v, key_field);
            out.push(v);
        }
        // All-or-nothing, like `JsonAdapter`: a duplicate key anywhere in
        // the batch rolls the whole batch back.
        conn.execute_batch("BEGIN TRANSACTION").map_err(|e| e.to_string())?;
        let appended = (|| {
            let mut appender = conn.appender(&table)?;
            for v in &out {
                appender.append_row(duckdb::appender_params_from_iter(row_values_for_insert(v, &columns)))?;
            }
            appender.flush()
        })();
        match appended {
            Ok(()) => {
                conn.execute_batch("COMMIT").map_err(|e| e.to_string())?;
                Ok(out)
            }
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                Err(key_error(e, key_field, &out))
            }
        }
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

    async fn query(&self, location: Location, query: ListQuery) -> Vec<Value> {
        if query.filters.iter().any(is_element_filter)
            || (!query.joins.is_empty() && !self.can_join_in_sql(&location, &query))
        {
            return typiql_core::query_in_memory(self, location, query).await;
        }
        let this = self.clone();
        tokio::task::spawn_blocking(move || this.query_blocking(location, query))
            .await
            .unwrap_or_default()
    }

    async fn count(&self, location: Location, filters: Vec<FieldFilter>) -> usize {
        let this = self.clone();
        tokio::task::spawn_blocking(move || this.count_blocking(location, filters))
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

    fn register_type(&self, table: &str, key_field: &str, columns: &[(&'static str, &'static str)]) {
        self.keys.write().insert(table.to_string(), key_field.to_string());
        let owned: Vec<(String, String)> = columns
            .iter()
            .map(|(n, t)| (n.to_string(), t.to_string()))
            .collect();
        self.schemas.write().insert(table.to_string(), owned);
    }
}

/// Reports a unique-index violation the way every adapter does (`Duplicate
/// key …`), so callers — and clients — see the same error whichever adapter
/// serves the type. Other errors pass through unchanged.
fn key_error(e: duckdb::Error, key_field: &str, rows: &[Value]) -> String {
    let msg = e.to_string();
    if !msg.to_lowercase().contains("duplicate key") {
        return msg;
    }
    // DuckDB names the offending key in its message; fall back to the batch.
    let key = rows
        .iter()
        .filter_map(|r| r.get(key_field).and_then(Value::as_str))
        .find(|k| msg.contains(k))
        .unwrap_or("?");
    format!("Duplicate key `{key}`")
}

// ── column type helpers ──────────────────────────────────────────────────

/// `TypiQLType::duckdb_columns()` marks the "nested/enum/Vec, JSON-serialize
/// it" fallback case (see typiql-macros' `duckdb_type_for`) as `"JSON"`
/// rather than `"VARCHAR"`, even though both are stored as a DuckDB VARCHAR
/// column — the distinction only matters to this adapter, to know whether a
/// text column's contents should round-trip as a plain JSON string or be
/// parsed back into a real JSON value/object. `ddl_type` translates the
/// marker to an actual DuckDB column type for `CREATE TABLE`.
///
/// `"ANY"` marks a field of some other named Rust type (an enum, a nested
/// struct): stored as plain text when the value is a JSON string — so a
/// unit enum's column holds `Active`, not `"Active"`, and equality filters
/// on it compare plain text — and as JSON text otherwise, parsed back on
/// read when the text is a JSON object or array.
fn ddl_type(ty: &str) -> String {
    if let Some(rest) = ty.strip_prefix("JSON").or_else(|| ty.strip_prefix("ANY")) {
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
        "ANY" => DValue::Text(match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        }),
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
            if base == "JSON" || (base == "ANY" && (s.starts_with('{') || s.starts_with('['))) {
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
/// (matching `JsonAdapter`'s semantics) rather than
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
        // Mongo semantics (the reference adapter): a row with no value is
        // "not equal" — unlike SQL's `NULL != x`, which is never true.
        FilterOp::Ne => (
            format!("({0} != ? OR {0} IS NULL)", field_sql),
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
        // Applied in memory instead — see `get_many_blocking`.
        FilterOp::AnyIn | FilterOp::NoneIn | FilterOp::ElemMatch => ("1 = 1".to_string(), vec![]),
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
    if positive {
        (format!("{} IN ({})", field_sql, placeholders), params)
    } else {
        // As with `Ne`: a row with no value is "not in" the list (Mongo `$nin`).
        (format!("({0} NOT IN ({1}) OR {0} IS NULL)", field_sql, placeholders), params)
    }
}

fn like_clause(
    field_sql: &str,
    value: &Value,
    positive: bool,
    pattern: impl Fn(&str) -> String,
) -> (String, Vec<DValue>) {
    let needle = value.as_str().unwrap_or("");
    let sql_pattern = pattern(&escape_like(needle));
    // Case-insensitive, as the reference Mongo adapter's `/i` regexes are.
    // A NULL column matches neither form — a regex only ever matches a
    // string, so in Mongo a row with no value is excluded by the negated
    // operators too.
    let op = if positive { "ILIKE" } else { "NOT ILIKE" };
    (
        format!("{} {} ? ESCAPE '\\'", field_sql, op),
        vec![DValue::Text(sql_pattern)],
    )
}

/// One join as a correlated subquery against the outer parent row `p`.
fn join_subquery(join: &Join, target_columns: &[(String, String)]) -> (String, Vec<DValue>) {
    let mut params = Vec::new();
    let mut conditions = Vec::new();
    for c in &join.conditions {
        match &c.value {
            JoinValue::Literal(v) => {
                let (sql, p) = render_filter(
                    &FieldFilter { field: c.field.clone(), op: c.op, value: v.clone() },
                    target_columns,
                );
                conditions.push(sql);
                params.extend(p);
            }
            // A parent without the value has no related rows.
            JoinValue::Parent(parent_field) => {
                let rhs = format!("p.\"{parent_field}\"");
                conditions.push(format!("({rhs} IS NOT NULL AND {})", render_against(c.op, &c.field, &rhs)));
            }
        }
    }
    let where_sql = if conditions.is_empty() { "1 = 1".to_string() } else { conditions.join(" AND ") };
    let fields = target_columns
        .iter()
        .map(|(n, _)| format!("\"{n}\" := t.\"{n}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let has = |f: &str| target_columns.iter().any(|(n, _)| n == f);
    let mut order: Vec<String> = join
        .sort
        .iter()
        .filter(|k| has(&k.field))
        .map(|k| {
            if k.desc {
                format!("t.\"{}\" DESC NULLS LAST", k.field)
            } else {
                format!("t.\"{}\" ASC NULLS FIRST", k.field)
            }
        })
        .collect();
    if has(&join.target_key) {
        order.push(format!("t.\"{}\" ASC", join.target_key));
    }
    let order_sql = if order.is_empty() { String::new() } else { format!(" ORDER BY {}", order.join(", ")) };
    let table = &join.target;
    let sql = if join.many {
        let mut page = String::new();
        if let Some(l) = join.limit {
            page.push_str(&format!(" LIMIT {l}"));
        }
        if join.skip > 0 {
            page.push_str(&format!(" OFFSET {}", join.skip));
        }
        format!(
            "SELECT list(struct_pack({fields}){order_sql}) FROM (SELECT * FROM \"{table}\" AS t WHERE {where_sql}{order_sql}{page}) AS t"
        )
    } else {
        format!(
            "SELECT struct_pack({fields}) FROM \"{table}\" AS t WHERE {where_sql}{order_sql} LIMIT 1 OFFSET {}",
            join.skip
        )
    };
    (sql, params)
}

/// `t."field" <op> rhs`, where `rhs` is a SQL expression (a parent column),
/// with the same semantics as [`render_filter`].
fn render_against(op: FilterOp, field: &str, rhs: &str) -> String {
    let f = format!("t.\"{field}\"");
    let escaped = format!("replace(replace(replace({rhs}, '\\', '\\\\'), '%', '\\%'), '_', '\\_')");
    let like = |negated: bool, pattern: String| {
        format!("{f} {}ILIKE {pattern} ESCAPE '\\'", if negated { "NOT " } else { "" })
    };
    match op {
        FilterOp::Eq => format!("{f} = {rhs}"),
        FilterOp::Ne => format!("({f} != {rhs} OR {f} IS NULL)"),
        FilterOp::Gt => format!("{f} > {rhs}"),
        FilterOp::Gte => format!("{f} >= {rhs}"),
        FilterOp::Lt => format!("{f} < {rhs}"),
        FilterOp::Lte => format!("{f} <= {rhs}"),
        FilterOp::StartsWith => like(false, format!("({escaped} || '%')")),
        FilterOp::NotStartsWith => like(true, format!("({escaped} || '%')")),
        FilterOp::EndsWith => like(false, format!("('%' || {escaped})")),
        FilterOp::NotEndsWith => like(true, format!("('%' || {escaped})")),
        FilterOp::Contains => like(false, format!("('%' || {escaped} || '%')")),
        FilterOp::NotContains => like(true, format!("('%' || {escaped} || '%')")),
        // Excluded by `can_join_in_sql`.
        FilterOp::In | FilterOp::NotIn | FilterOp::AnyIn | FilterOp::NoneIn | FilterOp::ElemMatch => {
            "1 = 0".to_string()
        }
    }
}

/// A joined STRUCT (has-one) or LIST of them (has-many) as JSON, converting
/// each field by the target table's own column types.
fn joined_to_json(raw: DValue, many: bool, target_columns: &[(String, String)]) -> Value {
    let row = |v: DValue| match v {
        DValue::Struct(fields) => {
            let mut map = serde_json::Map::new();
            for (name, value) in fields.iter() {
                let ty = column_type(target_columns, name);
                map.insert(name.clone(), dvalue_to_json(value.clone(), ty));
            }
            Value::Object(map)
        }
        _ => Value::Null,
    };
    match (raw, many) {
        (DValue::List(items), true) => Value::Array(items.into_iter().map(row).collect()),
        (_, true) => Value::Array(Vec::new()),
        (DValue::Null, false) => Value::Null,
        (v, false) => row(v),
    }
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

/// `tail_sql` follows the `WHERE` clause: `ORDER BY` / `LIMIT` / `OFFSET`.
fn query_rows(
    conn: &Connection,
    table: &str,
    columns: &[(String, String)],
    where_sql: &str,
    params: Vec<DValue>,
    tail_sql: &str,
) -> Vec<Value> {
    let sql = format!(
        "SELECT {} FROM \"{}\" WHERE {}{}",
        select_columns_sql(columns),
        table,
        where_sql,
        tail_sql,
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
