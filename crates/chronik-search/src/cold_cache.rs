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
use chronik_storage::term_filter::{term_filter_key, TermFilter};
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
    filters: Mutex<Filters>,
}

/// Term filters of archives, by archive key: present, or known to be absent
/// (an archive written before filters existed — one is built the first time
/// it is opened).
#[derive(Default)]
struct Filters {
    entries: HashMap<String, (Option<Arc<TermFilter>>, u64)>,
    bytes: usize,
    clock: u64,
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
    /// Its unpacked or downloaded copy in the cache; `None` for a local
    /// plain-tar archive, which is opened where it is.
    dir: Option<PathBuf>,
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
        Self {
            root,
            budget_bytes,
            max_open: max_open.max(1),
            state: Mutex::new(State::default()),
            filters: Mutex::new(Filters::default()),
        }
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
        let local = store.local_path(&key);
        let (opened, dir) = open_archive(store.clone(), &key, &self.root, local).await;
        let bytes = match (&opened, known, &dir) {
            (Ok(_), false, Some(d)) => {
                let d = d.clone();
                tokio::task::spawn_blocking(move || dir_size(&d)).await.unwrap_or(0)
            }
            _ => 0,
        };

        let mut s = self.state.lock().unwrap();
        s.in_flight.remove(&key);
        match opened {
            Err(e) => {
                drop(s);
                if let (false, Some(dir)) = (known, &dir) {
                    let _ = std::fs::remove_dir_all(dir);
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
                        Entry { dir: dir.clone(), bytes, last_used: now, reader: Some(reader.clone()) },
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
                if !known {
                    self.build_missing_filter(store, key, reader.clone());
                }
                Ok(reader)
            }
        }
    }

    /// The term filter of the archive at `object_key`, or `None` when it has
    /// none (yet). Loaded from the store on first use and kept in memory,
    /// within `CHRONIK_SEARCH_TERM_FILTER_BYTES` (default 256MB).
    pub async fn term_filter(&self, store: &dyn ObjectStore, object_key: &str) -> Option<Arc<TermFilter>> {
        {
            let mut f = self.filters.lock().unwrap();
            f.clock += 1;
            let now = f.clock;
            if let Some(entry) = f.entries.get_mut(object_key) {
                entry.1 = now;
                return entry.0.clone();
            }
        }
        let loaded = match store.get(&term_filter_key(object_key)).await {
            Ok(bytes) => TermFilter::from_bytes(&bytes).map(Arc::new),
            Err(_) => None,
        };
        self.remember_filter(object_key, loaded.clone());
        loaded
    }

    fn remember_filter(&self, object_key: &str, filter: Option<Arc<TermFilter>>) {
        static BUDGET: OnceLock<usize> = OnceLock::new();
        let budget = *BUDGET.get_or_init(|| {
            env_bytes("CHRONIK_SEARCH_TERM_FILTER_BYTES").unwrap_or(256 * 1024 * 1024) as usize
        });
        let mut f = self.filters.lock().unwrap();
        f.clock += 1;
        let now = f.clock;
        let size = filter.as_ref().map_or(0, |t| t.memory_bytes());
        if let Some((old, _)) = f.entries.insert(object_key.to_string(), (filter, now)) {
            f.bytes = f.bytes.saturating_sub(old.map_or(0, |t| t.memory_bytes()));
        }
        f.bytes += size;
        // Over budget: drop the least recently used (they reload on demand).
        while f.bytes > budget && f.entries.len() > 1 {
            let Some(victim) = f
                .entries
                .iter()
                .filter(|(k, _)| k.as_str() != object_key)
                .min_by_key(|(_, (_, used))| *used)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            if let Some((old, _)) = f.entries.remove(&victim) {
                f.bytes = f.bytes.saturating_sub(old.map_or(0, |t| t.memory_bytes()));
            }
        }
    }

    /// Give an archive written before term filters existed its filter, from
    /// the reader a search just opened, so later searches can skip it.
    fn build_missing_filter(
        self: &Arc<Self>,
        store: Arc<dyn ObjectStore>,
        object_key: String,
        reader: Arc<TantivySegmentReader>,
    ) {
        let known_present = matches!(
            self.filters.lock().unwrap().entries.get(&object_key),
            Some((Some(_), _))
        );
        if known_present {
            return;
        }
        let this = self.clone();
        tokio::spawn(async move {
            if this.term_filter(store.as_ref(), &object_key).await.is_some() {
                return;
            }
            let built = tokio::task::spawn_blocking(move || TermFilter::build(reader.index())).await;
            match built {
                Ok(Ok(filter)) => {
                    let key = term_filter_key(&object_key);
                    if let Err(e) = store.put(&key, bytes::Bytes::from(filter.to_bytes())).await {
                        debug!(object_key = %key, "could not store a term filter: {}", e);
                    }
                    this.remember_filter(&object_key, Some(Arc::new(filter)));
                }
                Ok(Err(e)) => debug!(object_key = %object_key, "could not build a term filter: {}", e),
                Err(e) => debug!(object_key = %object_key, "term filter task failed: {}", e),
            }
        });
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
            let Some(victim) = self.lru(keep, Pick::OnDisk) else { break };
            let e = self.entries.remove(&victim).expect("lru returns a present key");
            self.bytes = self.bytes.saturating_sub(e.bytes);
            self.evictions += 1;
            let Some(dir) = e.dir else { continue };
            // An open reader keeps working: its files are mapped, and an
            // unlinked file stays readable while mapped.
            let trash = root.join(format!(".evicted-{}", self.evictions));
            match std::fs::rename(&dir, &trash) {
                Ok(()) => doomed.push(trash),
                Err(err) => {
                    debug!(dir = %dir.display(), "evicting by rename failed ({err}); deleting in place");
                    let _ = std::fs::remove_dir_all(&dir);
                }
            }
        }
        while self.entries.values().filter(|e| e.reader.is_some()).count() > max_open {
            let Some(victim) = self.lru(keep, Pick::Open) else { break };
            // An archive opened in place has nothing else cached: forget it.
            if self.entries.get(&victim).is_some_and(|e| e.dir.is_none()) {
                self.entries.remove(&victim);
            } else if let Some(e) = self.entries.get_mut(&victim) {
                e.reader = None;
            }
        }
        doomed
    }

    fn lru(&self, keep: &str, pick: Pick) -> Option<String> {
        self.entries
            .iter()
            .filter(|(k, e)| {
                k.as_str() != keep
                    && !self.in_flight.contains_key(k.as_str())
                    && match pick {
                        Pick::OnDisk => e.dir.is_some(),
                        Pick::Open => e.reader.is_some(),
                    }
            })
            .min_by_key(|(_, e)| e.last_used)
            .map(|(k, _)| k.clone())
    }
}

/// Which entries an eviction may choose from.
#[derive(Clone, Copy)]
enum Pick {
    /// Holding a copy in the cache directory (the byte budget).
    OnDisk,
    /// Holding an open reader (the open-segment cap).
    Open,
}

/// Open the archive at `key`: a local plain tar in place (no cache copy,
/// `None`); otherwise through the cache directory — a downloaded plain tar
/// kept as one file, or a `.tar.gz` (written by an earlier version) unpacked.
async fn open_archive(
    store: Arc<dyn ObjectStore>,
    key: &str,
    root: &Path,
    local_path: Option<PathBuf>,
) -> (chronik_common::Result<TantivySegmentReader>, Option<PathBuf>) {
    if let Some(path) = local_path.filter(|p| p.is_file()) {
        let p = path.clone();
        match tokio::task::spawn_blocking(move || TantivySegmentReader::open_tar_file(&p)).await {
            Ok(Ok(Some(reader))) => return (Ok(reader), None),
            Ok(Ok(None)) => {} // a .tar.gz: unpack it below
            Ok(Err(e)) => return (Err(e), None),
            Err(e) => return (Err(chronik_common::Error::Internal(format!("open task: {e}"))), None),
        }
    }
    let dir = root.join(dir_name(key));
    let tar = dir.join("archive.tar");
    let ready = dir.join(".ready");
    if !ready.exists() {
        match store.get(key).await {
            Ok(bytes) if !chronik_storage::tar_directory::is_gzip(&bytes) => {
                let (d, t, r) = (dir.clone(), tar.clone(), ready.clone());
                let written = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
                    let _ = std::fs::remove_dir_all(&d);
                    std::fs::create_dir_all(&d)?;
                    std::fs::write(&t, &bytes)?;
                    std::fs::write(&r, b"")
                })
                .await;
                if let Err(e) = written.map_err(|e| e.to_string()).and_then(|r| r.map_err(|e| e.to_string())) {
                    return (Err(chronik_common::Error::Internal(format!("caching {key}: {e}"))), Some(dir));
                }
            }
            Ok(_) => {} // a .tar.gz: from_object_store_cached unpacks it
            Err(e) => return (Err(chronik_common::Error::Internal(format!("download {key}: {e}"))), Some(dir)),
        }
    }
    if tar.is_file() {
        let t = tar.clone();
        let opened = match tokio::task::spawn_blocking(move || TantivySegmentReader::open_tar_file(&t)).await {
            Ok(Ok(Some(reader))) => Ok(reader),
            Ok(Ok(None)) => Err(chronik_common::Error::Internal(format!("{key}: cached copy is not a tar"))),
            Ok(Err(e)) => Err(e),
            Err(e) => Err(chronik_common::Error::Internal(format!("open task: {e}"))),
        };
        return (opened, Some(dir));
    }
    (TantivySegmentReader::from_object_store_cached(store, key, &dir).await, Some(dir))
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
    /// `gzip`: archives as earlier versions wrote them (`.tar.gz`, unpacked to be
    /// read); otherwise plain tars, as written now (opened in place).
    async fn archives(n: i64, gzip: bool) -> (tempfile::TempDir, Arc<dyn ObjectStore>, Vec<String>) {
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
            let (tar, _) = w.commit_and_serialize(&out).unwrap();
            let mut data = std::fs::read(tar).unwrap();
            if gzip {
                use std::io::Write;
                let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
                gz.write_all(&data).unwrap();
                data = gz.finish().unwrap();
            }
            let ext = if gzip { "tar.gz" } else { "tar" };
            let key = format!("tantivy_indexes/cachetest/partition-0/segment-{}-{}.{ext}", base, base + 49);
            store.put(&key, bytes::Bytes::from(data)).await.unwrap();
            keys.push(key);
        }
        (dir, store, keys)
    }

    #[tokio::test]
    async fn stays_within_its_byte_budget_and_open_limit() {
        let (_d, store, keys) = archives(8, true).await;
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
        let (_d, store, keys) = archives(1, true).await;
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

    /// An archive written before term filters existed gets one the first time
    /// a search opens it, stored beside it for every later search.
    #[tokio::test]
    async fn an_old_archive_gets_its_term_filter_on_first_open() {
        let (_d, store, keys) = archives(1, false).await;
        let root = tempfile::tempdir().unwrap();
        let cache = Arc::new(ColdSegmentCache::new(root.path().join("c"), u64::MAX, 10));
        assert!(store.get(&term_filter_key(&keys[0])).await.is_err(), "starts without one");
        assert!(cache.term_filter(store.as_ref(), &keys[0]).await.is_none());

        cache.reader(store.clone(), &keys[0]).await.unwrap();
        let mut stored = None;
        for _ in 0..100 {
            if let Ok(bytes) = store.get(&term_filter_key(&keys[0])).await {
                stored = TermFilter::from_bytes(&bytes);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let stored = stored.expect("a filter stored beside the archive");
        assert_eq!(cache.term_filter(store.as_ref(), &keys[0]).await.as_deref(), Some(&stored));

        // A fresh node (empty memory) loads it from the store.
        let other = Arc::new(ColdSegmentCache::new(root.path().join("d"), u64::MAX, 10));
        assert_eq!(other.term_filter(store.as_ref(), &keys[0]).await.as_deref(), Some(&stored));
    }

    /// A plain-tar archive on local disk is opened where it is: nothing is
    /// copied into the cache, and closing its reader forgets it entirely.
    #[tokio::test]
    async fn a_local_plain_tar_opens_in_place() {
        let (_d, store, keys) = archives(3, false).await;
        let root = tempfile::tempdir().unwrap();
        let cache = Arc::new(ColdSegmentCache::new(root.path().join("c"), 1, 2));
        for key in &keys {
            let reader = cache.reader(store.clone(), key).await.unwrap();
            assert_eq!(reader.searcher().num_docs(), 50);
        }
        let (entries, bytes, open, evictions) = cache.stats();
        assert_eq!(bytes, 0, "no copy in the cache");
        assert_eq!(evictions, 0, "nothing on disk to evict, even over a 1-byte budget");
        assert!(open <= 2 && entries <= 2, "open cap still applies ({entries} entries, {open} open)");
        assert_eq!(std::fs::read_dir(root.path().join("c")).unwrap().count(), 0, "cache directory untouched");
    }

    /// A plain tar from a store with no local file (S3, GCS, Azure) is
    /// downloaded once into the cache, as one file, and opened from there.
    #[tokio::test]
    async fn a_remote_plain_tar_is_cached_as_one_file() {
        let (_d, store, keys) = archives(1, false).await;
        let root = tempfile::tempdir().unwrap();
        let (opened, dir) = open_archive(store.clone(), &keys[0], root.path(), None).await;
        assert_eq!(opened.unwrap().searcher().num_docs(), 50);
        let dir = dir.expect("a cache copy");
        assert!(dir.join("archive.tar").is_file());
        assert!(!dir.join("index").exists(), "not unpacked");
        // Reopened from the cached copy without downloading again.
        let (again, _) = open_archive(store, &keys[0], root.path(), None).await;
        assert_eq!(again.unwrap().searcher().num_docs(), 50);
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
