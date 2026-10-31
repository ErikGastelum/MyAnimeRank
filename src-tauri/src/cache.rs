//! FR-50: TTL cache for Discover/Feed. Port of cache.py. A second instance
//! holds Pick an anime's genre/tag lists (FR-84).
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
    store: Mutex<HashMap<String, (Instant, Value)>>,
}

impl TtlCache {
    pub async fn get_or_fetch<F, Fut, E>(&self, key: &str, ttl: Duration, fetch: F) -> Result<Value, E>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Value, E>>,
    {
        if let Some((at, value)) = self.lock().get(key) {
            if at.elapsed() < ttl {
                return Ok(value.clone());
            }
        }
        let value = fetch().await?;
        let mut store = self.lock();
        // Drop expired entries so the cache can't grow without bound.
        store.retain(|_, (at, _)| at.elapsed() < ttl);
        store.insert(key.to_string(), (Instant::now(), value.clone()));
        Ok(value)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, (Instant, Value)>> {
        self.store.lock().unwrap_or_else(|e| e.into_inner())
    }
}
