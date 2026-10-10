//! A `TypiQLAdapter` backed by MongoDB — the Rust port of the original C#
//! typiql's Mongo adapter (`MongoData.cs`), whose semantics are the reference
//! every other adapter is held to by `typiql-conformance`.
//!
//! Each type is a collection. Rows are stored as native BSON documents
//! (nested objects and lists included), and every read is an aggregation
//! pipeline: filters, sorting and paging run in the server, and relations in
//! join mode are `$lookup`s, as in C#'s `AggregateDocuments`.
//!
//! Keys, as in C#: a key field whose `data_name` is `_id` is Mongo's own id —
//! assigned as an `ObjectId` when missing, returned and matched as its hex
//! string. Any other key field is an ordinary field with a unique index
//! (created on first use), and the document keeps a private `_id` that's
//! never returned. Integer keys get the next number (max + 1).
//!
//! Not ported from C#: GridFS-backed file types.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use futures_util::TryStreamExt;
use mongodb::bson::{doc, oid::ObjectId, Bson, Document};
use mongodb::error::ErrorKind;
use mongodb::options::IndexOptions;
use mongodb::{Client, Collection, IndexModel};
use serde_json::{Map, Value};
use typiql_core::query::{matches_all, ElementAt, ElementMatch};
use typiql_core::{
    AggregateFn, AggregateSpec, FieldFilter, FilterOp, Join, JoinValue, ListQuery, Location, SortKey, TypiQLAdapter,
};
use uuid::Uuid;

const ID: &str = "_id";
/// Mongo's duplicate key error.
const DUPLICATE_KEY: i32 = 11000;

#[derive(Clone)]
pub struct MongoAdapter {
    inner: Arc<Inner>,
}

struct Inner {
    uri: String,
    database: String,
    client: tokio::sync::OnceCell<Client>,
    tables: RwLock<HashMap<String, Table>>,
    indexed: tokio::sync::Mutex<HashSet<String>>,
}

#[derive(Clone, Default)]
struct Table {
    key: String,
    integer_key: bool,
    columns: HashSet<String>,
}

impl Table {
    /// Whether rows include `_id` — only when the type declares it.
    fn exposes_id(&self) -> bool {
        self.key == ID || self.columns.contains(ID)
    }
}

impl MongoAdapter {
    /// `uri` is a MongoDB connection string (`mongodb://host:27017`); the
    /// connection is made on first use.
    pub fn new(uri: &str, database: &str) -> Self {
        MongoAdapter {
            inner: Arc::new(Inner {
                uri: uri.to_string(),
                database: database.to_string(),
                client: tokio::sync::OnceCell::new(),
                tables: RwLock::new(HashMap::new()),
                indexed: tokio::sync::Mutex::new(HashSet::new()),
            }),
        }
    }

    /// Drops a whole database (for tests' throwaway databases).
    pub async fn drop_database(uri: &str, database: &str) -> Result<(), String> {
        let client = Client::with_uri_str(uri).await.map_err(err)?;
        client.database(database).drop().await.map_err(err)
    }

    async fn client(&self) -> Result<&Client, String> {
        self.inner
            .client
            .get_or_try_init(|| async { Client::with_uri_str(&self.inner.uri).await.map_err(err) })
            .await
    }

    /// A location's collection name and registered type (a default one,
    /// keyed by `key_field`, for a collection no type registered).
    fn table(&self, location: &Location, key_field: &str) -> (String, Table) {
        let name = match location {
            Location::Named(t) => t.clone(),
            Location::At { table, .. } => table.clone(),
        };
        let table = self.inner.tables.read().unwrap().get(&name).cloned();
        let table = table.unwrap_or_else(|| Table { key: key_field.to_string(), ..Default::default() });
        (name, table)
    }

    fn registered(&self, name: &str) -> Option<Table> {
        self.inner.tables.read().unwrap().get(name).cloned()
    }

    /// The collection, with the key's unique index made on first use.
    async fn collection(&self, name: &str, table: &Table) -> Result<Collection<Document>, String> {
        let coll = self.client().await?.database(&self.inner.database).collection::<Document>(name);
        if table.key.is_empty() || table.key == ID {
            return Ok(coll);
        }
        let mut indexed = self.inner.indexed.lock().await;
        if indexed.insert(name.to_string()) {
            let index = IndexModel::builder()
                .keys(doc! { table.key.as_str(): 1 })
                .options(IndexOptions::builder().unique(true).name("typiql_key".to_string()).build())
                .build();
            if let Err(e) = coll.create_index(index).await {
                eprintln!("typiql-adapter-mongo: couldn't create the unique key index on {name}: {e}");
            }
        }
        Ok(coll)
    }

    async fn run(&self, name: &str, table: &Table, pipeline: Vec<Document>) -> Result<Vec<Value>, String> {
        let coll = self.collection(name, table).await?;
        let docs: Vec<Document> = coll
            .aggregate(pipeline)
            .allow_disk_use(true)
            .await
            .map_err(err)?
            .try_collect()
            .await
            .map_err(err)?;
        Ok(docs.into_iter().map(|d| row_json(d, table)).collect())
    }

    async fn run_count(&self, name: &str, table: &Table, mut pipeline: Vec<Document>) -> Result<usize, String> {
        pipeline.push(doc! { "$count": "n" });
        let rows = self.run(name, table, pipeline).await?;
        Ok(rows.first().and_then(|r| r.get("n")).and_then(Value::as_u64).unwrap_or(0) as usize)
    }

    /// Filters as pipeline stages; `None` when they need the in-memory path.
    fn match_stages(table: &Table, filters: &[FieldFilter]) -> Option<Vec<Document>> {
        let mut f = Filters { table, stages: Vec::new(), temps: Vec::new() };
        let matcher = f.all(filters, "", false)?;
        let mut stages = f.stages;
        if !matcher.is_empty() {
            stages.push(doc! { "$match": matcher });
        }
        if !f.temps.is_empty() {
            stages.push(doc! { "$unset": f.temps });
        }
        Some(stages)
    }

    /// The whole read as a pipeline: filters, joins, sorting and paging —
    /// `None` when some of it needs the in-memory path.
    fn query_pipeline(&self, table: &Table, q: &ListQuery) -> Option<Vec<Document>> {
        let mut stages = Self::match_stages(table, &q.filters)?;
        let joined_sort = q.sort.iter().any(|k| {
            k.field.split_once('.').is_some_and(|(name, _)| q.joins.iter().any(|j| j.name == name))
        });
        let paging = page_stages(&q.sort, &q.key_field, q.skip, q.limit);
        if !joined_sort {
            // Page first: only the page's rows need their joins.
            stages.extend(paging.clone());
        }
        for join in &q.joins {
            stages.extend(self.lookup(join)?);
        }
        if joined_sort {
            stages.extend(paging);
        }
        Some(stages)
    }

    /// A join as `$lookup`. Conditions against a literal are ordinary
    /// filters; against a parent field, equality only (anything else takes
    /// the in-memory path), and a parent without that value has no related
    /// rows.
    fn lookup(&self, join: &Join) -> Option<Vec<Document>> {
        let target = self.registered(&join.target)?;
        let mut lets = Document::new();
        let mut exprs: Vec<Bson> = Vec::new();
        let mut literal = Vec::new();
        for (i, c) in join.conditions.iter().enumerate() {
            match &c.value {
                JoinValue::Literal(v) => literal.push(FieldFilter { field: c.field.clone(), op: c.op, value: v.clone() }),
                JoinValue::Parent(field) => {
                    if c.op != FilterOp::Eq {
                        return None;
                    }
                    let var = format!("p{i}");
                    let reference = format!("$${var}");
                    // `_id`s are compared as their hex strings on the
                    // parent's side, as ObjectIds on the target's (which
                    // keeps its index usable).
                    let value: Bson = if field == ID {
                        doc! { "$toString": "$_id" }.into()
                    } else {
                        format!("${field}").into()
                    };
                    lets.insert(var, value);
                    let rhs: Bson = if c.field == ID {
                        doc! { "$convert": { "input": &reference, "to": "objectId", "onError": &reference, "onNull": Bson::Null } }.into()
                    } else {
                        reference.clone().into()
                    };
                    exprs.push(doc! { "$not": [{ "$in": [{ "$type": &reference }, ["missing", "null"]] }] }.into());
                    exprs.push(doc! { "$eq": [format!("${}", c.field), rhs] }.into());
                }
            }
        }
        let mut sub = Self::match_stages(&target, &literal)?;
        if !exprs.is_empty() {
            sub.insert(0, doc! { "$match": { "$expr": { "$and": exprs } } });
        }
        let limit = if join.many { join.limit } else { Some(1) };
        sub.extend(page_stages(&join.sort, &join.target_key, join.skip, limit));
        if !target.exposes_id() {
            sub.push(doc! { "$project": { ID: 0 } });
        }
        let mut stages = vec![doc! {
            "$lookup": { "from": &join.target, "let": lets, "pipeline": sub, "as": &join.name }
        }];
        if !join.many {
            let path = format!("${}", join.name);
            stages.push(doc! { "$addFields": { &join.name: { "$ifNull": [{ "$arrayElemAt": [path, 0] }, Bson::Null] } } });
        }
        Some(stages)
    }

    /// An aggregate type as `$group`, then its filters, sorting and paging.
    fn aggregate_pipeline(spec: &AggregateSpec, q: &ListQuery) -> Option<Vec<Document>> {
        let mut group = Document::new();
        let mut id = Document::new();
        let mut project = doc! { ID: 0 };
        for (i, (out, field)) in spec.groups.iter().enumerate() {
            // Missing and null are one group.
            id.insert(format!("g{i}"), doc! { "$ifNull": [format!("${field}"), Bson::Null] });
            project.insert(out, format!("$_id.g{i}"));
        }
        group.insert(ID, if id.is_empty() { Bson::Null } else { id.into() });
        for m in &spec.measures {
            let field = format!("${}", m.field.as_deref().unwrap_or_default());
            let acc = match m.func {
                AggregateFn::Count => doc! { "$sum": 1 },
                AggregateFn::CountOf => {
                    doc! { "$sum": { "$cond": [{ "$in": [{ "$type": &field }, ["missing", "null"]] }, 0, 1] } }
                }
                AggregateFn::Sum => doc! { "$sum": &field },
                AggregateFn::Avg => doc! { "$avg": &field },
                AggregateFn::Min => doc! { "$min": &field },
                AggregateFn::Max => doc! { "$max": &field },
            };
            group.insert(&m.name, acc);
            project.insert(&m.name, 1);
        }
        let mut stages = vec![doc! { "$group": group }, doc! { "$project": project }];
        stages.extend(Self::match_stages(&Table::default(), &q.filters)?);
        stages.extend(page_stages(&q.sort, &q.key_field, q.skip, q.limit));
        Some(stages)
    }

    /// Inserts rows, all or none: an ordered insert stops at the first
    /// failure, and the rows before it are removed again.
    async fn insert(&self, location: &Location, key_field: &str, values: Vec<Value>) -> Result<Vec<Value>, String> {
        let (name, table) = self.table(location, key_field);
        let coll = self.collection(&name, &table).await?;
        let key = table.key.clone();
        let mut seen = HashSet::new();
        for v in &values {
            if let Some(k) = key_text(v, &key) {
                if !seen.insert(k.clone()) {
                    return Err(format!("Duplicate key `{k}`"));
                }
            }
        }
        // Integer keys are max + 1; a concurrent insert may take the same
        // number, which the unique index rejects — then try again.
        for _ in 0..5 {
            let mut next = if table.integer_key {
                let stored = self.max_key(&coll, &key).await?;
                values.iter().filter_map(|v| v.get(&key).and_then(Value::as_i64)).fold(stored, i64::max)
            } else {
                0
            };
            let mut docs = Vec::with_capacity(values.len());
            let mut assigned = HashSet::new();
            for (i, v) in values.iter().enumerate() {
                let mut d = match to_bson(v) {
                    Bson::Document(d) => d,
                    _ => return Err("mongo adapter: a row must be an object".into()),
                };
                if !key.is_empty() {
                    let value = match key_text(v, &key) {
                        Some(k) => stored_key(&table, &key, &Value::String(k)),
                        None => {
                            assigned.insert(i);
                            if table.integer_key {
                                next += 1;
                                Bson::Int64(next)
                            } else if key == ID {
                                Bson::ObjectId(ObjectId::new())
                            } else {
                                Bson::String(Uuid::new_v4().to_string())
                            }
                        }
                    };
                    d.insert(key.clone(), value);
                }
                if !d.contains_key(ID) {
                    d.insert(ID, ObjectId::new());
                }
                docs.push(d);
            }
            let error = match coll.insert_many(&docs).ordered(true).await {
                Ok(_) => return Ok(docs.into_iter().map(|d| row_json(d, &table)).collect()),
                Err(e) => e,
            };
            let failed = match error.kind.as_ref() {
                ErrorKind::InsertMany(e) => e.write_errors.as_ref().and_then(|w| w.first()).map(|w| (w.index, w.code)),
                _ => None,
            };
            let Some((index, code)) = failed else {
                return Err(err(error));
            };
            let inserted: Vec<Bson> = docs[..index].iter().filter_map(|d| d.get(ID).cloned()).collect();
            if !inserted.is_empty() {
                coll.delete_many(doc! { ID: { "$in": inserted } }).await.map_err(err)?;
            }
            if code != DUPLICATE_KEY {
                return Err(err(error));
            }
            if !(table.integer_key && assigned.contains(&index)) {
                let k = docs[index].get(&key).cloned().map(to_json).unwrap_or(Value::Null);
                return Err(format!("Duplicate key `{}`", k.as_str().map(str::to_string).unwrap_or_else(|| k.to_string())));
            }
        }
        Err(format!("mongo adapter: couldn't assign a key in {name}"))
    }

    async fn max_key(&self, coll: &Collection<Document>, key: &str) -> Result<i64, String> {
        let pipeline = vec![
            doc! { "$match": { key: { "$type": "number" } } },
            doc! { "$sort": { key: -1 } },
            doc! { "$limit": 1 },
        ];
        let docs: Vec<Document> = coll.aggregate(pipeline).await.map_err(err)?.try_collect().await.map_err(err)?;
        Ok(docs.first().and_then(|d| d.get(key)).map(|b| to_json(b.clone())).and_then(|v| v.as_i64()).unwrap_or(0))
    }

    /// `{key: {$in: [...]}}` for keys given as text.
    fn keys_filter(table: &Table, keys: &[String]) -> Document {
        let values: Vec<Bson> = keys.iter().flat_map(|k| key_forms(table, &table.key, &Value::String(k.clone()))).collect();
        doc! { table.key.as_str(): { "$in": values } }
    }

    fn log<T: Default>(result: Result<T, String>) -> T {
        result.unwrap_or_else(|e| {
            eprintln!("typiql-adapter-mongo: {e}");
            T::default()
        })
    }
}

#[async_trait]
impl TypiQLAdapter for MongoAdapter {
    async fn get_one(&self, location: Location, key_field: &str, key_value: &str) -> Option<Value> {
        let (name, table) = self.table(&location, key_field);
        let filter = FieldFilter { field: key_field.to_string(), op: FilterOp::Eq, value: Value::String(key_value.to_string()) };
        let mut pipeline = Self::match_stages(&table, &[filter])?;
        pipeline.push(doc! { "$limit": 1 });
        Self::log(self.run(&name, &table, pipeline).await).into_iter().next()
    }

    async fn get_many(&self, location: Location, filters: Vec<FieldFilter>) -> Vec<Value> {
        let (name, table) = self.table(&location, "");
        match Self::match_stages(&table, &filters) {
            Some(pipeline) => Self::log(self.run(&name, &table, pipeline).await),
            None => {
                let all = Self::log(self.run(&name, &table, Vec::new()).await);
                all.into_iter().filter(|r| matches_all(r, &filters)).collect()
            }
        }
    }

    async fn get_many_matching_any(&self, location: Location, or_groups: Vec<Vec<FieldFilter>>) -> Vec<Value> {
        if or_groups.is_empty() {
            return Vec::new();
        }
        let (name, table) = self.table(&location, "");
        let mut f = Filters { table: &table, stages: Vec::new(), temps: Vec::new() };
        let groups: Option<Vec<Document>> = or_groups.iter().map(|g| f.all(g, "", false)).collect();
        match groups {
            Some(groups) => {
                let mut pipeline = f.stages;
                pipeline.push(doc! { "$match": { "$or": groups } });
                if !f.temps.is_empty() {
                    pipeline.push(doc! { "$unset": f.temps });
                }
                Self::log(self.run(&name, &table, pipeline).await)
            }
            None => {
                let all = Self::log(self.run(&name, &table, Vec::new()).await);
                all.into_iter().filter(|r| or_groups.iter().any(|g| matches_all(r, g))).collect()
            }
        }
    }

    async fn query(&self, location: Location, query: ListQuery) -> Vec<Value> {
        let (name, table) = self.table(&location, &query.key_field);
        match self.query_pipeline(&table, &query) {
            Some(pipeline) => Self::log(self.run(&name, &table, pipeline).await),
            None => typiql_core::query_in_memory(self, location, query).await,
        }
    }

    async fn aggregate(&self, spec: AggregateSpec, query: ListQuery) -> Vec<Value> {
        let (name, source) = self.table(&spec.source.as_str().into(), "");
        match Self::aggregate_pipeline(&spec, &query) {
            Some(pipeline) => Self::log(self.run(&name, &source, pipeline).await),
            None => {
                let rows = self.get_many(spec.source.as_str().into(), Vec::new()).await;
                let rows = spec.compute(rows).into_iter().filter(|r| matches_all(r, &query.filters)).collect();
                typiql_core::query::sort_and_page(rows, &query)
            }
        }
    }

    async fn aggregate_count(&self, spec: AggregateSpec, filters: Vec<FieldFilter>) -> usize {
        let query = ListQuery { filters, ..Default::default() };
        match Self::aggregate_pipeline(&spec, &query) {
            Some(pipeline) => {
                let (name, source) = self.table(&spec.source.as_str().into(), "");
                Self::log(self.run_count(&name, &source, pipeline).await)
            }
            None => self.aggregate(spec, query).await.len(),
        }
    }

    async fn count(&self, location: Location, filters: Vec<FieldFilter>) -> usize {
        let (name, table) = self.table(&location, "");
        match Self::match_stages(&table, &filters) {
            Some(pipeline) => Self::log(self.run_count(&name, &table, pipeline).await),
            None => self.get_many(location, filters).await.len(),
        }
    }

    async fn add(&self, location: Location, key_field: &str, values: Value) -> Result<Value, String> {
        self.insert(&location, key_field, vec![values]).await.map(|mut v| v.remove(0))
    }

    async fn add_many(&self, location: Location, key_field: &str, values: Vec<Value>) -> Result<Vec<Value>, String> {
        if values.is_empty() {
            return Ok(Vec::new());
        }
        self.insert(&location, key_field, values).await
    }

    async fn update(&self, location: Location, key_field: &str, key_value: &str, patch: Value) -> Option<Value> {
        self.update_many(location, key_field, vec![key_value.to_string()], patch).await.into_iter().next()
    }

    async fn update_many(&self, location: Location, key_field: &str, keys: Vec<String>, patch: Value) -> Vec<Value> {
        let (name, table) = self.table(&location, key_field);
        let table = Table { key: key_field.to_string(), ..table };
        let result = async {
            let coll = self.collection(&name, &table).await?;
            let filter = Self::keys_filter(&table, &keys);
            let set: Document = patch
                .as_object()
                .into_iter()
                .flatten()
                .filter(|(k, _)| *k != key_field && k.as_str() != ID)
                .map(|(k, v)| (k.clone(), to_bson(v)))
                .collect();
            if !set.is_empty() {
                coll.update_many(filter.clone(), doc! { "$set": set }).await.map_err(err)?;
            }
            self.run(&name, &table, vec![doc! { "$match": filter }]).await
        }
        .await;
        Self::log(result)
    }

    async fn remove(&self, location: Location, key_field: &str, key_value: &str) -> Option<Value> {
        let (name, table) = self.table(&location, key_field);
        let table = Table { key: key_field.to_string(), ..table };
        let result = async {
            let coll = self.collection(&name, &table).await?;
            let removed = coll.find_one_and_delete(Self::keys_filter(&table, &[key_value.to_string()])).await.map_err(err)?;
            Ok(removed.map(|d| row_json(d, &table)))
        }
        .await;
        Self::log(result)
    }

    async fn remove_many(&self, location: Location, key_field: &str, keys: Vec<String>) -> Vec<Value> {
        let (name, table) = self.table(&location, key_field);
        let table = Table { key: key_field.to_string(), ..table };
        let result = async {
            let coll = self.collection(&name, &table).await?;
            let docs: Vec<Document> = coll
                .find(Self::keys_filter(&table, &keys))
                .await
                .map_err(err)?
                .try_collect()
                .await
                .map_err(err)?;
            let ids: Vec<Bson> = docs.iter().filter_map(|d| d.get(ID).cloned()).collect();
            if !ids.is_empty() {
                coll.delete_many(doc! { ID: { "$in": ids } }).await.map_err(err)?;
            }
            Ok(docs.into_iter().map(|d| row_json(d, &table)).collect())
        }
        .await;
        Self::log(result)
    }

    async fn set_table(&self, location: Location, rows: Vec<Value>) {
        let (name, table) = self.table(&location, "");
        let result = async {
            let coll = self.collection(&name, &table).await?;
            coll.delete_many(doc! {}).await.map_err(err)?;
            if !rows.is_empty() {
                self.insert(&location, &table.key, rows).await?;
            }
            Ok(())
        }
        .await;
        Self::log(result)
    }

    fn register_type(&self, table: &str, key_field: &str, columns: &[(&'static str, &'static str)]) {
        let integer_key = columns.iter().any(|(name, ty)| *name == key_field && is_integer_type(ty));
        let columns = columns.iter().map(|(name, _)| name.to_string()).collect();
        self.inner
            .tables
            .write()
            .unwrap()
            .insert(table.to_string(), Table { key: key_field.to_string(), integer_key, columns });
    }
}

// ── filters ─────────────────────────────────────────────────────────────────

/// Translates filters into a `$match` document. An element at a position
/// (`_first`, `_last`, `_atIndex`) is picked into a temporary field first
/// (`$arrayElemAt`, as in C#), whose absence then reads as an empty element.
struct Filters<'a> {
    table: &'a Table,
    /// `$addFields` stages for positional elements, before the `$match`.
    stages: Vec<Document>,
    temps: Vec<String>,
}

impl Filters<'_> {
    /// Every filter (AND); `nested` inside `$elemMatch`, where a positional
    /// element can't be picked out (the in-memory path takes those).
    fn all(&mut self, filters: &[FieldFilter], prefix: &str, nested: bool) -> Option<Document> {
        let parts: Vec<Document> = filters.iter().map(|f| self.one(f, prefix, nested)).collect::<Option<_>>()?;
        Some(match parts.len() {
            0 => Document::new(),
            1 => parts.into_iter().next().unwrap_or_default(),
            _ => doc! { "$and": parts },
        })
    }

    fn one(&mut self, f: &FieldFilter, prefix: &str, nested: bool) -> Option<Document> {
        let path = format!("{prefix}{}", f.field);
        // Key forms (ObjectId, integer) only apply to the row's own fields.
        let top = prefix.is_empty() && !nested;
        let forms = |v: &Value| -> Vec<Bson> {
            if top {
                key_forms(self.table, &f.field, v)
            } else {
                vec![to_bson(v)]
            }
        };
        let list = |v: &Value| -> Vec<Bson> { v.as_array().into_iter().flatten().flat_map(forms).collect() };
        let first = |v: &Value| forms(v).into_iter().next().unwrap_or(Bson::Null);
        let cond: Document = match f.op {
            // As in memory: `eq null` is a stored null, `ne null` anything
            // else, missing included.
            FilterOp::Eq if f.value.is_null() => doc! { "$type": "null" },
            FilterOp::Ne if f.value.is_null() => doc! { "$not": { "$type": "null" } },
            // Mongo's `null` also means missing; a comparison with null
            // matches nothing in memory.
            FilterOp::Gt | FilterOp::Gte | FilterOp::Lt | FilterOp::Lte if f.value.is_null() => doc! { "$in": [] },
            FilterOp::In | FilterOp::NotIn if f.value.as_array().is_some_and(|a| a.iter().any(Value::is_null)) => {
                let rest: Vec<Value> = f.value.as_array().into_iter().flatten().filter(|v| !v.is_null()).cloned().collect();
                let values = list(&Value::Array(rest));
                return Some(if f.op == FilterOp::In {
                    doc! { "$or": [{ &path: { "$in": values } }, { &path: { "$type": "null" } }] }
                } else {
                    doc! { &path: { "$nin": values, "$not": { "$type": "null" } } }
                });
            }
            FilterOp::Eq => doc! { "$in": forms(&f.value) },
            FilterOp::Ne => doc! { "$nin": forms(&f.value) },
            FilterOp::Gt => doc! { "$gt": first(&f.value) },
            FilterOp::Gte => doc! { "$gte": first(&f.value) },
            FilterOp::Lt => doc! { "$lt": first(&f.value) },
            FilterOp::Lte => doc! { "$lte": first(&f.value) },
            FilterOp::In | FilterOp::AnyIn => doc! { "$in": list(&f.value) },
            FilterOp::NotIn | FilterOp::NoneIn => doc! { "$nin": list(&f.value) },
            FilterOp::StartsWith
            | FilterOp::EndsWith
            | FilterOp::Contains
            | FilterOp::NotStartsWith
            | FilterOp::NotEndsWith
            | FilterOp::NotContains => match f.value.as_str() {
                Some(needle) => regex(f.op, needle),
                None => doc! { "$in": [] },
            },
            FilterOp::ElemMatch => {
                let m: ElementMatch = serde_json::from_value(f.value.clone()).ok()?;
                let index = match m.at {
                    ElementAt::Any => {
                        let inner = self.all(&m.filters, "", true)?;
                        return Some(doc! { path: { "$elemMatch": inner } });
                    }
                    _ if nested => return None,
                    ElementAt::First => 0,
                    ElementAt::Last => -1,
                    ElementAt::Index(i) => i,
                };
                let temp = format!("__typiql_e{}", self.temps.len());
                let list = format!("${path}");
                self.stages.push(doc! {
                    "$addFields": { &temp: { "$cond": [{ "$isArray": &list }, { "$arrayElemAt": [&list, index] }, Bson::Null] } }
                });
                self.temps.push(temp.clone());
                return self.all(&m.filters, &format!("{temp}."), false);
            }
        };
        Some(doc! { path: cond })
    }
}

/// The pattern operators as case-insensitive regexes, as in C# — which only
/// ever match strings, so a missing value matches none of them, negated
/// ones included. (C# put the value into the pattern as is; here it's
/// escaped.)
fn regex(op: FilterOp, needle: &str) -> Document {
    let e = escape(needle);
    let (pattern, options) = match op {
        FilterOp::StartsWith => (format!("^{e}"), "i"),
        FilterOp::EndsWith => (format!("{e}\\z"), "i"),
        FilterOp::Contains => (e, "i"),
        FilterOp::NotStartsWith => (format!("^(?!{e})"), "i"),
        FilterOp::NotEndsWith => (format!("^(?!.*{e}\\z)"), "is"),
        _ => (format!("^(?!.*{e})"), "is"),
    };
    doc! { "$regex": pattern, "$options": options }
}

fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if "\\^$.|?*+()[]{}/-#".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// `$sort` (with the key breaking ties, as everywhere), `$skip`, `$limit`.
/// Without sort keys, rows stay in the collection's (insertion) order.
fn page_stages(sort: &[SortKey], key: &str, skip: usize, limit: Option<usize>) -> Vec<Document> {
    let mut stages = Vec::new();
    if !sort.is_empty() {
        let mut order = Document::new();
        for k in sort {
            if !order.contains_key(&k.field) {
                order.insert(k.field.clone(), if k.desc { -1 } else { 1 });
            }
        }
        if !key.is_empty() && !order.contains_key(key) {
            order.insert(key, 1);
        }
        stages.push(doc! { "$sort": order });
    }
    if skip > 0 {
        stages.push(doc! { "$skip": skip as i64 });
    }
    if let Some(l) = limit {
        stages.push(doc! { "$limit": l as i64 });
    }
    stages
}

// ── keys and values ─────────────────────────────────────────────────────────

/// A key given as text, as stored: an integer key's number; for `_id`, the
/// ObjectId a 24-digit hex string stands for.
fn stored_key(table: &Table, field: &str, v: &Value) -> Bson {
    key_forms(table, field, v).into_iter().next().unwrap_or(Bson::Null)
}

/// The forms a value of `field` may be stored in: an `_id` given as hex
/// may be an ObjectId (C#'s) or that string; an integer key given as text
/// is its number.
fn key_forms(table: &Table, field: &str, v: &Value) -> Vec<Bson> {
    if let Some(s) = v.as_str() {
        if field == table.key && table.integer_key {
            if let Ok(i) = s.parse::<i64>() {
                return vec![Bson::Int64(i)];
            }
        }
        if field == ID {
            if let Ok(id) = ObjectId::parse_str(s) {
                return vec![Bson::ObjectId(id), Bson::String(s.to_string())];
            }
        }
    }
    vec![to_bson(v)]
}

/// A row's key as text, if it has one.
fn key_text(row: &Value, key: &str) -> Option<String> {
    match row.get(key)? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn row_json(mut d: Document, table: &Table) -> Value {
    if !table.exposes_id() {
        d.remove(ID);
    }
    to_json(Bson::Document(d))
}

fn to_bson(v: &Value) -> Bson {
    match v {
        Value::Null => Bson::Null,
        Value::Bool(b) => Bson::Boolean(*b),
        Value::Number(n) => match n.as_i64() {
            Some(i) => Bson::Int64(i),
            None => Bson::Double(n.as_f64().unwrap_or(0.0)),
        },
        Value::String(s) => Bson::String(s.clone()),
        Value::Array(items) => Bson::Array(items.iter().map(to_bson).collect()),
        Value::Object(map) => Bson::Document(map.iter().map(|(k, v)| (k.clone(), to_bson(v))).collect()),
    }
}

/// BSON as JSON: ObjectIds as hex strings (as C# returned them), dates as
/// RFC 3339 text.
fn to_json(b: Bson) -> Value {
    match b {
        Bson::Null | Bson::Undefined | Bson::MinKey | Bson::MaxKey => Value::Null,
        Bson::Boolean(b) => Value::Bool(b),
        Bson::Int32(i) => Value::from(i),
        Bson::Int64(i) => Value::from(i),
        Bson::Double(f) => serde_json::Number::from_f64(f).map(Value::Number).unwrap_or(Value::Null),
        Bson::String(s) | Bson::Symbol(s) | Bson::JavaScriptCode(s) => Value::String(s),
        Bson::Array(items) => Value::Array(items.into_iter().map(to_json).collect()),
        Bson::Document(d) => Value::Object(d.into_iter().map(|(k, v)| (k, to_json(v))).collect::<Map<_, _>>()),
        Bson::ObjectId(id) => Value::String(id.to_hex()),
        Bson::DateTime(d) => d.try_to_rfc3339_string().map(Value::String).unwrap_or(Value::Null),
        Bson::Decimal128(d) => Value::String(d.to_string()),
        other => Value::String(other.to_string()),
    }
}

fn is_integer_type(ty: &str) -> bool {
    let base = ty.strip_suffix(" NOT NULL").unwrap_or(ty);
    matches!(
        base,
        "TINYINT" | "SMALLINT" | "INTEGER" | "BIGINT" | "HUGEINT" | "UTINYINT" | "USMALLINT" | "UINTEGER" | "UBIGINT" | "UHUGEINT"
    )
}

fn err(e: impl std::fmt::Display) -> String {
    format!("mongo adapter: {e}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn patterns_are_escaped() {
        assert_eq!(escape("a.b(c)*"), "a\\.b\\(c\\)\\*");
        assert_eq!(regex(FilterOp::NotContains, "x"), doc! { "$regex": "^(?!.*x)", "$options": "is" });
    }

    #[test]
    fn object_ids_read_as_hex() {
        let id = ObjectId::new();
        let row = row_json(doc! { ID: id, "name": "a" }, &Table { key: ID.into(), ..Default::default() });
        assert_eq!(row, json!({ "_id": id.to_hex(), "name": "a" }));
        let hidden = row_json(doc! { ID: id, "id": "k" }, &Table { key: "id".into(), ..Default::default() });
        assert_eq!(hidden, json!({ "id": "k" }));
    }

    #[test]
    fn id_filters_match_either_form() {
        let table = Table { key: ID.into(), ..Default::default() };
        let id = ObjectId::new();
        assert_eq!(key_forms(&table, ID, &json!(id.to_hex())), vec![Bson::ObjectId(id), Bson::String(id.to_hex())]);
        let ints = Table { key: "id".into(), integer_key: true, ..Default::default() };
        assert_eq!(key_forms(&ints, "id", &json!("7")), vec![Bson::Int64(7)]);
    }
}
