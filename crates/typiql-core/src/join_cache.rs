//! Per-request storage for joined relation values.
//!
//! A join-mode relation's related rows arrive attached to the parent row
//! (see [`crate::query::Join`]); [`crate::resolve_query`] moves them here,
//! and the relation field's resolver takes them from here instead of
//! loading them again. Parents read any other way (mutation results,
//! subscription events, hand-written resolvers) simply miss the cache and
//! load normally.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_graphql::extensions::{
    Extension, ExtensionContext, ExtensionFactory, NextPrepareRequest,
};
use async_graphql::{Request, ServerResult};
use serde_json::Value;

/// Joined values for one request, by (parent type, parent key as JSON
/// text, relation storage name).
#[derive(Default)]
pub struct JoinCache(Mutex<HashMap<(&'static str, String, String), Value>>);

impl JoinCache {
    pub fn insert(
        &self,
        parent_type: &'static str,
        parent_key: &str,
        relation: &str,
        value: Value,
    ) {
        self.0.lock().unwrap().insert(
            (parent_type, parent_key.to_string(), relation.to_string()),
            value,
        );
    }

    pub fn get(
        &self,
        parent_type: &'static str,
        parent_key: &str,
        relation: &str,
    ) -> Option<Value> {
        self.0
            .lock()
            .unwrap()
            .get(&(parent_type, parent_key.to_string(), relation.to_string()))
            .cloned()
    }
}

/// Gives every request its own [`JoinCache`]; registered by
/// `typiql_schema!`'s `build_typiql_schema`.
pub struct JoinCacheExtension;

impl ExtensionFactory for JoinCacheExtension {
    fn create(&self) -> Arc<dyn Extension> {
        Arc::new(JoinCacheExtension)
    }
}

#[async_trait::async_trait]
impl Extension for JoinCacheExtension {
    async fn prepare_request(
        &self,
        ctx: &ExtensionContext<'_>,
        request: Request,
        next: NextPrepareRequest<'_>,
    ) -> ServerResult<Request> {
        next.run(ctx, request.data(JoinCache::default())).await
    }
}
