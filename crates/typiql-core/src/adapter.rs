use std::collections::HashMap;
use async_trait::async_trait;
use serde_json::Value;

#[async_trait]
pub trait TypiQLAdapter: Send + Sync {
    async fn get_one(
        &self,
        collection: &str,
        key_field: &str,
        key_value: &str,
    ) -> Option<Value>;

    async fn get_many(
        &self,
        collection: &str,
        filters: HashMap<String, Value>,
    ) -> Vec<Value>;

    async fn add(
        &self,
        collection: &str,
        key_field: &str,
        values: Value,
    ) -> Result<Value, String>;

    async fn update(
        &self,
        collection: &str,
        key_field: &str,
        key_value: &str,
        update: Value,
    ) -> Option<Value>;

    async fn remove(
        &self,
        collection: &str,
        key_field: &str,
        key_value: &str,
    ) -> Option<Value>;
}
