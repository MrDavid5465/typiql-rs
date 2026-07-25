use crate::adapter::{FieldFilter, FilterOp, TypiQLAdapter};
use crate::TypiQLType;
use async_graphql::dataloader::Loader;
use serde::de::DeserializeOwned;
use std::collections::HashMap;
use std::future::Future;
use std::marker::PhantomData;
use std::sync::Arc;

/// Generic DataLoader-compatible batch loader for any relation target type.
/// The key is the composite set of (foreign_field, stringified_value) pairs
/// a relation field needs to match — a single-condition relation just uses a
/// one-element Vec. Batches every parent row's key across one GraphQL
/// execution into a single `get_many_matching_any` call, avoiding N+1.
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

impl<T> Loader<Vec<(String, String)>> for RelationLoader<T>
where
    T: TypiQLType + DeserializeOwned + Clone + Send + Sync + 'static,
{
    type Value = T;
    type Error = Arc<str>;

    fn load(
        &self,
        keys: &[Vec<(String, String)>],
    ) -> impl Future<Output = Result<HashMap<Vec<(String, String)>, T>, Self::Error>> + Send {
        let adapter = self.adapter.clone();
        let keys: Vec<Vec<(String, String)>> = keys.to_vec();
        async move {
            let or_groups: Vec<Vec<FieldFilter>> = keys
                .iter()
                .map(|conditions| {
                    conditions
                        .iter()
                        .map(|(field, value)| FieldFilter {
                            field: field.clone(),
                            op: FilterOp::Eq,
                            value: serde_json::Value::String(value.clone()),
                        })
                        .collect()
                })
                .collect();

            let results = adapter
                .get_many_matching_any(T::collection_name().into(), or_groups)
                .await;

            let mut out = HashMap::new();
            for key in &keys {
                for result in &results {
                    let matches = key.iter().all(|(field, value)| {
                        result.get(field).and_then(|v| v.as_str()) == Some(value.as_str())
                    });
                    if matches {
                        if let Ok(item) = serde_json::from_value::<T>(result.clone()) {
                            out.insert(key.clone(), item);
                        }
                        break;
                    }
                }
            }
            Ok(out)
        }
    }
}
