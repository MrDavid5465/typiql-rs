//! A `TypiQLAdapter` backed by SQL Server — the Rust port of the original C#
//! typiql's SQL adapter (`SqlData.cs`), held to the same GraphQL contract as
//! every other adapter by `typiql-conformance`.
//!
//! Each type is a table (`[dbo].[collection]`), created on first use if it
//! doesn't exist yet (with the key as its primary key, so keys are unique);
//! an existing table is used as it is. As in C#, a field's `data_name` is
//! used as raw SQL: a plain name is a column, anything else (`(SELECT ...)`,
//! `FirstName + ' ' + LastName`) is an expression — selected and filtered
//! on, but never created, inserted or updated.
//!
//! Semantics follow the reference (Mongo) adapter rather than SQL Server's
//! defaults: string equality, comparison and sorting are case-sensitive and
//! by code point (`Latin1_General_BIN2`, where the default collation would
//! ignore case), the pattern operators are case-insensitive, and a missing
//! value matches `ne` / `notIn` (SQL's `NULL <> x` alone wouldn't). Filters
//! inside JSON lists (see `typiql::query::ElementMatch`) are applied in
//! memory after the SQL.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};

use async_trait::async_trait;
use serde_json::{Map, Value};
use tiberius::{Client, ColumnData, Config, Query, Row};
use tokio::net::TcpStream;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};
use typiql_core::query::matches_all;
use typiql_core::{
    is_element_filter, AggregateFn, AggregateSpec, FieldFilter, FilterOp, Join, JoinValue,
    ListQuery, Location, SortKey, TypiQLAdapter,
};

type Conn = Client<Compat<TcpStream>>;

/// Byte-order (code point) collation: case-sensitive equality and Mongo's
/// sort order for strings.
const BINARY: &str = "Latin1_General_BIN2";
/// For the pattern operators, which are case-insensitive (Mongo's `/i`).
const CASE_INSENSITIVE: &str = "Latin1_General_CI_AS";
/// SQL Server allows 2,100 parameters per statement; stay well under.
const MAX_PARAMS: usize = 2000;

#[derive(Clone)]
pub struct MssqlAdapter {
    inner: Arc<Inner>,
}

struct Inner {
    config: Config,
    database: Option<String>,
    create_database: bool,
    database_ready: tokio::sync::OnceCell<()>,
    pool: Mutex<Vec<Conn>>,
    tables: RwLock<HashMap<String, Table>>,
    ensured: tokio::sync::Mutex<HashSet<String>>,
}

#[derive(Clone)]
struct Table {
    key: String,
    columns: Vec<Col>,
    /// C#'s stored-procedure model: rows come from running this procedure
    /// with these parameters (key fields' storage names, in order).
    procedure: Option<(String, Vec<String>)>,
}

#[derive(Clone)]
struct Col {
    /// The storage name (`data_name`): a column, or a SQL expression.
    name: String,
    /// typiql's column type marker (`VARCHAR NOT NULL`, `BIGINT`, `JSON`...).
    ty: String,
}

impl Col {
    fn is_expression(&self) -> bool {
        !is_identifier(&self.name)
    }
    fn base(&self) -> &str {
        self.ty.strip_suffix(" NOT NULL").unwrap_or(&self.ty)
    }
    fn is_text(&self) -> bool {
        matches!(self.base(), "VARCHAR" | "ANY")
    }
    fn is_integer(&self) -> bool {
        matches!(
            self.base(),
            "TINYINT"
                | "SMALLINT"
                | "INTEGER"
                | "BIGINT"
                | "UTINYINT"
                | "USMALLINT"
                | "UINTEGER"
                | "UBIGINT"
        )
    }
}

impl Table {
    fn col(&self, name: &str) -> Option<&Col> {
        self.columns.iter().find(|c| c.name == name)
    }
    /// Columns that exist in the table (not expressions).
    fn stored(&self) -> impl Iterator<Item = &Col> {
        self.columns.iter().filter(|c| !c.is_expression())
    }
    /// Whether the key is an integer — an `IDENTITY` column the server
    /// numbers itself.
    fn integer_key(&self) -> bool {
        self.col(&self.key).is_some_and(Col::is_integer)
    }
}

impl MssqlAdapter {
    /// Connects lazily, with an ADO.NET-style connection string
    /// (`server=tcp:host,1433;user=...;password=...;database=...;TrustServerCertificate=true`).
    pub fn new(connection_string: &str) -> Result<Self, String> {
        let config = Config::from_ado_string(connection_string).map_err(|e| e.to_string())?;
        Ok(Self::from_config(config, None, false))
    }

    /// Like [`MssqlAdapter::new`], connecting to `database`, which is created
    /// (from the server's `master`) if it doesn't exist — for tests and new
    /// deployments.
    pub fn with_database(
        connection_string: &str,
        database: &str,
        create_if_missing: bool,
    ) -> Result<Self, String> {
        let mut config = Config::from_ado_string(connection_string).map_err(|e| e.to_string())?;
        config.database(database);
        Ok(Self::from_config(
            config,
            Some(database.to_string()),
            create_if_missing,
        ))
    }

    /// Drops `database` (closing any open connections to it) — for removing
    /// throwaway databases, e.g. after tests.
    pub async fn drop_database(connection_string: &str, database: &str) -> Result<(), String> {
        let mut config = Config::from_ado_string(connection_string).map_err(|e| e.to_string())?;
        config.database("master");
        let mut c = Self::open(config).await?;
        let sql = format!(
            "IF DB_ID(N'{0}') IS NOT NULL BEGIN ALTER DATABASE {1} SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE {1}; END",
            database.replace('\'', "''"),
            bracket(database)
        );
        run(&mut c, &sql, vec![]).await
    }

    fn from_config(config: Config, database: Option<String>, create_database: bool) -> Self {
        MssqlAdapter {
            inner: Arc::new(Inner {
                config,
                database,
                create_database,
                database_ready: tokio::sync::OnceCell::new(),
                pool: Mutex::new(Vec::new()),
                tables: RwLock::new(HashMap::new()),
                ensured: tokio::sync::Mutex::new(HashSet::new()),
            }),
        }
    }

    // ── connections ────────────────────────────────────────────────────────

    async fn open(config: Config) -> Result<Conn, String> {
        let tcp = TcpStream::connect(config.get_addr())
            .await
            .map_err(|e| e.to_string())?;
        tcp.set_nodelay(true).ok();
        Client::connect(config, tcp.compat_write())
            .await
            .map_err(|e| e.to_string())
    }

    async fn conn(&self) -> Result<Pooled, String> {
        let inner = &self.inner;
        if inner.create_database {
            inner
                .database_ready
                .get_or_try_init(|| async {
                    let db = inner.database.clone().unwrap_or_default();
                    let mut master = inner.config.clone();
                    master.database("master");
                    let mut c = Self::open(master).await?;
                    let sql = format!(
                        "IF DB_ID(N'{0}') IS NULL CREATE DATABASE {1}",
                        db.replace('\'', "''"),
                        bracket(&db)
                    );
                    c.simple_query(sql)
                        .await
                        .map_err(|e| e.to_string())?
                        .into_results()
                        .await
                        .map_err(|e| e.to_string())?;
                    Ok::<(), String>(())
                })
                .await?;
        }
        let pooled = inner.pool.lock().unwrap().pop();
        let conn = match pooled {
            Some(c) => c,
            None => Self::open(inner.config.clone()).await?,
        };
        Ok(Pooled {
            conn: Some(conn),
            adapter: self.clone(),
            healthy: true,
        })
    }

    fn table(&self, location: &Location) -> Option<(String, Table)> {
        let name = match location {
            Location::Named(t) => t.clone(),
            Location::At { table, .. } => table.clone(),
        };
        let table = self.inner.tables.read().unwrap().get(&name).cloned()?;
        Some((name, table))
    }

    /// Creates the table on first use if it doesn't exist; an existing table
    /// is used as it is.
    async fn ensure(&self, c: &mut Conn, name: &str, table: &Table) -> Result<(), String> {
        if table.procedure.is_some() {
            // Served by a stored procedure: there's no table to create.
            return Ok(());
        }
        let mut ensured = self.inner.ensured.lock().await;
        if ensured.contains(name) {
            return Ok(());
        }
        let columns: Vec<String> = table
            .stored()
            .map(|col| {
                let key = col.name == table.key;
                let mut ty = sql_type(col.base(), key && !col.is_integer()).to_string();
                if key && col.is_integer() {
                    ty = "BIGINT IDENTITY(1, 1) NOT NULL PRIMARY KEY".to_string();
                } else if key {
                    ty.push_str(" NOT NULL PRIMARY KEY");
                } else if col.ty.ends_with(" NOT NULL") {
                    ty.push_str(" NOT NULL");
                }
                format!("{} {ty}", bracket(&col.name))
            })
            .collect();
        let sql = format!(
            "IF OBJECT_ID(N'dbo.{0}', N'U') IS NULL CREATE TABLE [dbo].{1} ({2})",
            name.replace('\'', "''"),
            bracket(name),
            columns.join(", ")
        );
        run(c, &sql, vec![]).await?;
        ensured.insert(name.to_string());
        Ok(())
    }

    /// Rows matching `filters`, with `tail` (`ORDER BY` / `OFFSET`) after.
    async fn select(
        &self,
        location: &Location,
        filters: Vec<FieldFilter>,
        tail: &str,
    ) -> Result<Vec<Value>, String> {
        let q = ListQuery {
            filters,
            ..Default::default()
        };
        self.select_with_tail(location, &q, tail, Vec::new()).await
    }

    /// Runs a raw T-SQL batch on this adapter's database — for an app's own
    /// migrations (stored procedures, views, indexes) and test setup.
    pub async fn execute(&self, sql: &str) -> Result<(), String> {
        let mut c = self.conn().await?;
        run(&mut c, sql, vec![]).await
    }

    /// C#'s stored-procedure model: `EXEC proc @P1, ...`, each parameter the
    /// value of an equality filter on that key field (NULL if there's none).
    /// Other filters, sorting and paging don't apply, as in C#.
    async fn exec_procedure(
        &self,
        table: &Table,
        procedure: &str,
        proc_params: &[String],
        filters: &[FieldFilter],
    ) -> Result<Vec<Value>, String> {
        let mut params = Vec::new();
        let args: Vec<String> = proc_params
            .iter()
            .map(|p| {
                let value = filters
                    .iter()
                    .find(|f| &f.field == p && f.op == FilterOp::Eq)
                    .map(|f| &f.value)
                    .unwrap_or(&Value::Null);
                let col = table.col(p).cloned().unwrap_or(Col {
                    name: p.clone(),
                    ty: "VARCHAR".into(),
                });
                param(&mut params, to_param(value, &col))
            })
            .collect();
        let name: Vec<String> = procedure.split('.').map(bracket).collect();
        let sql = format!("EXEC {} {}", name.join("."), args.join(", "));
        let mut c = self.conn().await?;
        let rows = query(&mut c, &sql, params)
            .await
            .inspect_err(|_| c.healthy = false)?;
        Ok(rows.iter().map(|r| row_json(r, table)).collect())
    }

    /// An aggregate type in SQL: the source grouped (`GROUP BY`, strings by
    /// code point) into the aggregate's rows, as a derived table `q` with
    /// its own columns — so filters, sorting and paging apply as to any
    /// table. Semantics match `AggregateSpec::compute`: a sum of nothing is
    /// 0, AVG is over floats (T-SQL's AVG of integers truncates), and an
    /// empty source has no rows even ungrouped.
    fn aggregate_sql(&self, spec: &AggregateSpec) -> Option<(String, Table)> {
        let source = self
            .inner
            .tables
            .read()
            .unwrap()
            .get(&spec.source)
            .cloned()?;
        let fallback = Col {
            name: String::new(),
            ty: "VARCHAR".into(),
        };
        let src = |f: &str| {
            source.col(f).cloned().unwrap_or_else(|| Col {
                name: f.to_string(),
                ..fallback.clone()
            })
        };
        let numeric = |c: &Col| c.is_integer() || c.base() == "DOUBLE";
        let mut select = Vec::new();
        let mut group_by = Vec::new();
        let mut columns = Vec::new();
        for (out, field) in &spec.groups {
            let col = src(field);
            let expr = sortable(&col_sql(&col.name, "p"), &col);
            select.push(format!("{expr} AS {}", bracket(out)));
            group_by.push(expr);
            columns.push(Col {
                name: out.clone(),
                ty: col.base().to_string(),
            });
        }
        for m in &spec.measures {
            let col = src(m.field.as_deref().unwrap_or_default());
            let c = col_sql(&col.name, "p");
            let (expr, ty) = match m.func {
                AggregateFn::Count => ("COUNT_BIG(*)".to_string(), "BIGINT"),
                AggregateFn::CountOf => (format!("COUNT_BIG({c})"), "BIGINT"),
                AggregateFn::Sum if !numeric(&col) => ("CAST(0 AS BIGINT)".to_string(), "BIGINT"),
                AggregateFn::Sum if col.is_integer() => (
                    format!("CAST(COALESCE(SUM(CAST({c} AS BIGINT)), 0) AS BIGINT)"),
                    "BIGINT",
                ),
                AggregateFn::Sum => (format!("COALESCE(SUM({c}), 0)"), "DOUBLE"),
                AggregateFn::Avg if !numeric(&col) => ("CAST(NULL AS FLOAT)".to_string(), "DOUBLE"),
                AggregateFn::Avg => (format!("AVG(CAST({c} AS FLOAT))"), "DOUBLE"),
                AggregateFn::Min => (format!("MIN({})", sortable(&c, &col)), col.base()),
                AggregateFn::Max => (format!("MAX({})", sortable(&c, &col)), col.base()),
            };
            select.push(format!("{expr} AS {}", bracket(&m.name)));
            columns.push(Col {
                name: m.name.clone(),
                ty: ty.to_string(),
            });
        }
        let tail = if group_by.is_empty() {
            " HAVING COUNT_BIG(*) > 0".to_string()
        } else {
            format!(" GROUP BY {}", group_by.join(", "))
        };
        let key = spec
            .groups
            .first()
            .map(|(o, _)| o.clone())
            .or_else(|| spec.measures.first().map(|m| m.name.clone()))
            .unwrap_or_default();
        let sql = format!(
            "SELECT {} FROM [dbo].{} AS p{tail}",
            select.join(", "),
            bracket(&spec.source)
        );
        Some((
            sql,
            Table {
                key,
                columns,
                procedure: None,
            },
        ))
    }

    async fn run_aggregate(
        &self,
        spec: &AggregateSpec,
        q: &ListQuery,
        count_only: bool,
    ) -> Result<(Vec<Value>, usize), String> {
        let Some((_, source)) = self.table(&spec.source.as_str().into()) else {
            return Ok((Vec::new(), 0));
        };
        let mut c = self.conn().await?;
        self.ensure(&mut c, &spec.source, &source).await?;
        let Some((inner, table)) = self.aggregate_sql(spec) else {
            return Ok((Vec::new(), 0));
        };
        let mut params = Vec::new();
        let where_sql = where_and(&q.filters, &table, "q", &mut params);
        if count_only {
            let sql = format!("SELECT COUNT_BIG(*) FROM ({inner}) AS q WHERE {where_sql}");
            let rows = query(&mut c, &sql, params).await?;
            return Ok((
                Vec::new(),
                rows.first().and_then(|r| r.get::<i64, _>(0)).unwrap_or(0) as usize,
            ));
        }
        let mut sort_params = Vec::new();
        let tail = self.order_sql(q, &table, &[], &mut sort_params);
        let offset = params.len();
        let tail = renumber(&tail, sort_params.len(), offset);
        params.extend(sort_params);
        let sql = format!("SELECT * FROM ({inner}) AS q WHERE {where_sql}{tail}");
        let rows = query(&mut c, &sql, params)
            .await
            .inspect_err(|_| c.healthy = false)?;
        Ok((rows.iter().map(|r| row_json(r, &table)).collect(), 0))
    }

    /// Writes to a procedure-served type are refused: it's read-only.
    fn writable(table: &Table, name: &str) -> Result<(), String> {
        match &table.procedure {
            Some((procedure, _)) => Err(format!(
                "`{name}` is read-only: its rows come from stored procedure {procedure}"
            )),
            None => Ok(()),
        }
    }

    /// Everything a query can do in SQL here; otherwise the in-memory path.
    fn sql_can(&self, q: &ListQuery) -> bool {
        !q.filters.iter().any(is_element_filter)
            && q.joins.iter().all(|j| {
                self.inner.tables.read().unwrap().contains_key(&j.target)
                    && j.conditions.iter().all(|c| {
                        !is_element_op(c.op)
                            && !(matches!(c.value, JoinValue::Parent(_))
                                && matches!(c.op, FilterOp::In | FilterOp::NotIn))
                    })
            })
    }

    fn order_sql(
        &self,
        q: &ListQuery,
        table: &Table,
        joins: &[Join],
        params: &mut Vec<Param>,
    ) -> String {
        let mut order: Vec<String> = Vec::new();
        for key in &q.sort {
            let dir = if key.desc { "DESC" } else { "ASC" };
            match key.field.split_once('.') {
                None => {
                    let Some(col) = table.col(&key.field) else {
                        continue;
                    };
                    order.push(format!(
                        "{} {dir}",
                        sortable(&format!("q.{}", bracket(&col.name)), col)
                    ));
                }
                Some((join_name, sub)) => {
                    let Some(join) = joins.iter().find(|j| j.name == join_name) else {
                        continue;
                    };
                    let Some(target) = self.inner.tables.read().unwrap().get(&join.target).cloned()
                    else {
                        continue;
                    };
                    let Some(col) = target.col(sub) else { continue };
                    let expr = join_sort_sql(join, &target, col, key.desc, "q", params);
                    order.push(format!("{expr} {dir}"));
                }
            }
        }
        if !order.is_empty() || q.skip > 0 || q.limit.is_some() {
            order.push(format!("q.{} ASC", bracket(&q.key_field)));
        }
        let mut tail = String::new();
        if !order.is_empty() {
            tail.push_str(&format!(" ORDER BY {}", order.join(", ")));
        }
        if q.skip > 0 || q.limit.is_some() {
            tail.push_str(&format!(" OFFSET {} ROWS", q.skip));
            if let Some(l) = q.limit {
                tail.push_str(&format!(" FETCH NEXT {l} ROWS ONLY"));
            }
        }
        tail
    }

    async fn write(
        &self,
        location: &Location,
        build: impl FnOnce(&str, &Table, &mut Vec<Param>) -> Option<String>,
    ) -> Result<Vec<Value>, String> {
        let Some((name, table)) = self.table(location) else {
            return Err(format!(
                "mssql adapter: no registered type for table {location:?}"
            ));
        };
        Self::writable(&table, &name)?;
        let mut c = self.conn().await?;
        self.ensure(&mut c, &name, &table).await?;
        let mut params = Vec::new();
        let Some(sql) = build(&name, &table, &mut params) else {
            return Ok(Vec::new());
        };
        let rows = query(&mut c, &sql, params)
            .await
            .inspect_err(|_| c.healthy = false)?;
        Ok(rows.iter().map(|r| row_json(r, &table)).collect())
    }

    async fn insert_many(
        &self,
        location: &Location,
        key_field: &str,
        mut values: Vec<Value>,
    ) -> Result<Vec<Value>, String> {
        let Some((name, table)) = self.table(location) else {
            return Err(format!(
                "mssql adapter: no registered type for table {location:?}"
            ));
        };
        Self::writable(&table, &name)?;
        let identity = table.integer_key();
        if !identity {
            for v in &mut values {
                assign_key(v, key_field);
            }
        }
        let mut c = self.conn().await?;
        self.ensure(&mut c, &name, &table).await?;
        // All-or-nothing: one duplicate key rolls back the whole batch.
        run(&mut c, "BEGIN TRANSACTION", vec![]).await?;
        let mut out = Vec::with_capacity(values.len());
        // An integer key is an IDENTITY column: rows without a key leave it
        // to the server; rows with one (a keyed upsert) need IDENTITY_INSERT.
        // Consecutive rows of each kind go in together.
        let has_key = |v: &Value| !matches!(v.get(key_field), None | Some(Value::Null));
        let mut start = 0;
        while start < values.len() {
            let explicit = !identity || has_key(&values[start]);
            let end = (start..values.len())
                .find(|&i| (!identity || has_key(&values[i])) != explicit)
                .unwrap_or(values.len());
            let group = &values[start..end];
            start = end;
            let columns: Vec<&Col> = table
                .stored()
                .filter(|c| explicit || c.name != table.key)
                .collect();
            let identity_insert = identity && explicit;
            if identity_insert {
                run(
                    &mut c,
                    &format!("SET IDENTITY_INSERT [dbo].{} ON", bracket(&name)),
                    vec![],
                )
                .await?;
            }
            let per_chunk = (MAX_PARAMS / columns.len().max(1)).clamp(1, 1000);
            for chunk in group.chunks(per_chunk) {
                let mut params = Vec::new();
                let rows: Vec<String> = chunk
                    .iter()
                    .map(|v| {
                        let cells: Vec<String> = columns
                            .iter()
                            .map(|col| {
                                param(
                                    &mut params,
                                    to_param(v.get(&col.name).unwrap_or(&Value::Null), col),
                                )
                            })
                            .collect();
                        format!("({})", cells.join(", "))
                    })
                    .collect();
                let sql = format!(
                    "INSERT INTO [dbo].{} ({}) OUTPUT {} VALUES {}",
                    bracket(&name),
                    columns
                        .iter()
                        .map(|c| bracket(&c.name))
                        .collect::<Vec<_>>()
                        .join(", "),
                    output_columns(&table, "INSERTED"),
                    rows.join(", ")
                );
                match query(&mut c, &sql, params).await {
                    Ok(rows) => out.extend(rows.iter().map(|r| row_json(r, &table))),
                    Err(e) => {
                        let _ = run(&mut c, "IF @@TRANCOUNT > 0 ROLLBACK", vec![]).await;
                        if identity_insert {
                            let _ = run(
                                &mut c,
                                &format!("SET IDENTITY_INSERT [dbo].{} OFF", bracket(&name)),
                                vec![],
                            )
                            .await;
                        }
                        return Err(key_error(&e));
                    }
                }
            }
            if identity_insert {
                run(
                    &mut c,
                    &format!("SET IDENTITY_INSERT [dbo].{} OFF", bracket(&name)),
                    vec![],
                )
                .await?;
            }
        }
        run(&mut c, "COMMIT", vec![]).await?;
        Ok(out)
    }
}

/// A pooled connection, returned to the pool when dropped — unless a query
/// on it failed, in which case it's discarded.
struct Pooled {
    conn: Option<Conn>,
    adapter: MssqlAdapter,
    healthy: bool,
}

impl std::ops::Deref for Pooled {
    type Target = Conn;
    fn deref(&self) -> &Conn {
        self.conn.as_ref().unwrap()
    }
}

impl std::ops::DerefMut for Pooled {
    fn deref_mut(&mut self) -> &mut Conn {
        self.conn.as_mut().unwrap()
    }
}

impl Drop for Pooled {
    fn drop(&mut self) {
        if let (Some(c), true) = (self.conn.take(), self.healthy) {
            self.adapter.inner.pool.lock().unwrap().push(c);
        }
    }
}

#[async_trait]
impl TypiQLAdapter for MssqlAdapter {
    async fn get_one(&self, location: Location, key_field: &str, key_value: &str) -> Option<Value> {
        let f = FieldFilter {
            field: key_field.to_string(),
            op: FilterOp::Eq,
            value: Value::String(key_value.to_string()),
        };
        self.select(
            &location,
            vec![f],
            " ORDER BY (SELECT NULL) OFFSET 0 ROWS FETCH NEXT 1 ROWS ONLY",
        )
        .await
        .ok()
        .and_then(|rows| rows.into_iter().next())
    }

    async fn get_many(&self, location: Location, filters: Vec<FieldFilter>) -> Vec<Value> {
        let (in_memory, sql): (Vec<FieldFilter>, Vec<FieldFilter>) =
            filters.into_iter().partition(is_element_filter);
        let rows = self.select(&location, sql, "").await.unwrap_or_else(|e| {
            eprintln!("typiql-adapter-mssql: {e}");
            Vec::new()
        });
        rows.into_iter()
            .filter(|r| matches_all(r, &in_memory))
            .collect()
    }

    async fn get_many_matching_any(
        &self,
        location: Location,
        or_groups: Vec<Vec<FieldFilter>>,
    ) -> Vec<Value> {
        if or_groups.is_empty() {
            return Vec::new();
        }
        if or_groups.iter().flatten().any(is_element_filter) {
            let all = self.get_many(location, Vec::new()).await;
            return all
                .into_iter()
                .filter(|r| or_groups.iter().any(|g| matches_all(r, g)))
                .collect();
        }
        let Some((name, table)) = self.table(&location) else {
            return Vec::new();
        };
        if let Some((procedure, proc_params)) = &table.procedure {
            let mut out = Vec::new();
            for group in &or_groups {
                match self
                    .exec_procedure(&table, procedure, proc_params, group)
                    .await
                {
                    Ok(rows) => out.extend(rows),
                    Err(e) => eprintln!("typiql-adapter-mssql: {e}"),
                }
            }
            return out;
        }
        let result = async {
            let mut c = self.conn().await?;
            self.ensure(&mut c, &name, &table).await?;
            let mut params = Vec::new();
            let groups: Vec<String> = or_groups
                .iter()
                .map(|g| format!("({})", where_and(g, &table, "p", &mut params)))
                .collect();
            let sql = format!(
                "SELECT {} FROM [dbo].{} AS p WHERE {}",
                select_columns(&table, "p"),
                bracket(&name),
                groups.join(" OR ")
            );
            let rows = query(&mut c, &sql, params).await?;
            Ok::<_, String>(rows.iter().map(|r| row_json(r, &table)).collect())
        }
        .await;
        result.unwrap_or_else(|e| {
            eprintln!("typiql-adapter-mssql: {e}");
            Vec::new()
        })
    }

    async fn query(&self, location: Location, query: ListQuery) -> Vec<Value> {
        if self
            .table(&location)
            .is_some_and(|(_, t)| t.procedure.is_some())
        {
            // As in C#: the procedure's rows, as it returns them.
            return self.get_many(location, query.filters).await;
        }
        if !self.sql_can(&query) {
            return typiql_core::query_in_memory(self, location, query).await;
        }
        let Some((_, table)) = self.table(&location) else {
            return Vec::new();
        };
        let mut sort_params = Vec::new();
        let tail = self.order_sql(&query, &table, &query.joins, &mut sort_params);
        let result = self
            .select_with_tail(&location, &query, &tail, sort_params)
            .await;
        result.unwrap_or_else(|e| {
            eprintln!("typiql-adapter-mssql: {e}");
            Vec::new()
        })
    }

    async fn aggregate(&self, spec: AggregateSpec, query: ListQuery) -> Vec<Value> {
        if query.filters.iter().any(is_element_filter) {
            let rows = self.get_many(spec.source.as_str().into(), Vec::new()).await;
            let rows = spec
                .compute(rows)
                .into_iter()
                .filter(|r| matches_all(r, &query.filters))
                .collect();
            return typiql_core::query::sort_and_page(rows, &query);
        }
        match self.run_aggregate(&spec, &query, false).await {
            Ok((rows, _)) => rows,
            Err(e) => {
                eprintln!("typiql-adapter-mssql: {e}");
                Vec::new()
            }
        }
    }

    async fn aggregate_count(&self, spec: AggregateSpec, filters: Vec<FieldFilter>) -> usize {
        let query = ListQuery {
            filters,
            ..Default::default()
        };
        self.run_aggregate(&spec, &query, true)
            .await
            .map(|(_, n)| n)
            .unwrap_or(0)
    }

    async fn count(&self, location: Location, filters: Vec<FieldFilter>) -> usize {
        if filters.iter().any(is_element_filter) {
            return self.get_many(location, filters).await.len();
        }
        let Some((name, table)) = self.table(&location) else {
            return 0;
        };
        if table.procedure.is_some() {
            return self.get_many(location, filters).await.len();
        }
        let result = async {
            let mut c = self.conn().await?;
            self.ensure(&mut c, &name, &table).await?;
            let mut params = Vec::new();
            let where_sql = where_and(&filters, &table, "p", &mut params);
            let sql = format!(
                "SELECT COUNT_BIG(*) FROM [dbo].{} AS p WHERE {where_sql}",
                bracket(&name)
            );
            let rows = query(&mut c, &sql, params).await?;
            Ok::<_, String>(rows.first().and_then(|r| r.get::<i64, _>(0)).unwrap_or(0) as usize)
        }
        .await;
        result.unwrap_or(0)
    }

    async fn add(
        &self,
        location: Location,
        key_field: &str,
        values: Value,
    ) -> Result<Value, String> {
        self.insert_many(&location, key_field, vec![values])
            .await
            .map(|mut v| v.remove(0))
    }

    async fn add_many(
        &self,
        location: Location,
        key_field: &str,
        values: Vec<Value>,
    ) -> Result<Vec<Value>, String> {
        let mut seen = HashSet::new();
        for v in &values {
            let key = match v.get(key_field) {
                Some(Value::String(k)) if !k.is_empty() => Some(k.clone()),
                Some(Value::Number(n)) => Some(n.to_string()),
                _ => None,
            };
            if let Some(k) = key {
                if !seen.insert(k.clone()) {
                    return Err(format!("Duplicate key `{k}`"));
                }
            }
        }
        self.insert_many(&location, key_field, values).await
    }

    async fn update(
        &self,
        location: Location,
        key_field: &str,
        key_value: &str,
        patch: Value,
    ) -> Option<Value> {
        self.update_many(location, key_field, vec![key_value.to_string()], patch)
            .await
            .into_iter()
            .next()
    }

    async fn update_many(
        &self,
        location: Location,
        key_field: &str,
        keys: Vec<String>,
        patch: Value,
    ) -> Vec<Value> {
        let mut out = Vec::new();
        for chunk in keys.chunks(MAX_PARAMS / 2) {
            let rows = self
                .write(&location, |name, table, params| {
                    let sets: Vec<String> = patch
                        .as_object()
                        .into_iter()
                        .flatten()
                        .filter_map(|(k, v)| {
                            let col = table.col(k)?;
                            (k != key_field && !col.is_expression()).then(|| {
                                format!("{} = {}", bracket(k), param(params, to_param(v, col)))
                            })
                        })
                        .collect();
                    let keys_sql = key_list(chunk, params);
                    let set_sql = if sets.is_empty() {
                        // Nothing to change: still return the rows.
                        format!("{0} = {0}", bracket(key_field))
                    } else {
                        sets.join(", ")
                    };
                    Some(format!(
                        "UPDATE [dbo].{} SET {set_sql} OUTPUT {} WHERE {} IN ({keys_sql})",
                        bracket(name),
                        output_columns(table, "INSERTED"),
                        bracket(key_field)
                    ))
                })
                .await;
            match rows {
                Ok(rows) => out.extend(rows),
                Err(e) => eprintln!("typiql-adapter-mssql: {e}"),
            }
        }
        out
    }

    async fn remove(&self, location: Location, key_field: &str, key_value: &str) -> Option<Value> {
        self.remove_many(location, key_field, vec![key_value.to_string()])
            .await
            .into_iter()
            .next()
    }

    async fn remove_many(
        &self,
        location: Location,
        key_field: &str,
        keys: Vec<String>,
    ) -> Vec<Value> {
        let mut out = Vec::new();
        for chunk in keys.chunks(MAX_PARAMS) {
            let rows = self
                .write(&location, |name, table, params| {
                    let keys_sql = key_list(chunk, params);
                    Some(format!(
                        "DELETE FROM [dbo].{} OUTPUT {} WHERE {} IN ({keys_sql})",
                        bracket(name),
                        output_columns(table, "DELETED"),
                        bracket(key_field)
                    ))
                })
                .await;
            match rows {
                Ok(rows) => out.extend(rows),
                Err(e) => eprintln!("typiql-adapter-mssql: {e}"),
            }
        }
        out
    }

    async fn set_table(&self, location: Location, rows: Vec<Value>) {
        let Some((name, table)) = self.table(&location) else {
            return;
        };
        let _ = self
            .write(&location, |name, _, _| {
                Some(format!("DELETE FROM [dbo].{}", bracket(name)))
            })
            .await;
        if !rows.is_empty() {
            if let Err(e) = self.insert_many(&location, &table.key, rows).await {
                eprintln!("typiql-adapter-mssql: set_table {name}: {e}");
            }
        }
    }

    fn register_procedure(&self, table: &str, procedure: &str, params: &[&'static str]) {
        if let Some(t) = self.inner.tables.write().unwrap().get_mut(table) {
            t.procedure = Some((
                procedure.to_string(),
                params.iter().map(|p| p.to_string()).collect(),
            ));
        }
    }

    fn register_type(
        &self,
        table: &str,
        key_field: &str,
        columns: &[(&'static str, &'static str)],
    ) {
        let columns = columns
            .iter()
            .map(|(n, t)| Col {
                name: n.to_string(),
                ty: t.to_string(),
            })
            .collect();
        self.inner.tables.write().unwrap().insert(
            table.to_string(),
            Table {
                key: key_field.to_string(),
                columns,
                procedure: None,
            },
        );
    }
}

impl MssqlAdapter {
    /// The query itself: the parent's columns, each join as a JSON
    /// subquery, the filters, then `tail`. The sort in `tail` numbered its
    /// own parameters from @P1 (`sort_params`); they're renumbered to follow
    /// the select's.
    async fn select_with_tail(
        &self,
        location: &Location,
        q: &ListQuery,
        tail: &str,
        sort_params: Vec<Param>,
    ) -> Result<Vec<Value>, String> {
        let Some((name, table)) = self.table(location) else {
            return Ok(Vec::new());
        };
        if let Some((procedure, proc_params)) = &table.procedure {
            return self
                .exec_procedure(&table, procedure, proc_params, &q.filters)
                .await;
        }
        let mut c = self.conn().await?;
        self.ensure(&mut c, &name, &table).await?;
        let mut params = Vec::new();
        let mut select: Vec<String> = table
            .columns
            .iter()
            .map(|col| format!("{} AS {}", col_sql(&col.name, "p"), bracket(&col.name)))
            .collect();
        let mut joined = Vec::new();
        for join in &q.joins {
            let Some(target) = self.inner.tables.read().unwrap().get(&join.target).cloned() else {
                continue;
            };
            self.ensure(&mut c, &join.target, &target).await?;
            let sql = join_json_sql(join, &target, "p", &mut params);
            select.push(format!("({sql}) AS {}", bracket(&join.name)));
            joined.push((join.name.clone(), join.many, target));
        }
        let where_sql = where_and(&q.filters, &table, "p", &mut params);
        let offset = params.len();
        let tail = renumber(tail, sort_params.len(), offset);
        params.extend(sort_params);
        let sql = format!(
            "SELECT * FROM (SELECT {} FROM [dbo].{} AS p WHERE {where_sql}) AS q{tail}",
            select.join(", "),
            bracket(&name)
        );
        let rows = query(&mut c, &sql, params)
            .await
            .inspect_err(|_| c.healthy = false)?;
        Ok(rows
            .iter()
            .map(|row| {
                let mut out = row_json(row, &table);
                if let Value::Object(map) = &mut out {
                    for (join_name, many, target) in &joined {
                        let raw = map.remove(join_name).unwrap_or(Value::Null);
                        map.insert(join_name.clone(), joined_json(raw, *many, target));
                    }
                }
                out
            })
            .collect())
    }
}

// ── SQL rendering ───────────────────────────────────────────────────────────

/// A bound parameter value.
#[derive(Debug, Clone)]
enum Param {
    Text(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Null,
}

/// Adds a parameter; returns its placeholder (`@P3`).
fn param(params: &mut Vec<Param>, p: Param) -> String {
    params.push(p);
    format!("@P{}", params.len())
}

/// Shifts `@P1..@Pn` in `sql` (the sort's own numbering) up by `offset`.
fn renumber(sql: &str, count: usize, offset: usize) -> String {
    let mut out = sql.to_string();
    for i in (1..=count).rev() {
        out = out.replace(&format!("@P{i}"), &format!("@P#{}", i + offset));
    }
    out.replace("@P#", "@P")
}

fn is_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn bracket(name: &str) -> String {
    format!("[{}]", name.replace(']', "]]"))
}

/// A field as SQL: `alias.[column]`, or the expression itself (C#'s raw
/// `DataName`) in parentheses.
fn col_sql(name: &str, alias: &str) -> String {
    if is_identifier(name) {
        format!("{alias}.{}", bracket(name))
    } else {
        format!("({name})")
    }
}

fn select_columns(table: &Table, alias: &str) -> String {
    table
        .columns
        .iter()
        .map(|c| format!("{} AS {}", col_sql(&c.name, alias), bracket(&c.name)))
        .collect::<Vec<_>>()
        .join(", ")
}

fn output_columns(table: &Table, pseudo: &str) -> String {
    table
        .stored()
        .map(|c| format!("{pseudo}.{}", bracket(&c.name)))
        .collect::<Vec<_>>()
        .join(", ")
}

fn key_list(keys: &[String], params: &mut Vec<Param>) -> String {
    keys.iter()
        .map(|k| param(params, Param::Text(k.clone())))
        .collect::<Vec<_>>()
        .join(", ")
}

/// typiql's column type marker → a SQL Server type.
fn sql_type(base: &str, key: bool) -> &'static str {
    match base {
        _ if key => "NVARCHAR(450)",
        "BOOLEAN" => "BIT",
        "DOUBLE" => "FLOAT",
        "TINYINT" | "SMALLINT" => "SMALLINT",
        "INTEGER" | "UTINYINT" | "USMALLINT" => "INT",
        "BIGINT" | "UINTEGER" => "BIGINT",
        "HUGEINT" | "UBIGINT" | "UHUGEINT" => "DECIMAL(38, 0)",
        _ => "NVARCHAR(MAX)",
    }
}

/// A JSON value as a parameter for a column of `col`'s type.
fn to_param(v: &Value, col: &Col) -> Param {
    match (v, col.base()) {
        (Value::Null, _) => Param::Null,
        (_, "JSON") => Param::Text(v.to_string()),
        (Value::String(s), _) => Param::Text(s.clone()),
        (Value::Bool(b), _) => Param::Bool(*b),
        (Value::Number(n), _) => match n.as_i64() {
            Some(i) => Param::Int(i),
            None => Param::Float(n.as_f64().unwrap_or(0.0)),
        },
        (other, _) => Param::Text(other.to_string()),
    }
}

/// A column expression for comparing or sorting: strings by code point.
fn sortable(expr: &str, col: &Col) -> String {
    if col.is_text() {
        format!("{expr} COLLATE {BINARY}")
    } else {
        expr.to_string()
    }
}

fn where_and(
    filters: &[FieldFilter],
    table: &Table,
    alias: &str,
    params: &mut Vec<Param>,
) -> String {
    let parts: Vec<String> = filters
        .iter()
        .filter(|f| !is_element_filter(f))
        .map(|f| render_filter(f, table, alias, params))
        .collect();
    if parts.is_empty() {
        "1 = 1".to_string()
    } else {
        parts.join(" AND ")
    }
}

fn is_element_op(op: FilterOp) -> bool {
    matches!(op, FilterOp::AnyIn | FilterOp::NoneIn | FilterOp::ElemMatch)
}

/// One filter against a literal value, with the reference semantics.
fn render_filter(f: &FieldFilter, table: &Table, alias: &str, params: &mut Vec<Param>) -> String {
    let Some(col) = table.col(&f.field) else {
        // Not a field of this table: null on every row.
        return match f.op {
            FilterOp::Ne | FilterOp::NotIn => "1 = 1",
            _ => "1 = 0",
        }
        .to_string();
    };
    let c = col_sql(&col.name, alias);
    let cmp = sortable(&c, col);
    let p = |params: &mut Vec<Param>, v: &Value| param(params, to_param(v, col));
    match f.op {
        FilterOp::Eq if f.value.is_null() => format!("{c} IS NULL"),
        FilterOp::Eq => format!("{cmp} = {}", p(params, &f.value)),
        FilterOp::Ne if f.value.is_null() => format!("{c} IS NOT NULL"),
        FilterOp::Ne => format!("({cmp} <> {} OR {c} IS NULL)", p(params, &f.value)),
        FilterOp::Gt => format!("{cmp} > {}", p(params, &f.value)),
        FilterOp::Gte => format!("{cmp} >= {}", p(params, &f.value)),
        FilterOp::Lt => format!("{cmp} < {}", p(params, &f.value)),
        FilterOp::Lte => format!("{cmp} <= {}", p(params, &f.value)),
        FilterOp::In | FilterOp::NotIn => {
            let items = f.value.as_array().cloned().unwrap_or_default();
            let positive = f.op == FilterOp::In;
            if items.is_empty() {
                return (if positive { "1 = 0" } else { "1 = 1" }).to_string();
            }
            let list = items
                .iter()
                .map(|v| p(params, v))
                .collect::<Vec<_>>()
                .join(", ");
            if positive {
                format!("{cmp} IN ({list})")
            } else {
                format!("({cmp} NOT IN ({list}) OR {c} IS NULL)")
            }
        }
        FilterOp::StartsWith => like(&c, params, &f.value, false, |n| format!("{n}%")),
        FilterOp::EndsWith => like(&c, params, &f.value, false, |n| format!("%{n}")),
        FilterOp::Contains => like(&c, params, &f.value, false, |n| format!("%{n}%")),
        FilterOp::NotStartsWith => like(&c, params, &f.value, true, |n| format!("{n}%")),
        FilterOp::NotEndsWith => like(&c, params, &f.value, true, |n| format!("%{n}")),
        FilterOp::NotContains => like(&c, params, &f.value, true, |n| format!("%{n}%")),
        // Applied in memory (see `get_many`).
        FilterOp::AnyIn | FilterOp::NoneIn | FilterOp::ElemMatch => "1 = 1".to_string(),
    }
}

/// Case-insensitive pattern match, with LIKE's metacharacters escaped so the
/// needle is plain text. A NULL column matches neither form.
fn like(
    c: &str,
    params: &mut Vec<Param>,
    needle: &Value,
    negated: bool,
    pattern: impl Fn(&str) -> String,
) -> String {
    let needle = needle.as_str().unwrap_or("");
    let escaped = needle
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
        .replace('[', "\\[");
    let p = param(params, Param::Text(pattern(&escaped)));
    format!(
        "{c} COLLATE {CASE_INSENSITIVE} {}LIKE {p} ESCAPE '\\'",
        if negated { "NOT " } else { "" }
    )
}

/// A join condition against the parent's own column (a correlated
/// reference), with the same semantics as [`render_filter`].
fn render_against(op: FilterOp, col: &Col, target_alias: &str, rhs: &str) -> String {
    let c = col_sql(&col.name, target_alias);
    let cmp = sortable(&c, col);
    let rhs_cmp = if col.is_text() {
        format!("{rhs} COLLATE {BINARY}")
    } else {
        rhs.to_string()
    };
    let escaped = format!("REPLACE(REPLACE(REPLACE(REPLACE({rhs}, '\\', '\\\\'), '%', '\\%'), '_', '\\_'), '[', '\\[')");
    let like = |negated: bool, pattern: String| {
        format!(
            "{c} COLLATE {CASE_INSENSITIVE} {}LIKE {pattern} ESCAPE '\\'",
            if negated { "NOT " } else { "" }
        )
    };
    match op {
        FilterOp::Eq => format!("{cmp} = {rhs_cmp}"),
        FilterOp::Ne => format!("({cmp} <> {rhs_cmp} OR {c} IS NULL)"),
        FilterOp::Gt => format!("{cmp} > {rhs_cmp}"),
        FilterOp::Gte => format!("{cmp} >= {rhs_cmp}"),
        FilterOp::Lt => format!("{cmp} < {rhs_cmp}"),
        FilterOp::Lte => format!("{cmp} <= {rhs_cmp}"),
        FilterOp::StartsWith => like(false, format!("({escaped} + '%')")),
        FilterOp::NotStartsWith => like(true, format!("({escaped} + '%')")),
        FilterOp::EndsWith => like(false, format!("('%' + {escaped})")),
        FilterOp::NotEndsWith => like(true, format!("('%' + {escaped})")),
        FilterOp::Contains => like(false, format!("('%' + {escaped} + '%')")),
        FilterOp::NotContains => like(true, format!("('%' + {escaped} + '%')")),
        // Excluded by `sql_can`.
        _ => "1 = 0".to_string(),
    }
}

/// A join's WHERE clause, against parent rows aliased `parent`.
fn join_where(join: &Join, target: &Table, parent: &str, params: &mut Vec<Param>) -> String {
    let mut parts = Vec::new();
    for c in &join.conditions {
        match &c.value {
            JoinValue::Literal(v) => {
                let f = FieldFilter {
                    field: c.field.clone(),
                    op: c.op,
                    value: v.clone(),
                };
                parts.push(render_filter(&f, target, "t", params));
            }
            JoinValue::Parent(parent_field) => {
                let Some(col) = target.col(&c.field) else {
                    parts.push("1 = 0".to_string());
                    continue;
                };
                let rhs = format!("{parent}.{}", bracket(parent_field));
                // A parent without the value has no related rows.
                parts.push(format!(
                    "({rhs} IS NOT NULL AND {})",
                    render_against(c.op, col, "t", &rhs)
                ));
            }
        }
    }
    if parts.is_empty() {
        "1 = 1".to_string()
    } else {
        parts.join(" AND ")
    }
}

fn join_order(join: &Join, target: &Table) -> String {
    let mut order: Vec<String> = join
        .sort
        .iter()
        .filter_map(|k: &SortKey| {
            let col = target.col(&k.field)?;
            Some(format!(
                "{} {}",
                sortable(&col_sql(&col.name, "t"), col),
                if k.desc { "DESC" } else { "ASC" }
            ))
        })
        .collect();
    order.push(format!("t.{} ASC", bracket(&join.target_key)));
    order.join(", ")
}

fn join_page(join: &Join) -> String {
    let limit = if join.many { join.limit } else { Some(1) };
    let mut page = format!(" OFFSET {} ROWS", join.skip);
    if let Some(l) = limit {
        page.push_str(&format!(" FETCH NEXT {l} ROWS ONLY"));
    }
    page
}

/// A join as a correlated subquery returning its rows as JSON text.
fn join_json_sql(join: &Join, target: &Table, parent: &str, params: &mut Vec<Param>) -> String {
    let where_sql = join_where(join, target, parent, params);
    let wrapper = if join.many {
        ""
    } else {
        ", WITHOUT_ARRAY_WRAPPER"
    };
    format!(
        "SELECT {} FROM [dbo].{} AS t WHERE {where_sql} ORDER BY {}{} FOR JSON PATH, INCLUDE_NULL_VALUES{wrapper}",
        select_columns(target, "t"),
        bracket(&join.target),
        join_order(join, target),
        join_page(join)
    )
}

/// The value a parent sorts by for `target.col` of a join: the related row's
/// (has-one), or the smallest / largest of the related rows' (has-many, as
/// Mongo sorts by an array).
fn join_sort_sql(
    join: &Join,
    target: &Table,
    col: &Col,
    desc: bool,
    parent: &str,
    params: &mut Vec<Param>,
) -> String {
    let where_sql = join_where(join, target, parent, params);
    let value = sortable(&format!("x.{}", bracket(&col.name)), col);
    let pick = if desc { "MAX" } else { "MIN" };
    format!(
        "(SELECT {pick}({value}) FROM (SELECT {} FROM [dbo].{} AS t WHERE {where_sql} ORDER BY {}{}) AS x)",
        select_columns(target, "t"),
        bracket(&join.target),
        join_order(join, target),
        join_page(join)
    )
}

// ── rows ────────────────────────────────────────────────────────────────────

fn row_json(row: &Row, table: &Table) -> Value {
    let mut map = Map::new();
    for (column, data) in row.cells() {
        let name = column.name().to_string();
        let ty = table
            .col(&name)
            .map(|c| c.base().to_string())
            .unwrap_or_default();
        map.insert(name, cell_json(data, &ty));
    }
    Value::Object(map)
}

fn cell_json(data: &ColumnData<'static>, ty: &str) -> Value {
    match data {
        ColumnData::U8(v) => v.map(Value::from).unwrap_or(Value::Null),
        ColumnData::I16(v) => v.map(Value::from).unwrap_or(Value::Null),
        ColumnData::I32(v) => v.map(Value::from).unwrap_or(Value::Null),
        ColumnData::I64(v) => v.map(Value::from).unwrap_or(Value::Null),
        ColumnData::F32(v) => v.map(|f| Value::from(f as f64)).unwrap_or(Value::Null),
        ColumnData::F64(v) => v.map(Value::from).unwrap_or(Value::Null),
        ColumnData::Bit(v) => v.map(Value::from).unwrap_or(Value::Null),
        ColumnData::Numeric(v) => v
            .map(|n| {
                let s = n.to_string();
                s.parse::<i64>().map(Value::from).unwrap_or_else(|_| {
                    s.parse::<f64>()
                        .map(Value::from)
                        .unwrap_or(Value::String(s))
                })
            })
            .unwrap_or(Value::Null),
        ColumnData::String(Some(s)) => text_json(s, ty),
        ColumnData::String(None) => Value::Null,
        ColumnData::Guid(v) => v
            .map(|g| Value::String(g.to_string()))
            .unwrap_or(Value::Null),
        _ => Value::Null,
    }
}

/// Text as JSON, by column type: a JSON column holds JSON text; an `ANY`
/// column holds plain text for strings (e.g. an enum) or JSON otherwise.
fn text_json(s: &str, ty: &str) -> Value {
    let structured = ty == "JSON" || (ty == "ANY" && (s.starts_with('{') || s.starts_with('[')));
    if structured {
        serde_json::from_str(s).unwrap_or_else(|_| Value::String(s.to_string()))
    } else {
        Value::String(s.to_string())
    }
}

/// A join's JSON text as JSON, converting each field by the target's types:
/// JSON columns arrive as strings inside FOR JSON, and BITs as booleans.
fn joined_json(raw: Value, many: bool, target: &Table) -> Value {
    let parsed = match raw {
        Value::String(s) => serde_json::from_str(&s).unwrap_or(Value::Null),
        other => other,
    };
    let fix = |row: Value| match row {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(k, v)| {
                    let ty = target
                        .col(&k)
                        .map(|c| c.base().to_string())
                        .unwrap_or_default();
                    let v = match v {
                        Value::String(s) => text_json(&s, &ty),
                        other => other,
                    };
                    (k, v)
                })
                .collect(),
        ),
        other => other,
    };
    match (parsed, many) {
        (Value::Array(items), true) => Value::Array(items.into_iter().map(fix).collect()),
        (_, true) => Value::Array(Vec::new()),
        (Value::Null, false) => Value::Null,
        (v, false) => fix(v),
    }
}

fn assign_key(row: &mut Value, key_field: &str) {
    let missing = row
        .get(key_field)
        .and_then(Value::as_str)
        .is_none_or(str::is_empty);
    if missing {
        if let Value::Object(map) = row {
            map.insert(
                key_field.to_string(),
                Value::String(uuid::Uuid::new_v4().to_string()),
            );
        }
    }
}

/// A primary key violation (2627) or unique index violation (2601) as every
/// adapter reports it.
fn key_error(e: &str) -> String {
    if e.contains("2627") || e.contains("2601") || e.to_lowercase().contains("duplicate key") {
        let key = e
            .split("duplicate key value is (")
            .nth(1)
            .and_then(|rest| rest.split(')').next())
            .unwrap_or("?");
        format!("Duplicate key `{key}`")
    } else {
        e.to_string()
    }
}

// ── execution ───────────────────────────────────────────────────────────────

fn bound<'a>(sql: &'a str, params: Vec<Param>) -> Query<'a> {
    let mut q = Query::new(sql);
    for p in params {
        match p {
            Param::Text(s) => q.bind(s),
            Param::Int(i) => q.bind(i),
            Param::Float(f) => q.bind(f),
            Param::Bool(b) => q.bind(b),
            Param::Null => q.bind(Option::<String>::None),
        }
    }
    q
}

async fn query(c: &mut Conn, sql: &str, params: Vec<Param>) -> Result<Vec<Row>, String> {
    let stream = bound(sql, params)
        .query(c)
        .await
        .map_err(|e| format!("{e} (code {:?}) in: {sql}", e.code()))?;
    stream
        .into_first_result()
        .await
        .map_err(|e| format!("{e} (code {:?}) in: {sql}", e.code()))
}

/// Runs a statement for its effect. Without parameters it goes as a plain
/// batch — parameterised statements run inside `sp_executesql`, where a
/// transaction can't be begun and left open for the next statement.
async fn run(c: &mut Conn, sql: &str, params: Vec<Param>) -> Result<(), String> {
    let err = |e: tiberius::error::Error| format!("{e} (code {:?}) in: {sql}", e.code());
    if params.is_empty() {
        c.simple_query(sql)
            .await
            .map_err(err)?
            .into_results()
            .await
            .map_err(err)?;
        Ok(())
    } else {
        bound(sql, params).execute(c).await.map(|_| ()).map_err(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expressions_are_raw_sql() {
        assert_eq!(col_sql("name", "p"), "p.[name]");
        assert_eq!(
            col_sql("FirstName + ' ' + LastName", "p"),
            "(FirstName + ' ' + LastName)"
        );
        assert_eq!(bracket("a]b"), "[a]]b]");
    }

    #[test]
    fn renumbers_sort_parameters() {
        assert_eq!(renumber("x = @P1 AND y = @P2", 2, 3), "x = @P4 AND y = @P5");
        assert_eq!(renumber("@P1 @P10", 10, 0), "@P1 @P10");
    }

    #[test]
    fn duplicate_key_errors_name_the_key() {
        let e = "Violation of PRIMARY KEY constraint 'PK__items'. Cannot insert duplicate key in object 'dbo.items'. The duplicate key value is (k1). (code Some(2627))";
        assert_eq!(key_error(e), "Duplicate key `k1`");
    }
}
