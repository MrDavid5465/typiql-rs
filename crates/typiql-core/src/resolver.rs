use crate::{
    adapter::{FieldFilter, TypiQLAdapter},
    TypiQLType,
};
use async_graphql::{Context, Result as GqlResult};
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

pub async fn resolve_add<T: TypiQLType>(ctx: &Context<'_>, values: T::Input) -> GqlResult<T> {
    let adapter = adapter_for::<T>(ctx)?;
    let json = serde_json::to_value(values)?;
    // Strip null fields so only explicitly provided values are stored.
    let json = strip_nulls(json);
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
    let adapter = adapter_for::<T>(ctx)?;
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
    let patch = serde_json::to_value(update)?;
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

pub async fn resolve_remove<T: TypiQLType>(
    ctx: &Context<'_>,
    key_value: String,
) -> GqlResult<Option<T>> {
    let adapter = adapter_for::<T>(ctx)?;
    match adapter
        .remove(T::collection_name().into(), T::key_field(), &key_value)
        .await
    {
        Some(v) => Ok(Some(serde_json::from_value(v)?)),
        None => Ok(None),
    }
}

fn strip_nulls(v: Value) -> Value {
    match v {
        Value::Object(map) => {
            Value::Object(map.into_iter().filter(|(_, v)| !v.is_null()).collect())
        }
        other => other,
    }
}
