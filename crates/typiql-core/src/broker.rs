use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt as _;

static BROKERS: OnceLock<Mutex<HashMap<TypeId, Arc<dyn Any + Send + Sync>>>> = OnceLock::new();

fn global() -> &'static Mutex<HashMap<TypeId, Arc<dyn Any + Send + Sync>>> {
    BROKERS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// A global per-type pub/sub bus backed by `tokio::sync::broadcast`.
///
/// Analogous to `async_graphql::SimpleBroker` (removed in 7.x).
/// Use `publish` in mutations and `subscribe` in subscription resolvers.
pub struct TypiQLBroker<T>(std::marker::PhantomData<T>);

impl<T: Clone + Send + Sync + 'static> TypiQLBroker<T> {
    fn sender() -> Arc<broadcast::Sender<T>> {
        let type_id = TypeId::of::<T>();
        let mut map = global().lock().unwrap();
        if let Some(any) = map.get(&type_id) {
            // Safety: we inserted this Arc<broadcast::Sender<T>> keyed by TypeId::of::<T>()
            any.clone()
                .downcast::<broadcast::Sender<T>>()
                .expect("typiql broker type mismatch")
        } else {
            let (tx, _) = broadcast::channel::<T>(256);
            let arc = Arc::new(tx);
            map.insert(type_id, arc.clone());
            arc
        }
    }

    /// Broadcast a change event to all active subscribers.
    pub fn publish(msg: T) {
        let _ = Self::sender().send(msg);
    }

    /// Subscribe to change events. Missed events (when the buffer is full)
    /// are silently dropped via `filter_map`.
    pub fn subscribe() -> impl futures_util::Stream<Item = T> + Send + 'static {
        BroadcastStream::new(Self::sender().subscribe()).filter_map(|r| r.ok())
    }
}
