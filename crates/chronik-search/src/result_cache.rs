//! Per-archive results of counts and aggregations, kept between searches.
//!
//! An archive never changes once written: a merge, reindex, retention or
//! truncation replaces or removes it, never rewrites it in place. So a
//! request's count and partial aggregations over one archive (a
//! [`Partial`]) can be computed once and reused by every later request with
//! the same query and aggregations. Dashboards refresh the same queries every
//! few seconds; with this only the hot index (records no archive holds yet)
//! is recomputed for them.
//!
//! An archive is identified by its object key *and* its creation time and
//! size: after a follower truncation the indexer can write a new archive
//! under the name of one it removed, holding different records.
//!
//! Bounded by `CHRONIK_SEARCH_RESULT_CACHE_BYTES` (default 64MB; 0 turns it
//! off), least recently used dropped first.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, OnceLock};

use chronik_storage::segment_index::SegmentMetadata;

use crate::topic_aggs::Partial;

/// Default budget (`CHRONIK_SEARCH_RESULT_CACHE_BYTES`).
const DEFAULT_BUDGET_BYTES: u64 = 64 * 1024 * 1024;
/// Bookkeeping charged per entry on top of the partial itself.
const ENTRY_OVERHEAD: u64 = 256;

#[derive(Clone, PartialEq, Eq, Hash)]
struct Key {
    archive: String,
    created_at: i64,
    size_bytes: u64,
    request: Arc<str>,
}

struct Entry {
    partial: Arc<Partial>,
    bytes: u64,
    tick: u64,
}

#[derive(Default)]
struct State {
    entries: HashMap<Key, Entry>,
    /// Entries by last use, oldest first.
    order: BTreeMap<u64, Key>,
    bytes: u64,
    clock: u64,
    hits: u64,
    misses: u64,
}

pub struct ResultCache {
    budget: u64,
    state: Mutex<State>,
}

impl ResultCache {
    /// The process-wide cache, sized from the environment.
    pub fn shared() -> &'static ResultCache {
        static CACHE: OnceLock<ResultCache> = OnceLock::new();
        CACHE.get_or_init(|| {
            ResultCache::new(chronik_common::env::bytes("CHRONIK_SEARCH_RESULT_CACHE_BYTES").unwrap_or(DEFAULT_BUDGET_BYTES))
        })
    }

    pub fn new(budget: u64) -> Self {
        Self { budget, state: Mutex::new(State::default()) }
    }

    /// What identifies a request's results on one archive: its query and
    /// aggregations (the archive's schema is part of the archive).
    pub fn request_key(query: Option<&crate::api::QueryDsl>, aggs: Option<&serde_json::Value>) -> Option<Arc<str>> {
        serde_json::to_string(&(query, aggs)).ok().map(Arc::from)
    }

    fn key(archive: &SegmentMetadata, request: &Arc<str>) -> Key {
        Key {
            archive: archive.object_store_path.clone(),
            created_at: archive.created_at,
            size_bytes: archive.size_bytes,
            request: request.clone(),
        }
    }

    pub fn get(&self, archive: &SegmentMetadata, request: &Arc<str>) -> Option<Arc<Partial>> {
        if self.budget == 0 {
            return None;
        }
        let key = Self::key(archive, request);
        let mut s = self.state.lock().unwrap();
        s.clock += 1;
        let now = s.clock;
        let Some(entry) = s.entries.get_mut(&key) else {
            s.misses += 1;
            return None;
        };
        let (old, partial) = (entry.tick, entry.partial.clone());
        entry.tick = now;
        s.order.remove(&old);
        s.order.insert(now, key);
        s.hits += 1;
        Some(partial)
    }

    pub fn put(&self, archive: &SegmentMetadata, request: &Arc<str>, partial: Arc<Partial>) {
        let bytes = ENTRY_OVERHEAD
            + request.len() as u64
            + partial.aggs.as_ref().and_then(|a| bincode::serialized_size(a).ok()).unwrap_or(0);
        if self.budget == 0 || bytes > self.budget / 4 {
            return; // one result may not take over the cache
        }
        let key = Self::key(archive, request);
        let mut s = self.state.lock().unwrap();
        s.clock += 1;
        let now = s.clock;
        if let Some(old) = s.entries.remove(&key) {
            s.order.remove(&old.tick);
            s.bytes -= old.bytes;
        }
        while s.bytes + bytes > self.budget {
            let Some((_, oldest)) = s.order.pop_first() else { break };
            if let Some(gone) = s.entries.remove(&oldest) {
                s.bytes -= gone.bytes;
            }
        }
        s.order.insert(now, key.clone());
        s.entries.insert(key, Entry { partial, bytes, tick: now });
        s.bytes += bytes;
    }

    /// (entries, bytes, hits, misses)
    pub fn stats(&self) -> (usize, u64, u64, u64) {
        let s = self.state.lock().unwrap();
        (s.entries.len(), s.bytes, s.hits, s.misses)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn archive(path: &str, created_at: i64) -> SegmentMetadata {
        SegmentMetadata {
            segment_id: path.into(),
            topic: "t".into(),
            partition: 0,
            min_offset: 0,
            max_offset: 9,
            record_count: 10,
            min_timestamp: 0,
            max_timestamp: 0,
            object_store_path: path.into(),
            size_bytes: 100,
            created_at,
            compression: "tar".into(),
        }
    }

    fn partial(count: u64) -> Arc<Partial> {
        Arc::new(Partial { count, aggs: None })
    }

    #[test]
    fn a_result_is_reused_for_the_same_archive_and_request_only() {
        let cache = ResultCache::new(1 << 20);
        let (a, req) = (archive("a.tar", 1), Arc::<str>::from("q1"));
        assert!(cache.get(&a, &req).is_none());
        cache.put(&a, &req, partial(7));
        assert_eq!(cache.get(&a, &req).unwrap().count, 7);
        assert!(cache.get(&a, &Arc::from("q2")).is_none(), "another request");
        assert!(cache.get(&archive("b.tar", 1), &req).is_none(), "another archive");
    }

    #[test]
    fn an_archive_written_again_under_the_same_name_is_not_served_the_old_result() {
        let cache = ResultCache::new(1 << 20);
        let req = Arc::<str>::from("q");
        cache.put(&archive("a.tar", 1), &req, partial(7));
        assert!(cache.get(&archive("a.tar", 2), &req).is_none());
    }

    #[test]
    fn the_least_recently_used_go_first_and_the_budget_holds() {
        let per = ENTRY_OVERHEAD + 1;
        let cache = ResultCache::new(per * 4);
        let req = Arc::<str>::from("q");
        for i in 0..4 {
            cache.put(&archive(&format!("{i}"), 0), &req, partial(i));
        }
        cache.get(&archive("0", 0), &req); // 0 is now the most recent
        cache.put(&archive("4", 0), &req, partial(4));
        assert!(cache.get(&archive("1", 0), &req).is_none(), "oldest dropped");
        assert!(cache.get(&archive("0", 0), &req).is_some());
        let (entries, bytes, _, _) = cache.stats();
        assert_eq!(entries, 4);
        assert!(bytes <= per * 4);
    }

    #[test]
    fn zero_budget_turns_it_off() {
        let cache = ResultCache::new(0);
        let req = Arc::<str>::from("q");
        cache.put(&archive("a", 0), &req, partial(1));
        assert!(cache.get(&archive("a", 0), &req).is_none());
    }
}
