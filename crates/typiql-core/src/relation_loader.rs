use crate::adapter::{FieldFilter, FilterOp, TypiQLAdapter};
use crate::query::{matches_all, sort_and_page, ListQuery, SortKey};
use crate::TypiQLType;
use async_graphql::dataloader::Loader;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::collections::HashMap;
use std::future::Future;
use std::marker::PhantomData;
use std::sync::Arc;

/// One condition of a has-one relation, already resolved against its parent
/// row: the target's `field` (storage name) `op` `value`. The value is kept
/// as JSON text so keys can be hashed.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RelationCondition {
    pub field: String,
    pub op: FilterOp,
    pub value: String,
}

impl RelationCondition {
    pub fn new(field: impl Into<String>, op: FilterOp, value: &Value) -> Self {
        RelationCondition {
            field: field.into(),
            op,
            value: value.to_string(),
        }
    }

    fn filter(&self) -> FieldFilter {
        FieldFilter {
            field: self.field.clone(),
            op: self.op,
            value: serde_json::from_str(&self.value).unwrap_or(Value::Null),
        }
    }
}

/// What one parent row needs from a has-one relation: the target row that
/// matches every condition, first by `sort` (storage name, descending?)
/// after skipping `skip` — C#'s relation arguments, including their
/// `_orderBy`/`_orderBy_desc`/`_start` (e.g. "latest visit").
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RelationKey {
    pub conditions: Vec<RelationCondition>,
    pub sort: Vec<(String, bool)>,
    pub skip: usize,
}

/// Batches every parent's has-one lookup in one GraphQL execution into a
/// single adapter call: each key's conditions become one OR-group of
/// `get_many_matching_any`, and the candidates it returns are matched back
/// to each key in memory, with the reference filter semantics.
pub struct RelationLoader<T> {
    adapter: Arc<dyn TypiQLAdapter>,
    _marker: PhantomData<T>,
}

impl<T> RelationLoader<T> {
    pub fn new(adapter: Arc<dyn TypiQLAdapter>) -> Self {
        Self {
            adapter,
            _marker: PhantomData,
        }
    }
}

impl<T> Loader<RelationKey> for RelationLoader<T>
where
    T: TypiQLType + DeserializeOwned + Clone + Send + Sync + 'static,
{
    type Value = T;
    type Error = Arc<str>;

    fn load(
        &self,
        keys: &[RelationKey],
    ) -> impl Future<Output = Result<HashMap<RelationKey, T>, Self::Error>> + Send {
        let adapter = self.adapter.clone();
        let keys: Vec<RelationKey> = keys.to_vec();
        async move {
            let groups: Vec<Vec<FieldFilter>> = keys
                .iter()
                .map(|k| k.conditions.iter().map(RelationCondition::filter).collect())
                .collect();
            let candidates = adapter
                .get_many_matching_any(T::collection_name().into(), groups.clone())
                .await;

            let mut out = HashMap::new();
            for (key, group) in keys.iter().zip(&groups) {
                let matching: Vec<Value> = candidates
                    .iter()
                    .filter(|row| matches_all(row, group))
                    .cloned()
                    .collect();
                let query = ListQuery {
                    sort: key
                        .sort
                        .iter()
                        .map(|(field, desc)| SortKey {
                            field: field.clone(),
                            desc: *desc,
                        })
                        .collect(),
                    skip: key.skip,
                    limit: Some(1),
                    key_field: T::key_field().to_string(),
                    ..Default::default()
                };
                if let Some(row) = sort_and_page(matching, &query).into_iter().next() {
                    if let Ok(item) = serde_json::from_value::<T>(row) {
                        out.insert(key.clone(), item);
                    }
                }
            }
            Ok(out)
        }
    }
}
