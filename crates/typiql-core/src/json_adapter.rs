use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use parking_lot::RwLock;
use serde_json::Value;
use async_trait::async_trait;
use uuid::Uuid;
use crate::adapter::TypiQLAdapter;

/// Stores each collection as a JSON array in `<data_dir>/<collection>.json`.
/// All data is cached in-memory; writes go to disk synchronously on mutation.
pub struct JsonAdapter {
    data_dir: PathBuf,
    store: Arc<RwLock<HashMap<String, Vec<Value>>>>,
}

impl JsonAdapter {
    pub fn new(data_dir: impl AsRef<Path>) -> Self {
        Self {
            data_dir: data_dir.as_ref().to_path_buf(),
            store: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    fn file_path(&self, collection: &str) -> PathBuf {
        self.data_dir.join(format!("{}.json", collection))
    }

    fn load_from_disk(&self, collection: &str) -> Vec<Value> {
        let path = self.file_path(collection);
        if path.exists() {
            let content = std::fs::read_to_string(&path).unwrap_or_default();
            serde_json::from_str(&content).unwrap_or_default()
        } else {
            vec![]
        }
    }

    fn flush_to_disk(&self, collection: &str, data: &[Value]) {
        let path = self.file_path(collection);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let content = serde_json::to_string_pretty(data).unwrap_or_default();
        std::fs::write(&path, content).ok();
    }

    fn read(&self, collection: &str) -> Vec<Value> {
        {
            let store = self.store.read();
            if let Some(data) = store.get(collection) {
                return data.clone();
            }
        }
        let data = self.load_from_disk(collection);
        self.store.write().insert(collection.to_string(), data.clone());
        data
    }

    fn write(&self, collection: &str, data: Vec<Value>) {
        self.flush_to_disk(collection, &data);
        self.store.write().insert(collection.to_string(), data);
    }

    fn matches(item: &Value, filters: &HashMap<String, Value>) -> bool {
        filters.iter().all(|(key, val)| item.get(key) == Some(val))
    }
}

#[async_trait]
impl TypiQLAdapter for JsonAdapter {
    async fn get_one(
        &self,
        collection: &str,
        key_field: &str,
        key_value: &str,
    ) -> Option<Value> {
        self.read(collection).into_iter().find(|item| {
            item.get(key_field)
                .and_then(Value::as_str)
                .map(|s| s == key_value)
                .unwrap_or(false)
        })
    }

    async fn get_many(
        &self,
        collection: &str,
        filters: HashMap<String, Value>,
    ) -> Vec<Value> {
        let data = self.read(collection);
        if filters.is_empty() {
            data
        } else {
            data.into_iter()
                .filter(|item| Self::matches(item, &filters))
                .collect()
        }
    }

    async fn add(
        &self,
        collection: &str,
        key_field: &str,
        mut values: Value,
    ) -> Result<Value, String> {
        let mut data = self.read(collection);

        // Auto-assign UUID if the key is absent or empty.
        let needs_id = values
            .get(key_field)
            .and_then(Value::as_str)
            .map(str::is_empty)
            .unwrap_or(true);

        if needs_id {
            if let Value::Object(ref mut map) = values {
                map.insert(
                    key_field.to_string(),
                    Value::String(Uuid::new_v4().to_string()),
                );
            }
        }

        data.push(values.clone());
        self.write(collection, data);
        Ok(values)
    }

    async fn update(
        &self,
        collection: &str,
        key_field: &str,
        key_value: &str,
        patch: Value,
    ) -> Option<Value> {
        let mut data = self.read(collection);
        let mut updated: Option<Value> = None;

        for item in &mut data {
            let is_match = item
                .get(key_field)
                .and_then(Value::as_str)
                .map(|s| s == key_value)
                .unwrap_or(false);

            if is_match {
                if let (Value::Object(item_map), Value::Object(patch_map)) =
                    (item, &patch)
                {
                    for (k, v) in patch_map {
                        if k != key_field {
                            item_map.insert(k.clone(), v.clone());
                        }
                    }
                    updated = Some(Value::Object(item_map.clone()));
                }
                break;
            }
        }

        if updated.is_some() {
            self.write(collection, data);
        }
        updated
    }

    async fn remove(
        &self,
        collection: &str,
        key_field: &str,
        key_value: &str,
    ) -> Option<Value> {
        let data = self.read(collection);
        let mut removed: Option<Value> = None;

        let remaining: Vec<Value> = data
            .into_iter()
            .filter(|item| {
                let is_match = item
                    .get(key_field)
                    .and_then(Value::as_str)
                    .map(|s| s == key_value)
                    .unwrap_or(false);
                if is_match {
                    removed = Some(item.clone());
                }
                !is_match
            })
            .collect();

        if removed.is_some() {
            self.write(collection, remaining);
        }
        removed
    }
}
