//! FR-50: TTL cache for Discover/Feed, and Pick an anime's genre/tag lists
//! (FR-84). Port of cache.py. Each entry keeps the TTL it was stored with, so
//! Feed's 60 s, Discover's 10 minutes and the pick lists' 24 hours share one.
//!
//! Not used for Favorites (FR-40: "not cached") or any Library/sync data. It
//! exists purely to keep Discover/Feed tab-switching inside AniList's rate limit.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::Value;

#[derive(Default)]
pub struct TtlCache {
    /// key -> (expires at, value)
    store: Mutex<HashMap<String, (Instant, Value)>>,
}

impl TtlCache {
    pub async fn get_or_fetch<F, Fut, E>(&self, key: &str, ttl: Duration, fetch: F) -> Result<Value, E>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Value, E>>,
    {
        if let Some((expires, value)) = self.lock().get(key) {
            if Instant::now() < *expires {
                return Ok(value.clone());
            }
        }
        let value = fetch().await?;
        let mut store = self.lock();
        // Drop expired entries so the cache can't grow without bound.
        let now = Instant::now();
        store.retain(|_, (expires, _)| now < *expires);
        store.insert(key.to_string(), (now + ttl, value.clone()));
        Ok(value)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, (Instant, Value)>> {
        self.store.lock().unwrap_or_else(|e| e.into_inner())
    }
}
