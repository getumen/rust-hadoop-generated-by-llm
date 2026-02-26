use lru::LruCache;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Cached signing key entry.
struct CacheEntry {
    key: Vec<u8>,
    expiry: Instant,
}

/// Thread-safe LRU cache for derived signing keys.
/// Keys are valid for 24 hours.
pub struct SigningKeyCache {
    cache: Arc<Mutex<LruCache<(String, String), CacheEntry>>>,
}

impl SigningKeyCache {
    pub fn new(capacity: usize) -> Self {
        Self {
            cache: Arc::new(Mutex::new(LruCache::new(
                NonZeroUsize::new(capacity).unwrap_or(NonZeroUsize::new(100).unwrap()),
            ))),
        }
    }

    pub fn get(&self, access_key: &str, date: &str) -> Option<Vec<u8>> {
        let mut cache = match self.cache.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                let mut guard = poisoned.into_inner();
                guard.clear();
                return None;
            }
        };

        let key = (access_key.to_string(), date.to_string());
        if let Some(entry) = cache.get(&key) {
            if entry.expiry > Instant::now() {
                return Some(entry.key.clone());
            }
            cache.pop(&key);
        }
        None
    }

    pub fn insert(&self, access_key: &str, date: &str, signing_key: Vec<u8>) {
        let mut cache = match self.cache.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };

        let key = (access_key.to_string(), date.to_string());
        let entry = CacheEntry {
            key: signing_key,
            expiry: Instant::now() + Duration::from_secs(24 * 3600),
        };
        cache.put(key, entry);
    }
}

impl Default for SigningKeyCache {
    fn default() -> Self {
        Self::new(100)
    }
}
