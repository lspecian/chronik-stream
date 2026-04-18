//! In-memory Tantivy index shadowing the WAL tail for near-real-time search.
//!
//! See `docs/ROADMAP_HOT_PATH.md` (HP-1.1) for the full design.
//!
//! The hot path is RAM-only. "writer" and "commit" below refer to Tantivy's
//! in-memory API (RAMDirectory, buffer visibility flip) — there is no disk I/O.
//!
//! Query path merges hot hits with cold Tantivy hits; eviction is driven by
//! WalIndexer after it successfully persists offsets to the cold index.
//!
//! # Schema
//!
//! Mirrors the cold TantivyIndexer schema (see `indexer.rs`) so hot and cold
//! hits are directly mergeable:
//! - `topic`      STRING | STORED
//! - `partition`  i64 indexed | stored
//! - `offset`     i64 indexed | stored
//! - `timestamp`  i64 indexed | stored
//! - `key`        TEXT  | STORED
//! - `value`      TEXT  | STORED
//! - `headers`    TEXT  | STORED

use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use dashmap::DashMap;
use tantivy::{
    collector::TopDocs,
    query::QueryParser,
    schema::{Field, NumericOptions, Schema, STORED, STRING, TEXT},
    Index, IndexReader, IndexWriter, ReloadPolicy, Term,
};
use tokio::sync::RwLock;
use tracing::{debug, trace, warn};

/// Minimum heap a Tantivy writer accepts.
const TANTIVY_MIN_HEAP: usize = 15_000_000;

/// A record to be inserted into the hot index.
#[derive(Debug, Clone)]
pub struct HotDoc {
    pub offset: i64,
    pub timestamp: i64,
    pub key: Option<String>,
    pub value: String,
    pub headers: Option<String>,
}

/// A hit produced by searching the hot index.
#[derive(Debug, Clone)]
pub struct HotHit {
    pub topic: String,
    pub partition: i32,
    pub offset: i64,
    pub timestamp: i64,
    pub score: f32,
    pub key: Option<String>,
    pub value: String,
}

/// Configuration for the hot text index.
#[derive(Debug, Clone)]
pub struct HotTextConfig {
    /// Writer heap budget in bytes (Tantivy requires >= 15MB).
    pub writer_heap_bytes: usize,
    /// Max docs per partition before eviction rebuilds the index.
    pub max_docs_per_partition: usize,
}

impl Default for HotTextConfig {
    fn default() -> Self {
        Self {
            writer_heap_bytes: TANTIVY_MIN_HEAP,
            max_docs_per_partition: 100_000,
        }
    }
}

/// Per-partition in-memory Tantivy index.
struct HotPartitionIndex {
    topic: String,
    partition: i32,
    index: Index,
    writer: IndexWriter,
    reader: IndexReader,
    /// HP-3 Item 2: most recent produce timestamp (ms since epoch) added to
    /// this partition. Sampled at commit time to report visibility lag.
    last_add_produce_ts_ms: Option<i64>,
    topic_field: Field,
    partition_field: Field,
    offset_field: Field,
    timestamp_field: Field,
    key_field: Field,
    value_field: Field,
    headers_field: Field,
    doc_count: usize,
    min_offset: Option<i64>,
    max_offset: Option<i64>,
    config: HotTextConfig,
}

impl HotPartitionIndex {
    fn new(topic: String, partition: i32, config: HotTextConfig) -> Result<Self> {
        let mut schema_builder = Schema::builder();
        let topic_field = schema_builder.add_text_field("topic", STRING | STORED);
        let partition_field = schema_builder.add_i64_field(
            "partition",
            NumericOptions::default().set_indexed().set_stored(),
        );
        let offset_field = schema_builder.add_i64_field(
            "offset",
            NumericOptions::default().set_indexed().set_stored(),
        );
        let timestamp_field = schema_builder.add_i64_field(
            "timestamp",
            NumericOptions::default().set_indexed().set_stored(),
        );
        let key_field = schema_builder.add_text_field("key", TEXT | STORED);
        let value_field = schema_builder.add_text_field("value", TEXT | STORED);
        let headers_field = schema_builder.add_text_field("headers", TEXT | STORED);
        let schema = schema_builder.build();

        let index = Index::create_in_ram(schema);
        chronik_storage::register_analyzer(&index);

        let heap = config.writer_heap_bytes.max(TANTIVY_MIN_HEAP);
        let writer = index
            .writer(heap)
            .map_err(|e| anyhow!("create hot writer for {}-{}: {}", topic, partition, e))?;
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()
            .map_err(|e| anyhow!("create hot reader for {}-{}: {}", topic, partition, e))?;

        Ok(Self {
            topic,
            partition,
            index,
            writer,
            reader,
            last_add_produce_ts_ms: None,
            topic_field,
            partition_field,
            offset_field,
            timestamp_field,
            key_field,
            value_field,
            headers_field,
            doc_count: 0,
            min_offset: None,
            max_offset: None,
            config,
        })
    }

    fn add_batch(&mut self, docs: &[HotDoc]) -> Result<()> {
        for d in docs {
            let mut doc = tantivy::TantivyDocument::default();
            doc.add_text(self.topic_field, &self.topic);
            doc.add_i64(self.partition_field, self.partition as i64);
            doc.add_i64(self.offset_field, d.offset);
            doc.add_i64(self.timestamp_field, d.timestamp);
            if let Some(k) = &d.key {
                doc.add_text(self.key_field, k);
            }
            doc.add_text(self.value_field, &d.value);
            if let Some(h) = &d.headers {
                doc.add_text(self.headers_field, h);
            }
            self.writer
                .add_document(doc)
                .map_err(|e| anyhow!("hot add_document: {}", e))?;

            self.doc_count += 1;
            self.min_offset = Some(self.min_offset.map_or(d.offset, |o| o.min(d.offset)));
            self.max_offset = Some(self.max_offset.map_or(d.offset, |o| o.max(d.offset)));
            // HP-3 Item 2: track the newest produce timestamp added since
            // the last commit; consumed there to compute visibility lag.
            self.last_add_produce_ts_ms = Some(
                self.last_add_produce_ts_ms.map_or(d.timestamp, |t| t.max(d.timestamp)),
            );
        }
        Ok(())
    }

    /// Flip in-memory buffer visibility. Tantivy name is `commit` — no disk writes.
    fn make_visible(&mut self) -> Result<()> {
        self.writer
            .commit()
            .map_err(|e| anyhow!("hot writer commit: {}", e))?;
        self.reader
            .reload()
            .map_err(|e| anyhow!("hot reader reload: {}", e))?;
        Ok(())
    }

    fn search(&self, query_str: &str, top_k: usize) -> Result<Vec<HotHit>> {
        if query_str.trim().is_empty() {
            return Ok(Vec::new());
        }
        let searcher = self.reader.searcher();
        let parser = QueryParser::for_index(
            &self.index,
            vec![self.value_field, self.key_field, self.headers_field],
        );
        let query = parser
            .parse_query(query_str)
            .map_err(|e| anyhow!("parse hot query `{}`: {}", query_str, e))?;
        let top_docs = searcher
            .search(&query, &TopDocs::with_limit(top_k))
            .map_err(|e| anyhow!("hot search: {}", e))?;

        let mut hits = Vec::with_capacity(top_docs.len());
        for (score, doc_addr) in top_docs {
            let doc: tantivy::TantivyDocument = searcher
                .doc(doc_addr)
                .map_err(|e| anyhow!("hot doc fetch: {}", e))?;
            hits.push(self.doc_to_hit(&doc, score)?);
        }
        Ok(hits)
    }

    fn doc_to_hit(&self, doc: &tantivy::TantivyDocument, score: f32) -> Result<HotHit> {
        use tantivy::schema::Value;
        let get_i64 = |f: Field| -> Option<i64> {
            doc.get_first(f).and_then(|v| v.as_i64())
        };
        let get_text = |f: Field| -> Option<String> {
            doc.get_first(f).and_then(|v| v.as_str().map(|s| s.to_string()))
        };

        Ok(HotHit {
            topic: get_text(self.topic_field).unwrap_or_else(|| self.topic.clone()),
            partition: get_i64(self.partition_field)
                .map(|v| v as i32)
                .unwrap_or(self.partition),
            offset: get_i64(self.offset_field)
                .context("hot doc missing offset")?,
            timestamp: get_i64(self.timestamp_field).unwrap_or(0),
            score,
            key: get_text(self.key_field),
            value: get_text(self.value_field).unwrap_or_default(),
        })
    }

    /// Drop all docs with offset <= threshold.
    fn evict_up_to(&mut self, threshold: i64) -> Result<usize> {
        // Tantivy lacks a range-delete, so we delete matching docs by term.
        // For bulk eviction (common after cold flush of a whole segment) this
        // is O(evicted). If the workload requires it we can switch to a
        // rebuild-from-surviving-offsets strategy later.
        let evicted_before = self.doc_count;

        // Collect surviving offsets first — needed only if we rebuild.
        let searcher = self.reader.searcher();
        let mut to_delete: Vec<i64> = Vec::new();
        for seg_reader in searcher.segment_readers() {
            let store_reader = seg_reader
                .get_store_reader(0)
                .map_err(|e| anyhow!("hot store reader: {}", e))?;
            for doc_id in 0..seg_reader.max_doc() {
                if seg_reader.is_deleted(doc_id) {
                    continue;
                }
                let doc: tantivy::TantivyDocument = store_reader
                    .get(doc_id)
                    .map_err(|e| anyhow!("hot store get: {}", e))?;
                if let Some(off) = doc
                    .get_first(self.offset_field)
                    .and_then(|v| tantivy::schema::Value::as_i64(&v))
                {
                    if off <= threshold {
                        to_delete.push(off);
                    }
                }
            }
        }

        for off in &to_delete {
            let term = Term::from_field_i64(self.offset_field, *off);
            self.writer.delete_term(term);
        }
        self.make_visible()?;

        // Recompute counters from the post-commit reader.
        self.recompute_counters()?;

        let evicted = evicted_before.saturating_sub(self.doc_count);
        trace!(
            topic = %self.topic,
            partition = self.partition,
            threshold,
            evicted,
            "hot: evicted docs"
        );
        Ok(evicted)
    }

    fn recompute_counters(&mut self) -> Result<()> {
        let searcher = self.reader.searcher();
        let mut count = 0usize;
        let mut min: Option<i64> = None;
        let mut max: Option<i64> = None;
        for seg_reader in searcher.segment_readers() {
            let store_reader = seg_reader
                .get_store_reader(0)
                .map_err(|e| anyhow!("hot store reader: {}", e))?;
            for doc_id in 0..seg_reader.max_doc() {
                if seg_reader.is_deleted(doc_id) {
                    continue;
                }
                count += 1;
                let doc: tantivy::TantivyDocument = store_reader
                    .get(doc_id)
                    .map_err(|e| anyhow!("hot store get: {}", e))?;
                if let Some(off) = doc
                    .get_first(self.offset_field)
                    .and_then(|v| tantivy::schema::Value::as_i64(&v))
                {
                    min = Some(min.map_or(off, |m| m.min(off)));
                    max = Some(max.map_or(off, |m| m.max(off)));
                }
            }
        }
        self.doc_count = count;
        self.min_offset = min;
        self.max_offset = max;
        Ok(())
    }

    fn over_capacity(&self) -> bool {
        self.doc_count > self.config.max_docs_per_partition
    }

    /// HP-1.7: Total doc count across segments INCLUDING tombstoned deletes.
    /// Tantivy's `delete_term` marks docs invisible but keeps their bytes
    /// in segments until merge, so this is the honest proxy for RAM pressure.
    fn total_including_deleted(&self) -> usize {
        let searcher = self.reader.searcher();
        searcher
            .segment_readers()
            .iter()
            .map(|s| s.max_doc() as usize)
            .sum()
    }

    /// HP-1.7: When the partition's doc count exceeds `max_docs_per_partition`,
    /// evict the oldest offsets so RAM stays bounded. Called from the commit
    /// timer, so eviction piggy-backs on an existing visibility flip and does
    /// not add disk I/O.
    ///
    /// Strategy: keep a sliding window of the most-recent `max_docs_per_partition`
    /// offsets. Offsets below `max_offset - max_docs` are dropped. Since WAL
    /// offsets are dense in the common case, this keeps doc count within ~2×
    /// the cap even with batch-level granularity.
    fn trim_to_capacity(&mut self) -> Result<usize> {
        if !self.over_capacity() {
            return Ok(0);
        }
        let Some(max_off) = self.max_offset else {
            return Ok(0);
        };
        let keep = self.config.max_docs_per_partition as i64;
        let threshold = max_off.saturating_sub(keep);
        if threshold < 0 {
            return Ok(0);
        }
        self.evict_up_to(threshold)
    }
}

/// Per-(topic, partition) in-memory Tantivy indexes for NRT search.
pub struct HotTextIndex {
    partitions: DashMap<(String, i32), Arc<RwLock<HotPartitionIndex>>>,
    config: HotTextConfig,
}

impl HotTextIndex {
    pub fn new(config: HotTextConfig) -> Self {
        Self {
            partitions: DashMap::new(),
            config,
        }
    }

    fn get_or_create(&self, topic: &str, partition: i32) -> Result<Arc<RwLock<HotPartitionIndex>>> {
        if let Some(entry) = self.partitions.get(&(topic.to_string(), partition)) {
            return Ok(entry.clone());
        }
        let part = HotPartitionIndex::new(topic.to_string(), partition, self.config.clone())?;
        let arc = Arc::new(RwLock::new(part));
        self.partitions
            .insert((topic.to_string(), partition), arc.clone());
        Ok(arc)
    }

    /// Insert a batch of docs into (topic, partition). Does not make them
    /// visible to readers — call `commit` (or rely on the background timer).
    pub async fn add_batch(&self, topic: &str, partition: i32, docs: &[HotDoc]) -> Result<()> {
        if docs.is_empty() {
            return Ok(());
        }
        let part = self.get_or_create(topic, partition)?;
        let mut guard = part.write().await;
        guard.add_batch(docs)?;
        chronik_monitoring::MetricsRecorder::record_hot_text_add(docs.len() as u64);
        if guard.over_capacity() {
            // Over-capacity is recovered by the commit-timer trim + hard-reset
            // path (HP-1.7). Just trace here — noisy at bulk throughput.
            trace!(
                topic, partition,
                doc_count = guard.doc_count,
                limit = self.config.max_docs_per_partition,
                "hot text: over capacity (will be trimmed/reset on next commit)"
            );
        }
        Ok(())
    }

    /// Flip buffer visibility for a single partition. Background-timer path.
    /// Also trims the partition to `max_docs_per_partition` if over capacity.
    pub async fn commit(&self, topic: &str, partition: i32) -> Result<()> {
        let needs_reset = {
            let Some(entry) = self.partitions.get(&(topic.to_string(), partition)) else {
                return Ok(());
            };
            let part = entry.clone();
            drop(entry);
            let mut guard = part.write().await;
            guard.make_visible()?;
            chronik_monitoring::MetricsRecorder::record_hot_text_commit();
            // HP-3 Item 2: emit a visibility lag sample for the newest doc
            // that just became visible. Consume the marker so we only sample
            // on commits that actually made new docs visible.
            if let Some(ts_ms) = guard.last_add_produce_ts_ms.take() {
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(ts_ms);
                let lag = (now_ms - ts_ms).max(0) as u64;
                chronik_monitoring::MetricsRecorder::record_hot_text_visibility_lag(lag);
            }
            // HP-1.7: bound RAM — drop offsets outside the sliding window.
            let trimmed = guard.trim_to_capacity()?;
            if trimmed > 0 {
                trace!(topic, partition, trimmed, "hot text: trimmed on commit");
                chronik_monitoring::MetricsRecorder::record_hot_text_trim(trimmed as u64);
            }
            // HP-1.7: Tantivy's delete_term marks docs as tombstones but does
            // not reclaim their bytes in a RAMDirectory until merge. Under
            // sustained high-throughput ingest the heap grows unboundedly.
            // When the total (live + tombstoned) count is ≥ 2× the cap we
            // drop the partition entirely and let the next add_batch recreate
            // it fresh. The coverage gap is brief and cold search continues
            // to serve these offsets.
            guard.total_including_deleted() >= 2 * guard.config.max_docs_per_partition.max(1)
        };
        if needs_reset {
            tracing::debug!(
                hot_path = "text",
                event = "partition_reset",
                topic,
                partition,
                "hot text: resetting partition RAMDirectory to reclaim tombstones"
            );
            chronik_monitoring::MetricsRecorder::record_hot_text_reset();
            self.partitions.remove(&(topic.to_string(), partition));
        }
        Ok(())
    }

    /// Flip buffer visibility across every known partition. Coarse test helper.
    pub async fn commit_all(&self) -> Result<()> {
        let keys: Vec<_> = self.partitions.iter().map(|e| e.key().clone()).collect();
        for (topic, partition) in keys {
            self.commit(&topic, partition).await?;
        }
        Ok(())
    }

    /// Search a single (topic, partition).
    pub async fn search_partition(
        &self,
        topic: &str,
        partition: i32,
        query: &str,
        top_k: usize,
    ) -> Result<Vec<HotHit>> {
        let Some(entry) = self.partitions.get(&(topic.to_string(), partition)) else {
            return Ok(Vec::new());
        };
        let part = entry.clone();
        drop(entry);
        let guard = part.read().await;
        let start = std::time::Instant::now();
        let result = guard.search(query, top_k);
        chronik_monitoring::MetricsRecorder::record_hot_text_search(start.elapsed());
        result
    }

    /// Search all partitions of a topic, merged by score, dedup by offset,
    /// top_k returned.
    pub async fn search_topic(&self, topic: &str, query: &str, top_k: usize) -> Result<Vec<HotHit>> {
        let keys: Vec<(String, i32)> = self
            .partitions
            .iter()
            .filter(|e| e.key().0 == topic)
            .map(|e| e.key().clone())
            .collect();

        let mut all = Vec::new();
        for (t, p) in keys {
            let mut hits = self.search_partition(&t, p, query, top_k).await?;
            all.append(&mut hits);
        }
        // dedup by (partition, offset); keep highest score on tie
        all.sort_by(|a, b| {
            a.partition
                .cmp(&b.partition)
                .then(a.offset.cmp(&b.offset))
                .then(b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal))
        });
        all.dedup_by(|a, b| a.partition == b.partition && a.offset == b.offset);
        all.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        all.truncate(top_k);
        Ok(all)
    }

    /// Drop all docs up to (and including) `threshold` offset for
    /// (topic, partition). Called by WalIndexer after a successful cold flush.
    pub async fn evict_up_to(
        &self,
        topic: &str,
        partition: i32,
        threshold: i64,
    ) -> Result<usize> {
        let Some(entry) = self.partitions.get(&(topic.to_string(), partition)) else {
            return Ok(0);
        };
        let part = entry.clone();
        drop(entry);
        let mut guard = part.write().await;
        let evicted = guard.evict_up_to(threshold)?;
        if evicted > 0 {
            chronik_monitoring::MetricsRecorder::record_hot_text_evict(evicted as u64);
        }
        Ok(evicted)
    }

    /// Doc count for a single partition. Mostly for tests / metrics.
    pub async fn doc_count(&self, topic: &str, partition: i32) -> usize {
        let Some(entry) = self.partitions.get(&(topic.to_string(), partition)) else {
            return 0;
        };
        let part = entry.clone();
        drop(entry);
        let guard = part.read().await;
        guard.doc_count
    }

    pub fn partition_keys(&self) -> Vec<(String, i32)> {
        self.partitions.iter().map(|e| e.key().clone()).collect()
    }

    /// HP-1.4: Detached eviction helper used by the `ColdFlushListener` impl.
    /// Spawns a tokio task; safe to call from non-async contexts.
    fn spawn_eviction(self: &Arc<Self>, topic: String, partition: i32, max_offset: i64) {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            match this.evict_up_to(&topic, partition, max_offset).await {
                Ok(n) if n > 0 => trace!(topic, partition, evicted = n, "hot text evicted"),
                Ok(_) => {}
                Err(e) => tracing::debug!(
                    topic, partition, "hot text eviction failed: {}", e
                ),
            }
        });
    }

    /// Spawn a background task that flips buffer visibility across all known
    /// partitions on a fixed interval. Returns the JoinHandle so the caller
    /// can abort on shutdown.
    ///
    /// No disk I/O — this only makes buffered in-memory docs visible to
    /// in-memory readers (Tantivy `commit` semantics on RAMDirectory).
    pub fn start_commit_timer(
        self: Arc<Self>,
        interval: std::time::Duration,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // Skip the immediate first tick
            ticker.tick().await;
            loop {
                ticker.tick().await;
                if let Err(e) = self.commit_all().await {
                    warn!("hot text index commit_all failed: {}", e);
                }
            }
        })
    }
}

/// HP-1.4: Adapter that owns an `Arc<HotTextIndex>` and implements
/// `ColdFlushListener`. The indirection exists because `notify_cold_flushed`
/// takes `&self` but eviction needs an `Arc<Self>` to spawn a detached task.
/// Register the adapter — not `HotTextIndex` directly — via
/// `WalIndexer::set_cold_flush_listener`.
pub struct HotTextColdFlushAdapter {
    inner: Arc<HotTextIndex>,
}

impl HotTextColdFlushAdapter {
    pub fn new(inner: Arc<HotTextIndex>) -> Arc<Self> {
        Arc::new(Self { inner })
    }
}

impl chronik_storage::wal_indexer::ColdFlushListener for HotTextColdFlushAdapter {
    fn notify_cold_flushed(&self, topic: String, partition: i32, max_offset: i64) {
        self.inner.spawn_eviction(topic, partition, max_offset);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_doc(offset: i64, value: &str) -> HotDoc {
        HotDoc {
            offset,
            timestamp: 1_700_000_000_000 + offset,
            key: None,
            value: value.to_string(),
            headers: None,
        }
    }

    #[tokio::test]
    async fn insert_and_search_single_partition() {
        let idx = HotTextIndex::new(HotTextConfig::default());
        let docs = vec![
            make_doc(0, "leather sofa in the living room"),
            make_doc(1, "running shoes for marathons"),
            make_doc(2, "blue velvet chair"),
        ];
        idx.add_batch("products", 0, &docs).await.unwrap();
        idx.commit("products", 0).await.unwrap();

        let hits = idx
            .search_partition("products", 0, "running", 10)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].offset, 1);

        let hits = idx.search_topic("products", "chair", 10).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].offset, 2);
    }

    #[tokio::test]
    async fn insert_1k_docs_and_query() {
        let idx = HotTextIndex::new(HotTextConfig::default());
        let docs: Vec<HotDoc> = (0..1000)
            .map(|i| {
                let v = if i % 7 == 0 {
                    format!("doc {} the quick brown fox jumps", i)
                } else {
                    format!("doc {} random filler text for benchmarking", i)
                };
                make_doc(i, &v)
            })
            .collect();
        idx.add_batch("bench", 0, &docs).await.unwrap();
        idx.commit("bench", 0).await.unwrap();

        assert_eq!(idx.doc_count("bench", 0).await, 1000);

        let hits = idx.search_partition("bench", 0, "fox", 20).await.unwrap();
        // Every 7th doc (0, 7, 14, ..., 994) — 143 matches; top_k caps at 20.
        assert_eq!(hits.len(), 20);
        for h in &hits {
            assert!(h.offset % 7 == 0, "fox only in multiples-of-7 docs");
        }
    }

    #[tokio::test]
    async fn evict_drops_old_docs() {
        let idx = HotTextIndex::new(HotTextConfig::default());
        let docs: Vec<HotDoc> = (0..100).map(|i| make_doc(i, "payload")).collect();
        idx.add_batch("t", 0, &docs).await.unwrap();
        idx.commit("t", 0).await.unwrap();

        assert_eq!(idx.doc_count("t", 0).await, 100);

        let evicted = idx.evict_up_to("t", 0, 49).await.unwrap();
        assert_eq!(evicted, 50, "offsets 0..=49 should evict");

        let remaining = idx.doc_count("t", 0).await;
        assert_eq!(remaining, 50);

        // Newer docs still searchable
        let hits = idx.search_partition("t", 0, "payload", 200).await.unwrap();
        assert_eq!(hits.len(), 50);
        for h in &hits {
            assert!(h.offset >= 50, "offset {} should have been evicted", h.offset);
        }
    }

    #[tokio::test]
    async fn search_topic_merges_partitions_and_dedups() {
        let idx = HotTextIndex::new(HotTextConfig::default());
        idx.add_batch("t", 0, &[make_doc(0, "red sofa")]).await.unwrap();
        idx.add_batch("t", 1, &[make_doc(0, "red chair")]).await.unwrap();
        idx.commit("t", 0).await.unwrap();
        idx.commit("t", 1).await.unwrap();

        let hits = idx.search_topic("t", "red", 10).await.unwrap();
        // Same offset on different partitions — both should appear.
        assert_eq!(hits.len(), 2);
        let parts: std::collections::HashSet<i32> = hits.iter().map(|h| h.partition).collect();
        assert_eq!(parts, [0, 1].into_iter().collect());
    }

    #[tokio::test]
    async fn stemming_flows_through_from_shared_analyzer() {
        let idx = HotTextIndex::new(HotTextConfig::default());
        idx.add_batch(
            "t",
            0,
            &[
                make_doc(0, "running shoes"),
                make_doc(1, "leather sofa"),
            ],
        )
        .await
        .unwrap();
        idx.commit("t", 0).await.unwrap();

        // Stemmer: "run" should match "running"
        let hits = idx.search_partition("t", 0, "run", 10).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].offset, 0);
    }

    #[tokio::test]
    async fn empty_query_returns_no_hits() {
        let idx = HotTextIndex::new(HotTextConfig::default());
        idx.add_batch("t", 0, &[make_doc(0, "payload")]).await.unwrap();
        idx.commit("t", 0).await.unwrap();
        let hits = idx.search_partition("t", 0, "   ", 10).await.unwrap();
        assert!(hits.is_empty());
    }

    #[tokio::test]
    async fn commit_hard_resets_partition_under_ram_pressure() {
        let config = HotTextConfig {
            writer_heap_bytes: 15_000_000,
            max_docs_per_partition: 50,
        };
        let idx = HotTextIndex::new(config);
        // Produce enough docs across several batches to build up tombstones.
        for base in (0..5).map(|i| i * 100) {
            let docs: Vec<HotDoc> = (base..base + 100)
                .map(|o| make_doc(o, "payload"))
                .collect();
            idx.add_batch("t", 0, &docs).await.unwrap();
            idx.commit("t", 0).await.unwrap();
        }
        // Each commit trimmed live docs but accumulated tombstones. At some
        // point the partition is reset — after reset it's absent from the map.
        // Either way, total live docs should be bounded.
        let remaining = idx.doc_count("t", 0).await;
        assert!(
            remaining <= 50,
            "post-reset live doc count {} should be ≤ 50",
            remaining
        );
    }

    #[tokio::test]
    async fn commit_trims_to_capacity() {
        let config = HotTextConfig {
            writer_heap_bytes: 15_000_000,
            max_docs_per_partition: 100,
        };
        let idx = HotTextIndex::new(config);
        let docs: Vec<HotDoc> = (0..500).map(|i| make_doc(i, "payload")).collect();
        idx.add_batch("t", 0, &docs).await.unwrap();
        assert_eq!(idx.doc_count("t", 0).await, 500);

        // commit triggers the trim because we're over capacity
        idx.commit("t", 0).await.unwrap();

        // After trim: max_offset=499, threshold=499-100=399 → offsets >399 survive → 100 docs
        let remaining = idx.doc_count("t", 0).await;
        assert!(
            remaining <= 100,
            "doc count {} should be ≤ 100 after trim",
            remaining
        );
        // Oldest offsets should be gone
        let hits = idx.search_partition("t", 0, "payload", 200).await.unwrap();
        for h in &hits {
            assert!(h.offset > 399, "offset {} should have been trimmed", h.offset);
        }
    }

    #[tokio::test]
    async fn cold_flush_adapter_triggers_eviction() {
        use chronik_storage::wal_indexer::ColdFlushListener;

        let idx = Arc::new(HotTextIndex::new(HotTextConfig::default()));
        idx.add_batch("t", 0, &[make_doc(0, "old"), make_doc(1, "old")])
            .await
            .unwrap();
        idx.add_batch("t", 0, &[make_doc(2, "new"), make_doc(3, "new")])
            .await
            .unwrap();
        idx.commit("t", 0).await.unwrap();
        assert_eq!(idx.doc_count("t", 0).await, 4);

        let adapter = HotTextColdFlushAdapter::new(Arc::clone(&idx));
        // Pretend WalIndexer just committed offsets 0..=1 to cold Tantivy.
        adapter.notify_cold_flushed("t".to_string(), 0, 1);

        // Eviction is spawned — wait for it to land.
        for _ in 0..50 {
            if idx.doc_count("t", 0).await == 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(idx.doc_count("t", 0).await, 2);
        let hits = idx.search_partition("t", 0, "new", 10).await.unwrap();
        assert_eq!(hits.len(), 2);
        let hits_old = idx.search_partition("t", 0, "old", 10).await.unwrap();
        assert!(hits_old.is_empty());
    }

    #[tokio::test]
    async fn commit_timer_makes_buffered_docs_visible() {
        let idx = Arc::new(HotTextIndex::new(HotTextConfig::default()));
        let handle = Arc::clone(&idx)
            .start_commit_timer(std::time::Duration::from_millis(50));

        idx.add_batch("t", 0, &[make_doc(0, "hot path is live")])
            .await
            .unwrap();

        // Before the timer fires, the doc is buffered but not visible.
        let hits_pre = idx.search_partition("t", 0, "hot", 10).await.unwrap();
        assert!(hits_pre.is_empty(), "docs shouldn't be visible pre-commit");

        // Wait for at least two ticks of the timer.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let hits_post = idx.search_partition("t", 0, "hot", 10).await.unwrap();
        assert_eq!(hits_post.len(), 1);

        handle.abort();
    }

    #[tokio::test]
    async fn missing_partition_returns_empty() {
        let idx = HotTextIndex::new(HotTextConfig::default());
        let hits = idx.search_partition("nope", 0, "anything", 10).await.unwrap();
        assert!(hits.is_empty());
        assert_eq!(idx.doc_count("nope", 0).await, 0);
        assert_eq!(idx.evict_up_to("nope", 0, 10).await.unwrap(), 0);
    }
}
