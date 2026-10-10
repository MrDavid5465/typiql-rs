use crate::{
    adapter::{FieldFilter, FilterOp, Location, TypiQLAdapter},
    join_cache::JoinCache,
    query::{sort_and_page, ListQuery, PageInfo, SortKey},
    TypiQLType,
};
use async_graphql::{Context, Result as GqlResult};
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

/// Every registered adapter, keyed by name (see `typiql_schema!`'s generated
/// `build_typiql_schema`). Most apps only ever register `"default"`; a
/// multi-adapter app (e.g. JSON + DuckDB) registers one entry per adapter,
/// and each type's `T::adapter_name()` picks which one serves it.
pub type AdapterMap = HashMap<&'static str, Arc<dyn TypiQLAdapter>>;

fn adapter_for<T: TypiQLType>(ctx: &Context<'_>) -> GqlResult<Arc<dyn TypiQLAdapter>> {
    let adapters = ctx.data::<AdapterMap>()?;
    adapters.get(T::adapter_name()).cloned().ok_or_else(|| {
        async_graphql::Error::new(format!(
            "no adapter registered under name {:?} (needed by type {:?})",
            T::adapter_name(),
            T::type_name(),
        ))
    })
}

// resolve_get was removed — the macro's generated get-one resolver now goes
// through resolve_list (same filter-arg machinery as the list query, taking
// the first match) instead of an exact single-field key lookup. See
// typiql-macros' get_fn codegen.

pub async fn resolve_list<T: TypiQLType>(
    ctx: &Context<'_>,
    filters: Vec<FieldFilter>,
) -> GqlResult<Vec<T>> {
    let adapter = adapter_for::<T>(ctx)?;
    adapter
        .get_many(T::collection_name().into(), filters)
        .await
        .into_iter()
        .map(|v| serde_json::from_value(v).map_err(|e| async_graphql::Error::new(e.to_string())))
        .collect()
}

/// A sorted, paged list read — see [`crate::query`]. Joins `T`'s join-mode
/// relations (relation fields with a `data_name`) into each row: those on
/// `T`'s own adapter by that adapter (which may push them into its store),
/// the rest here. When the sort depends on a join done here, so does the
/// sorting and paging — results never depend on which adapter serves what.
/// Joined values go to the request's [`JoinCache`] for the relation fields
/// to pick up.
pub async fn resolve_query<T: TypiQLType>(ctx: &Context<'_>, mut query: ListQuery) -> GqlResult<Vec<T>> {
    let adapter = adapter_for::<T>(ctx)?;
    if let Some(spec) = T::aggregate() {
        return adapter
            .aggregate(spec, query)
            .await
            .into_iter()
            .map(|v| serde_json::from_value(v).map_err(|e| async_graphql::Error::new(e.to_string())))
            .collect();
    }
    let adapters = ctx.data::<AdapterMap>()?;
    // Only join what this query uses: a selected relation field, or one
    // the sort goes through.
    let look = ctx.look_ahead();
    let joins: Vec<_> = T::joins(ctx)?
        .into_iter()
        .filter(|j| {
            look.field(&j.graphql_name).exists()
                || query.sort.iter().any(|k| k.field.split('.').next() == Some(j.name.as_str()))
        })
        .collect();
    let mut local = Vec::new();
    let mut remote = Vec::new();
    for join in joins.iter().cloned() {
        let target = adapters.get(join.target_adapter).cloned().ok_or_else(|| {
            async_graphql::Error::new(format!("no adapter registered under name {:?}", join.target_adapter))
        })?;
        if Arc::ptr_eq(&target, &adapter) {
            local.push(join);
        } else {
            remote.push((join, target));
        }
    }
    let sorts_by_remote = query.sort.iter().any(|k| {
        remote.iter().any(|(j, _)| k.field.split('.').next() == Some(j.name.as_str()))
    });

    let location: Location = T::collection_name().into();
    let mut rows = if sorts_by_remote {
        let unpaged = ListQuery {
            filters: query.filters.clone(),
            key_field: query.key_field.clone(),
            joins: local,
            ..Default::default()
        };
        adapter.query(location, unpaged).await
    } else {
        query.joins = local;
        adapter.query(location, query.clone()).await
    };
    for (join, target) in &remote {
        let candidates = target
            .get_many_matching_any(join.target.as_str().into(), join.candidate_groups(&rows))
            .await;
        join.attach(&mut rows, &candidates);
    }
    if sorts_by_remote {
        query.joins = Vec::new();
        rows = sort_and_page(rows, &query);
    }

    if let Some(cache) = ctx.data_opt::<JoinCache>() {
        for row in &rows {
            let key = row.get(T::key_field()).map(Value::to_string).unwrap_or_default();
            for join in &joins {
                if let Some(v) = row.get(&join.name) {
                    cache.insert(T::type_name(), &key, &join.name, v.clone());
                }
            }
        }
    }
    rows.into_iter()
        .map(|v| serde_json::from_value(v).map_err(|e| async_graphql::Error::new(e.to_string())))
        .collect()
}

/// `<list>PageInfo` for the same arguments as a list read.
pub async fn resolve_page_info<T: TypiQLType>(ctx: &Context<'_>, query: ListQuery) -> GqlResult<PageInfo> {
    let adapter = adapter_for::<T>(ctx)?;
    let total = match T::aggregate() {
        Some(spec) => adapter.aggregate_count(spec, query.filters).await,
        None => adapter.count(T::collection_name().into(), query.filters).await,
    };
    Ok(PageInfo::new(total, query.skip, query.limit))
}

pub async fn resolve_add<T: TypiQLType>(ctx: &Context<'_>, values: T::Input) -> GqlResult<T> {
    resolve_add_with::<T>(ctx, values, Vec::new()).await
}

/// `resolve_add` plus server-side values (an operation's declared `set(...)`,
/// already resolved) that overwrite whatever the client sent for those fields.
pub async fn resolve_add_with<T: TypiQLType>(
    ctx: &Context<'_>,
    values: T::Input,
    set: Vec<(&'static str, Value)>,
) -> GqlResult<T> {
    let adapter = adapter_for::<T>(ctx)?;
    let json = serde_json::to_value(values)?;
    // Strip null fields so only explicitly provided values are stored.
    let json = complete_row::<T>(apply_set(strip_nulls(json), set))?;
    let result = adapter
        .add(T::collection_name().into(), T::key_field(), json)
        .await
        .map_err(async_graphql::Error::new)?;
    Ok(serde_json::from_value(result)?)
}

/// Bulk-insert variant of `resolve_add` — takes whole `T` rows rather than
/// `T::Input` (there's no per-field omit-vs-null patch semantic for a fresh
/// insert, and callers building many rows programmatically, e.g. recording
/// frames, already have fully-formed structs, not GraphQL Input values).
/// Goes through the adapter's real `add_many` (e.g. DuckDB's `Appender`),
/// not a naive one-`add`-per-row loop, wherever the adapter provides one.
pub async fn resolve_add_many<T: TypiQLType>(
    ctx: &Context<'_>,
    values: Vec<T>,
) -> GqlResult<Vec<T>> {
    let adapter = adapter_for::<T>(ctx)?;
    let jsons: Vec<Value> = values
        .into_iter()
        .map(|v| serde_json::to_value(v).map_err(|e| async_graphql::Error::new(e.to_string())))
        .collect::<GqlResult<_>>()?;
    let results = adapter
        .add_many(T::collection_name().into(), T::key_field(), jsons)
        .await
        .map_err(async_graphql::Error::new)?;
    results
        .into_iter()
        .map(|v| serde_json::from_value(v).map_err(|e| async_graphql::Error::new(e.to_string())))
        .collect()
}

pub async fn resolve_update<T: TypiQLType>(
    ctx: &Context<'_>,
    key_value: String,
    update: T::Input,
) -> GqlResult<Option<T>> {
    resolve_update_with::<T>(ctx, key_value, update, Vec::new(), Vec::new()).await
}

/// `resolve_update` restricted to a record that also matches `scope` (an
/// operation's declared `scope(...)`, already resolved) — a key outside the
/// scope behaves exactly like a missing key — with `set` values overwriting
/// the client's patch for those fields.
pub async fn resolve_update_with<T: TypiQLType>(
    ctx: &Context<'_>,
    key_value: String,
    update: T::Input,
    scope: Vec<FieldFilter>,
    set: Vec<(&'static str, Value)>,
) -> GqlResult<Option<T>> {
    let adapter = adapter_for::<T>(ctx)?;
    if !in_scope::<T>(&adapter, &key_value, scope).await {
        return Ok(None);
    }
    // No strip_nulls here (unlike resolve_add) — Input fields are now
    // MaybeUndefined<T> with #[serde(skip_serializing_if =
    // "MaybeUndefined::is_undefined")] (see typiql-macros' input_fields
    // codegen), so `serde_json::to_value(update)` already produces exactly
    // the right patch shape on its own: omitted (Undefined) fields are
    // fully absent as keys, explicitly-cleared (Null) fields are present
    // with JSON null, and provided (Value) fields are present with their
    // value. Stripping nulls here would silently re-collapse "explicitly
    // clear this field" back into "leave it untouched", reintroducing the
    // exact bug this type was adopted to fix.
    let patch = apply_set(serde_json::to_value(update)?, set);
    match adapter
        .update(
            T::collection_name().into(),
            T::key_field(),
            &key_value,
            patch,
        )
        .await
    {
        Some(v) => Ok(Some(serde_json::from_value(v)?)),
        None => Ok(None),
    }
}

/// An update that finds its row by `identify` (equality filters: the key,
/// or an update's declared `by(...)` fields — possibly none) plus `scope`,
/// updating the first match. With `upsert` (C#'s `_upsert`, Mongo's
/// `IsUpsert`), a missing row is created instead, from the update's values,
/// the equality values of `identify` and `scope`, and `set` — which wins,
/// as it does on update. A key comes from `identify` if it names one, else
/// the adapter generates it. Two concurrent upserts identified by non-key
/// fields can both insert, as in Mongo; keyed ones can't (keys are unique).
pub async fn resolve_upsert<T: TypiQLType>(
    ctx: &Context<'_>,
    identify: Vec<FieldFilter>,
    update: T::Input,
    scope: Vec<FieldFilter>,
    set: Vec<(&'static str, Value)>,
    upsert: bool,
) -> GqlResult<Option<T>> {
    let adapter = adapter_for::<T>(ctx)?;
    let patch = serde_json::to_value(update)?;
    let mut filters = identify.clone();
    filters.extend(scope.iter().cloned());
    let found = adapter
        .query(
            T::collection_name().into(),
            ListQuery {
                filters,
                sort: vec![SortKey { field: T::key_field().to_string(), desc: false }],
                limit: Some(1),
                key_field: T::key_field().to_string(),
                ..Default::default()
            },
        )
        .await
        .into_iter()
        .next();

    if let Some(row) = found {
        let key = match row.get(T::key_field()) {
            Some(Value::String(k)) => k.clone(),
            Some(other) => other.to_string(),
            None => return Ok(None),
        };
        let patch = apply_set(patch, set);
        return match adapter.update(T::collection_name().into(), T::key_field(), &key, patch).await {
            Some(v) => Ok(Some(serde_json::from_value(v)?)),
            None => Ok(None),
        };
    }
    if !upsert {
        return Ok(None);
    }

    let mut row = strip_nulls(patch);
    if let Value::Object(map) = &mut row {
        for f in identify.iter().chain(&scope).filter(|f| f.op == FilterOp::Eq) {
            map.insert(f.field.clone(), f.value.clone());
        }
    }
    let row = complete_row::<T>(apply_set(row, set))?;
    let added = adapter
        .add(T::collection_name().into(), T::key_field(), row)
        .await
        .map_err(async_graphql::Error::new)?;
    Ok(Some(serde_json::from_value(added)?))
}

/// C#'s AddMany: every item (plus `set`) inserted all-or-nothing through
/// the adapter's `add_many` — one duplicate key rejects the whole batch.
pub async fn resolve_add_many_inputs<T: TypiQLType>(
    ctx: &Context<'_>,
    items: Vec<T::Input>,
    set: Vec<(&'static str, Value)>,
) -> GqlResult<Vec<T>> {
    let adapter = adapter_for::<T>(ctx)?;
    let rows = items
        .into_iter()
        .map(|item| complete_row::<T>(apply_set(strip_nulls(serde_json::to_value(item)?), set.clone())))
        .collect::<GqlResult<Vec<Value>>>()?;
    let added = adapter
        .add_many(T::collection_name().into(), T::key_field(), rows)
        .await
        .map_err(async_graphql::Error::new)?;
    added.into_iter().map(|v| Ok(serde_json::from_value(v)?)).collect()
}

/// The keys of every row matching `filters` — the rows a bulk update or
/// remove acts on. No filters at all is refused: a bulk change to every
/// row has to be asked for with a filter that says so.
async fn matching_keys<T: TypiQLType>(
    adapter: &Arc<dyn TypiQLAdapter>,
    operation: &str,
    filters: Vec<FieldFilter>,
) -> GqlResult<Vec<String>> {
    if filters.is_empty() {
        return Err(async_graphql::Error::new(format!(
            "`{operation}` needs at least one filter — without one it would change every row"
        )));
    }
    Ok(adapter
        .get_many(T::collection_name().into(), filters)
        .await
        .iter()
        .filter_map(|row| match row.get(T::key_field()) {
            Some(Value::String(k)) => Some(k.clone()),
            Some(Value::Null) | None => None,
            Some(other) => Some(other.to_string()),
        })
        .collect())
}

/// C#'s UpdateMany: one patch (plus `set`) applied to every row matching
/// `filters` (the operation's filter arguments and scope), returning them.
pub async fn resolve_update_many<T: TypiQLType>(
    ctx: &Context<'_>,
    operation: &str,
    filters: Vec<FieldFilter>,
    update: T::Input,
    set: Vec<(&'static str, Value)>,
) -> GqlResult<Vec<T>> {
    let adapter = adapter_for::<T>(ctx)?;
    let keys = matching_keys::<T>(&adapter, operation, filters).await?;
    let patch = apply_set(serde_json::to_value(update)?, set);
    adapter
        .update_many(T::collection_name().into(), T::key_field(), keys, patch)
        .await
        .into_iter()
        .map(|v| Ok(serde_json::from_value(v)?))
        .collect()
}

/// C#'s RemoveMany: every row matching `filters` removed and returned.
pub async fn resolve_remove_many<T: TypiQLType>(
    ctx: &Context<'_>,
    operation: &str,
    filters: Vec<FieldFilter>,
) -> GqlResult<Vec<T>> {
    let adapter = adapter_for::<T>(ctx)?;
    let keys = matching_keys::<T>(&adapter, operation, filters).await?;
    adapter
        .remove_many(T::collection_name().into(), T::key_field(), keys)
        .await
        .into_iter()
        .map(|v| Ok(serde_json::from_value(v)?))
        .collect()
}

pub async fn resolve_remove<T: TypiQLType>(
    ctx: &Context<'_>,
    key_value: String,
) -> GqlResult<Option<T>> {
    resolve_remove_with::<T>(ctx, key_value, Vec::new()).await
}

/// `resolve_remove` restricted to a record that also matches `scope`.
pub async fn resolve_remove_with<T: TypiQLType>(
    ctx: &Context<'_>,
    key_value: String,
    scope: Vec<FieldFilter>,
) -> GqlResult<Option<T>> {
    let adapter = adapter_for::<T>(ctx)?;
    if !in_scope::<T>(&adapter, &key_value, scope).await {
        return Ok(None);
    }
    match adapter
        .remove(T::collection_name().into(), T::key_field(), &key_value)
        .await
    {
        Some(v) => Ok(Some(serde_json::from_value(v)?)),
        None => Ok(None),
    }
}

/// Whether the record keyed `key_value` also matches every `scope` filter.
/// No scope means no restriction (and no extra lookup).
async fn in_scope<T: TypiQLType>(
    adapter: &Arc<dyn TypiQLAdapter>,
    key_value: &str,
    mut scope: Vec<FieldFilter>,
) -> bool {
    if scope.is_empty() {
        return true;
    }
    scope.push(FieldFilter {
        field: T::key_field().to_string(),
        op: FilterOp::Eq,
        value: T::key_json(key_value),
    });
    !adapter
        .get_many(T::collection_name().into(), scope)
        .await
        .is_empty()
}

/// Whether `item` matches every `scope` filter — used to keep a scoped
/// subscription from streaming records outside its scope. Scope filters are
/// always equality (see `typiql-macros`' `scope(...)`).
pub fn matches_scope<T: Serialize>(item: &T, scope: &[FieldFilter]) -> bool {
    if scope.is_empty() {
        return true;
    }
    let Ok(json) = serde_json::to_value(item) else {
        return false;
    };
    scope
        .iter()
        .all(|f| json.get(&f.field).is_some_and(|v| v == &f.value))
}

/// Completes a new row to `T`'s full shape before it's inserted: every field
/// the client left out gets the default it would read back as (`[]`, `0`,
/// `""`, `null`, ...). Adapters then store identical rows — a schema-bound
/// store (DuckDB's `NOT NULL` columns) never sees a missing field that a
/// schemaless one (JSON) would just omit. A missing key stays missing, for
/// the adapter to assign.
fn complete_row<T: TypiQLType>(row: Value) -> GqlResult<Value> {
    let key = T::key_field();
    let has_key = row.get(key).is_some_and(|k| !k.is_null());
    let mut probe = row.clone();
    if let Value::Object(map) = &mut probe {
        if !has_key {
            map.insert(key.to_string(), T::key_placeholder());
        }
    }
    let full: T = serde_json::from_value(probe)?;
    let mut out = serde_json::to_value(full)?;
    if let (Value::Object(map), false) = (&mut out, has_key) {
        map.remove(key);
    }
    Ok(out)
}

fn apply_set(mut json: Value, set: Vec<(&'static str, Value)>) -> Value {
    if let Value::Object(map) = &mut json {
        for (field, value) in set {
            map.insert(field.to_string(), value);
        }
    }
    json
}

fn strip_nulls(v: Value) -> Value {
    match v {
        Value::Object(map) => {
            Value::Object(map.into_iter().filter(|(_, v)| !v.is_null()).collect())
        }
        other => other,
    }
}
