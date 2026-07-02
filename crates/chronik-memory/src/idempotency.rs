//! Best-effort in-process idempotency cache.
//!
//! Purpose: drop exact-duplicate ingests within a short window without round-tripping
//! to Kafka. This is **not** a strict idempotency guarantee — it is per-process and
//! lossy on restart. For cross-process strictness, callers pass `external_id` and
//! the SDK uses it as the Kafka record key (compaction handles dedup at rest).
//!
//! Implementation: LRU cache with TTL stored as the value type (`Instant`). Eviction
//! is by capacity (LRU); freshness is checked on lookup.

use lru::LruCache;
use std::num::NonZeroUsize;
use std::time::{Duration, Instant};

/// LRU + TTL cache for content-hash dedup at ingest time.
#[derive(Debug)]
pub struct IdempotencyCache {
    cache: LruCache<String, Instant>,
    ttl: Duration,
}

impl IdempotencyCache {
    /// Build a new cache with the given capacity and TTL.
    ///
    /// Phase 1 defaults are 1000 entries / 5 minutes (per the SDK roadmap, AMS-1.2).
    pub fn new(capacity: usize, ttl: Duration) -> Self {
        let cap = NonZeroUsize::new(capacity.max(1)).expect("capacity > 0");
        Self {
            cache: LruCache::new(cap),
            ttl,
        }
    }

    /// Atomic check-and-insert. Returns `true` if `key` was already seen within TTL
    /// (caller should treat this as a duplicate and skip the produce).
    ///
    /// On expired entry, the entry is refreshed and `false` is returned (treated
    /// as a fresh insert).
    pub fn check_and_insert(&mut self, key: String) -> bool {
        if let Some(&t) = self.cache.peek(&key) {
            if t.elapsed() < self.ttl {
                // Still fresh — touch (mark as recently used) and report duplicate.
                let _ = self.cache.get(&key);
                return true;
            }
        }
        // Either absent or expired — overwrite with current Instant.
        self.cache.put(key, Instant::now());
        false
    }

    /// Number of entries currently in the cache (including expired-but-not-evicted).
    pub fn len(&self) -> usize {
        self.cache.len()
    }

    /// True if no entries.
    pub fn is_empty(&self) -> bool {
        self.cache.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_insert_is_not_duplicate() {
        let mut c = IdempotencyCache::new(10, Duration::from_secs(60));
        assert!(!c.check_and_insert("k1".into()));
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn second_insert_within_ttl_is_duplicate() {
        let mut c = IdempotencyCache::new(10, Duration::from_secs(60));
        assert!(!c.check_and_insert("k1".into()));
        assert!(c.check_and_insert("k1".into()));
    }

    #[test]
    fn lru_evicts_at_capacity() {
        let mut c = IdempotencyCache::new(2, Duration::from_secs(60));
        c.check_and_insert("a".into());
        c.check_and_insert("b".into());
        c.check_and_insert("c".into()); // evicts "a"
        assert!(!c.check_and_insert("a".into())); // a was evicted, so not duplicate
        assert_eq!(c.len(), 2);
    }

    #[test]
    fn expired_entry_is_treated_as_fresh() {
        let mut c = IdempotencyCache::new(10, Duration::from_millis(1));
        c.check_and_insert("k1".into());
        std::thread::sleep(Duration::from_millis(10));
        // After expiry, returns false (fresh insert) and refreshes timestamp.
        assert!(!c.check_and_insert("k1".into()));
    }

    #[test]
    fn capacity_zero_is_clamped_to_one() {
        // Don't panic — clamp.
        let mut c = IdempotencyCache::new(0, Duration::from_secs(60));
        c.check_and_insert("k1".into());
    }
}
