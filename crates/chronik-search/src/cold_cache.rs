//! Unpacked archived index segments, kept on disk for cold search.
//!
//! The WalIndexer archives each sealed segment's Tantivy index as a `.tar.gz`
//! in the object store. Tantivy cannot read a compressed archive, so a search
//! unpacks it into a directory first. Unpacking costs far more than searching,
//! so the directories are kept and reused across queries.
//!
//! Before v2.17.2 they were kept forever, in a directory under `/tmp` shared by
//! every broker on the host: a node with 6,000 archives (~37 MB unpacked
//! each) could fill hundreds of GB with a single search across all topics,
//! and brokers sharing a host read each other's copies (the directory name
//! came from the object key, which is the same on every node). This cache
//! lives under the node's own data directory, is cleared at boot, holds at
//! most a byte budget of unpacked segments, and keeps at most a fixed number
//! of them open; the least recently used go first.

use chronik_common::{Error, Result};
use chronik_storage::object_store::ObjectStore;
use chronik_storage::tantivy_segment::TantivySegmentReader;
use futures::future::{BoxFuture, FutureExt, Shared};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use tracing::{debug, info, warn};

/// Unpacked segments kept on disk, by default (`CHRONIK_SEARCH_CACHE_BYTES`).
const DEFAULT_BUDGET_BYTES: u64 = 4 * 1024 * 1024 * 1024;
/// Segments kept open at once, by default (`CHRONIK_SEARCH_CACHE_OPEN_SEGMENTS`).
const DEFAULT_MAX_OPEN: usize = 256;

type Load = Shared<BoxFuture<'static, std::result::Result<Arc<TantivySegmentReader>, String>>>;

pub struct ColdSegmentCache {
    root: PathBuf,
    budget_bytes: u64,
    max_open: usize,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    entries: HashMap<String, Entry>,
    /// Loads under way, so concurrent searches of one segment unpack it once.
    in_flight: HashMap<String, Load>,
    bytes: u64,
    clock: u64,
    evictions: u64,
}

struct Entry {
    dir: PathBuf,
    bytes: u64,
    last_used: u64,
    reader: Option<Arc<TantivySegmentReader>>,
}

impl ColdSegmentCache {
    /// The cache for `root`, shared by every search API in the process that
    /// asks for the same directory. The first call clears the directory.
    pub fn shared(root: PathBuf) -> Arc<Self> {
        static CACHES: OnceLock<Mutex<HashMap<PathBuf, Arc<ColdSegmentCache>>>> = OnceLock::new();
        let mut caches = CACHES.get_or_init(Default::default).lock().unwrap();
        caches
            .entry(root.clone())
            .or_insert_with(|| {
                Arc::new(Self::new(
                    root,
                    env_bytes("CHRONIK_SEARCH_CACHE_BYTES").unwrap_or(DEFAULT_BUDGET_BYTES),
                    env_usize("CHRONIK_SEARCH_CACHE_OPEN_SEGMENTS").unwrap_or(DEFAULT_MAX_OPEN),
                ))
            })
            .clone()
    }

    /// The cache directory for a node whose WAL indexes live in
    /// `index_base_path` (`{data_dir}/tantivy_indexes`), unless
    /// `CHRONIK_SEARCH_CACHE_DIR` names one.
    pub fn dir_for(index_base_path: &str) -> PathBuf {
        if let Ok(dir) = std::env::var("CHRONIK_SEARCH_CACHE_DIR") {
            if !dir.trim().is_empty() {
                return PathBuf::from(dir);
            }
        }
        let base = Path::new(index_base_path);
        base.parent().unwrap_or(base).join("search-cache")
    }

    pub fn new(root: PathBuf, budget_bytes: u64, max_open: usize) -> Self {
        // Nothing in it is durable: start empty, so the budget accounts for
        // every byte in the directory.
        if root.exists() {
            if let Err(e) = std::fs::remove_dir_all(&root) {
                warn!(dir = %root.display(), "could not clear the search cache: {}", e);
            }
        }
        if let Err(e) = std::fs::create_dir_all(&root) {
            warn!(dir = %root.display(), "could not create the search cache: {}", e);
        }
        info!(
            dir = %root.display(),
            budget_mb = budget_bytes / (1024 * 1024),
            max_open,
            "search cache for archived segments"
        );
        Self { root, budget_bytes, max_open: max_open.max(1), state: Mutex::new(State::default()) }
    }

    /// An open reader for the archived segment at `object_key`, unpacking it
    /// first if it is not cached.
    pub async fn reader(
        self: &Arc<Self>,
        store: Arc<dyn ObjectStore>,
        object_key: &str,
    ) -> Result<Arc<TantivySegmentReader>> {
        let load = {
            let mut s = self.state.lock().unwrap();
            if let Some(reader) = s.touch(object_key) {
                return Ok(reader);
            }
            match s.in_flight.get(object_key) {
                Some(load) => load.clone(),
                None => {
                    // Spawned, so a search that stops waiting (its time limit)
                    // does not cancel the load halfway: the unpacked directory
                    // is always either recorded against the budget or removed.
                    let this = self.clone();
                    let key = object_key.to_string();
                    let handle = tokio::spawn(async move { this.load(store, key).await });
                    let load: Load = async move {
                        handle.await.map_err(|e| format!("segment load task failed: {e}"))?
                    }
                    .boxed()
                    .shared();
                    s.in_flight.insert(object_key.to_string(), load.clone());
                    load
                }
            }
        };
        load.await.map_err(Error::Internal)
    }

    async fn load(
        self: Arc<Self>,
        store: Arc<dyn ObjectStore>,
        key: String,
    ) -> std::result::Result<Arc<TantivySegmentReader>, String> {
        let known = self.state.lock().unwrap().entries.contains_key(&key);
        let dir = self.root.join(dir_name(&key));
        let opened = TantivySegmentReader::from_object_store_cached(store, &key, &dir).await;
        let bytes = match (&opened, known) {
            (Ok(_), false) => {
                let d = dir.clone();
                tokio::task::spawn_blocking(move || dir_size(&d)).await.unwrap_or(0)
            }
            _ => 0,
        };

        let mut s = self.state.lock().unwrap();
        s.in_flight.remove(&key);
        match opened {
            Err(e) => {
                drop(s);
                if !known {
                    let _ = std::fs::remove_dir_all(&dir);
                }
                Err(e.to_string())
            }
            Ok(reader) => {
                let reader = Arc::new(reader);
                s.clock += 1;
                let now = s.clock;
                if known {
                    if let Some(e) = s.entries.get_mut(&key) {
                        e.reader = Some(reader.clone());
                        e.last_used = now;
                    }
                } else {
                    s.bytes += bytes;
                    s.entries.insert(
                        key.clone(),
                        Entry { dir, bytes, last_used: now, reader: Some(reader.clone()) },
                    );
                }
                let evicted = s.evict(self.budget_bytes, self.max_open, &key, &self.root);
                drop(s);
                if !evicted.is_empty() {
                    tokio::task::spawn_blocking(move || {
                        for dir in evicted {
                            let _ = std::fs::remove_dir_all(&dir);
                        }
                    });
                }
                Ok(reader)
            }
        }
    }

    /// (unpacked segments, bytes on disk, segments open, evictions so far)
    pub fn stats(&self) -> (usize, u64, usize, u64) {
        let s = self.state.lock().unwrap();
        let open = s.entries.values().filter(|e| e.reader.is_some()).count();
        (s.entries.len(), s.bytes, open, s.evictions)
    }
}

impl State {
    fn touch(&mut self, key: &str) -> Option<Arc<TantivySegmentReader>> {
        self.clock += 1;
        let now = self.clock;
        let e = self.entries.get_mut(key)?;
        let reader = e.reader.clone()?;
        e.last_used = now;
        Some(reader)
    }

    /// Bring the cache within its limits, never touching `keep` (just loaded)
    /// or a segment being loaded. Returns directories to delete; each is first
    /// renamed out of the way, so a later load of the same segment cannot have
    /// its fresh copy deleted from under it.
    fn evict(&mut self, budget: u64, max_open: usize, keep: &str, root: &Path) -> Vec<PathBuf> {
        let mut doomed = Vec::new();
        while self.bytes > budget {
            let Some(victim) = self.lru(keep, false) else { break };
            let e = self.entries.remove(&victim).expect("lru returns a present key");
            self.bytes = self.bytes.saturating_sub(e.bytes);
            self.evictions += 1;
            // An open reader keeps working: its files are mapped, and an
            // unlinked file stays readable while mapped.
            let trash = root.join(format!(".evicted-{}", self.evictions));
            match std::fs::rename(&e.dir, &trash) {
                Ok(()) => doomed.push(trash),
                Err(err) => {
                    debug!(dir = %e.dir.display(), "evicting by rename failed ({err}); deleting in place");
                    let _ = std::fs::remove_dir_all(&e.dir);
                }
            }
        }
        while self.entries.values().filter(|e| e.reader.is_some()).count() > max_open {
            let Some(victim) = self.lru(keep, true) else { break };
            if let Some(e) = self.entries.get_mut(&victim) {
                e.reader = None;
            }
        }
        doomed
    }

    fn lru(&self, keep: &str, open_only: bool) -> Option<String> {
        self.entries
            .iter()
            .filter(|(k, e)| {
                k.as_str() != keep
                    && !self.in_flight.contains_key(k.as_str())
                    && (!open_only || e.reader.is_some())
            })
            .min_by_key(|(_, e)| e.last_used)
            .map(|(k, _)| k.clone())
    }
}

/// A fixed-length directory name for an object key. Keys embed topic names
/// (up to 249 characters), which can exceed a file name's 255-byte limit.
fn dir_name(key: &str) -> String {
    let mut a = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut a);
    let mut b = std::collections::hash_map::DefaultHasher::new();
    (key, "chronik-search-cache").hash(&mut b);
    format!("{:016x}{:016x}", a.finish(), b.finish())
}

fn dir_size(dir: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else { continue };
        for entry in entries.flatten() {
            match entry.metadata() {
                Ok(m) if m.is_dir() => stack.push(entry.path()),
                Ok(m) => total += m.len(),
                Err(_) => {}
            }
        }
    }
    total
}

fn env_usize(name: &str) -> Option<usize> {
    std::env::var(name).ok().and_then(|v| v.trim().parse().ok()).filter(|n| *n > 0)
}

/// `CHRONIK_SEARCH_CACHE_BYTES`: raw bytes, or a number with KB, MB or GB.
fn env_bytes(name: &str) -> Option<u64> {
    let raw = std::env::var(name).ok()?;
    let parsed = parse_bytes(&raw);
    if parsed.is_none() {
        warn!("{name}={raw:?} is not a size (e.g. 4GB, 512MB, 1073741824); using the default");
    }
    parsed
}

fn parse_bytes(raw: &str) -> Option<u64> {
    let v = raw.trim().to_ascii_uppercase();
    let (num, mult) = if let Some(n) = v.strip_suffix("GB") {
        (n, 1024 * 1024 * 1024)
    } else if let Some(n) = v.strip_suffix("MB") {
        (n, 1024 * 1024)
    } else if let Some(n) = v.strip_suffix("KB") {
        (n, 1024)
    } else {
        (v.as_str(), 1)
    };
    num.trim().parse::<u64>().ok().map(|n| n * mult)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chronik_storage::canonical_record::{CanonicalRecord, CanonicalRecordEntry, CompressionType, TimestampType};
    use chronik_storage::object_store::{ObjectStoreConfig, ObjectStoreFactory, StorageBackend};
    use chronik_storage::tantivy_segment::TantivySegmentWriter;

    #[test]
    fn sizes_parse() {
        assert_eq!(parse_bytes("4GB"), Some(4 << 30));
        assert_eq!(parse_bytes(" 512mb "), Some(512 << 20));
        assert_eq!(parse_bytes("1024"), Some(1024));
        assert_eq!(parse_bytes("lots"), None);
    }

    #[test]
    fn directory_names_are_short_and_distinct() {
        let long = format!("tantivy_indexes/{}/partition-0/segment-0-9.tar.gz", "t".repeat(249));
        assert_eq!(dir_name(&long).len(), 32);
        assert_ne!(dir_name("a/partition-0/x"), dir_name("a/partition-1/x"));
    }

    fn record(offset: i64) -> CanonicalRecordEntry {
        CanonicalRecordEntry {
            offset,
            timestamp: 1_700_000_000_000 + offset,
            key: None,
            value: Some(format!("{{\"n\":{offset},\"pad\":\"{}\"}}", "x".repeat(2000)).into_bytes()),
            headers: vec![],
            attributes: 0,
        }
    }

    /// Archive `n` segments of 50 records into a local object store.
    async fn archives(n: i64) -> (tempfile::TempDir, Arc<dyn ObjectStore>, Vec<String>) {
        let dir = tempfile::tempdir().unwrap();
        let config = ObjectStoreConfig {
            backend: StorageBackend::Local { path: dir.path().join("store").to_string_lossy().to_string() },
            bucket: "test".to_string(),
            prefix: None,
            connection: Default::default(),
            performance: Default::default(),
            retry: Default::default(),
            auth: Default::default(),
            default_metadata: None,
            encryption: None,
        };
        let store: Arc<dyn ObjectStore> = Arc::from(ObjectStoreFactory::create(config).await.unwrap());
        let mut keys = Vec::new();
        for s in 0..n {
            let base = s * 50;
            let batch = CanonicalRecord {
                base_offset: base,
                partition_leader_epoch: 0,
                producer_id: -1,
                producer_epoch: -1,
                base_sequence: -1,
                is_transactional: false,
                is_control: false,
                compression: CompressionType::None,
                timestamp_type: TimestampType::CreateTime,
                base_timestamp: 1_700_000_000_000 + base,
                max_timestamp: 1_700_000_000_000 + base + 49,
                records: (base..base + 50).map(record).collect(),
                compressed_records_wire_bytes: None,
                original_v1_wire_format: None,
                original_v2_wire_format: None,
            };
            let mut w = TantivySegmentWriter::new("cachetest".into(), 0, base).unwrap();
            w.write_batch(&batch).unwrap();
            let out = dir.path().join(format!("out-{s}"));
            std::fs::create_dir_all(&out).unwrap();
            let (tar_gz, _) = w.commit_and_serialize(&out).unwrap();
            let key = format!("tantivy_indexes/cachetest/partition-0/segment-{}-{}.tar.gz", base, base + 49);
            store.put(&key, bytes::Bytes::from(std::fs::read(tar_gz).unwrap())).await.unwrap();
            keys.push(key);
        }
        (dir, store, keys)
    }

    #[tokio::test]
    async fn stays_within_its_byte_budget_and_open_limit() {
        let (_d, store, keys) = archives(8).await;
        let root = tempfile::tempdir().unwrap();
        // Measure one unpacked segment, then allow room for three.
        let probe = Arc::new(ColdSegmentCache::new(root.path().join("probe"), u64::MAX, 100));
        probe.reader(store.clone(), &keys[0]).await.unwrap();
        let one = probe.stats().1;
        assert!(one > 0);

        let cache = Arc::new(ColdSegmentCache::new(root.path().join("c"), one * 3, 2));
        for key in &keys {
            let reader = cache.reader(store.clone(), key).await.unwrap();
            assert_eq!(reader.searcher().num_docs(), 50);
            let (count, bytes, open, _) = cache.stats();
            assert!(bytes <= one * 3 + one / 2, "{bytes} bytes over a budget of {}", one * 3);
            assert!(count <= 3, "{count} segments kept");
            assert!(open <= 2, "{open} segments open");
        }
        let (count, _, _, evictions) = cache.stats();
        assert_eq!(evictions, 8 - count as u64);
        // On disk too, once the background deletes finish.
        for _ in 0..50 {
            if dir_size(&root.path().join("c")) <= one * 3 + one / 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(dir_size(&root.path().join("c")) <= one * 3 + one / 2);

        // An evicted segment comes back on demand.
        let reader = cache.reader(store.clone(), &keys[0]).await.unwrap();
        assert_eq!(reader.searcher().num_docs(), 50);
    }

    #[tokio::test]
    async fn concurrent_searches_of_one_segment_unpack_it_once() {
        let (_d, store, keys) = archives(1).await;
        let root = tempfile::tempdir().unwrap();
        let cache = Arc::new(ColdSegmentCache::new(root.path().join("c"), u64::MAX, 10));
        let loads: Vec<_> = (0..16)
            .map(|_| {
                let (c, s, k) = (cache.clone(), store.clone(), keys[0].clone());
                tokio::spawn(async move { c.reader(s, &k).await.map(|r| r.searcher().num_docs()) })
            })
            .collect();
        for l in loads {
            assert_eq!(l.await.unwrap().unwrap(), 50);
        }
        assert_eq!(cache.stats().0, 1);
        let dirs = std::fs::read_dir(root.path().join("c")).unwrap().count();
        assert_eq!(dirs, 1, "one unpacked copy");
    }

    #[test]
    fn boot_clears_what_a_previous_run_left() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("c");
        std::fs::create_dir_all(dir.join("stale")).unwrap();
        std::fs::write(dir.join("stale/blob"), vec![0u8; 4096]).unwrap();
        let cache = ColdSegmentCache::new(dir.clone(), 1024, 1);
        assert_eq!(cache.stats().1, 0);
        assert_eq!(dir_size(&dir), 0);
    }
}
