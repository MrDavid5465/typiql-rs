use std::sync::Arc;
use async_graphql::{Context, Result as GqlResult};
use serde_json::Value;
use crate::{TypiQLType, adapter::{FieldFilter, TypiQLAdapter}};

// resolve_get was removed — the macro's generated get-one resolver now goes
// through resolve_list (same filter-arg machinery as the list query, taking
// the first match) instead of an exact single-field key lookup. See
// typiql-macros' get_fn codegen.

pub async fn resolve_list<T: TypiQLType>(
    ctx: &Context<'_>,
    filters: Vec<FieldFilter>,
) -> GqlResult<Vec<T>> {
    let adapter = ctx.data::<Arc<dyn TypiQLAdapter>>()?;
    adapter
        .get_many(T::collection_name().into(), filters)
        .await
        .into_iter()
        .map(|v| {
            serde_json::from_value(v)
                .map_err(|e| async_graphql::Error::new(e.to_string()))
        })
        .collect()
}

pub async fn resolve_add<T: TypiQLType>(
    ctx: &Context<'_>,
    values: T::Input,
) -> GqlResult<T> {
    let adapter = ctx.data::<Arc<dyn TypiQLAdapter>>()?;
    let json = serde_json::to_value(values)?;
    // Strip null fields so only explicitly provided values are stored.
    let json = strip_nulls(json);
    let result = adapter
        .add(T::collection_name().into(), T::key_field(), json)
        .await
        .map_err(|e| async_graphql::Error::new(e))?;
    Ok(serde_json::from_value(result)?)
}

pub async fn resolve_update<T: TypiQLType>(
    ctx: &Context<'_>,
    key_value: String,
    update: T::Input,
) -> GqlResult<Option<T>> {
    let adapter = ctx.data::<Arc<dyn TypiQLAdapter>>()?;
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
        .update(T::collection_name().into(), T::key_field(), &key_value, patch)
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
    let adapter = ctx.data::<Arc<dyn TypiQLAdapter>>()?;
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
        Value::Object(map) => Value::Object(
            map.into_iter()
                .filter(|(_, v)| !v.is_null())
                .collect(),
        ),
        other => other,
    }
}
