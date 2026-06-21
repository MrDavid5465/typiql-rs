use std::collections::HashMap;
use std::sync::Arc;
use async_graphql::{Context, Result as GqlResult};
use serde_json::Value;
use crate::{TypiQLType, adapter::TypiQLAdapter};

pub async fn resolve_get<T: TypiQLType>(
    ctx: &Context<'_>,
    key_value: String,
) -> GqlResult<Option<T>> {
    let adapter = ctx.data::<Arc<dyn TypiQLAdapter>>()?;
    match adapter
        .get_one(T::collection_name(), T::key_field(), &key_value)
        .await
    {
        Some(v) => Ok(Some(serde_json::from_value(v)?)),
        None => Ok(None),
    }
}

pub async fn resolve_list<T: TypiQLType>(
    ctx: &Context<'_>,
    filters: HashMap<String, Value>,
) -> GqlResult<Vec<T>> {
    let adapter = ctx.data::<Arc<dyn TypiQLAdapter>>()?;
    adapter
        .get_many(T::collection_name(), filters)
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
        .add(T::collection_name(), T::key_field(), json)
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
    let json = serde_json::to_value(update)?;
    let patch = strip_nulls(json);
    match adapter
        .update(T::collection_name(), T::key_field(), &key_value, patch)
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
        .remove(T::collection_name(), T::key_field(), &key_value)
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
