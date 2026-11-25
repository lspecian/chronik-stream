//! In-memory Tantivy index shadowing the WAL tail for near-real-time search.
//!
//! See `docs/ROADMAP_HOT_PATH.md` (HP-1.1) for the full design.
//!
//! The hot path is RAM-only. "writer" and "commit" below refer to Tantivy's
//! in-memory API (RAMDirectory, buffer visibility flip) â there is no disk I/O.
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

use std::ops::Bound;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use dashmap::DashMap;
use tantivy::{
    collector::{Count, TopDocs},
    query::{QueryParser, RangeQuery},
    schema::{Field, NumericOptions, Schema, STORED, STRING, TEXT},
    Index, IndexReader, IndexWriter, ReloadPolicy, Term,
};
use tokio::sync::RwLock;
use tracing::{debug, trace, warn};

/// Minimum heap a Tantivy writer accepts.
const TANTIVY_MIN_HEAP: usize = 15_000_000;

/// Live hot writers across ALL partitions. Each is >=15MB of heap budget plus
/// indexing threads, so during a bulk topic-creation burst (thousands of
/// partitions written within one commit window) unbounded writers OOM the node
/// (measured: hot text alone added ~11GB at a 2000-topic burst; the idle-release
/// path cannot keep up because the commit timer walks dirty partitions
/// sequentially). `add_batch` consults this to commit+release eagerly once the
/// burst cap is exceeded â bursty partitions pay one cheap RAM-only commit per
/// batch while steady producers stay under the cap and keep their writers.

/// Burst cap on concurrent live hot writers (env: CHRONIK_HOT_TEXT_MAX_LIVE_WRITERS).
/// 64 writers â 1GB worst-case budget.
fn hot_writer_burst_cap() -> usize {
    static CAP: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *CAP.get_or_init(|| {
        std::env::var("CHRONIK_HOT_TEXT_MAX_LIVE_WRITERS")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|&n| n > 0)
            .unwrap_or(64)
    })
}

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

/// Hot hits in [`crate::ranking::hit_order`]'s order (one topic, so: score,
/// newest, partition, highest offset), so the top `top_k` across partitions is
/// the same slice the final merge would pick.
fn hot_hit_order(a: &HotHit, b: &HotHit) -> std::cmp::Ordering {
    b.score
        .total_cmp(&a.score)
        .then(b.timestamp.div_euclid(1000).cmp(&a.timestamp.div_euclid(1000)))
        .then(a.partition.cmp(&b.partition))
        .then(b.offset.cmp(&a.offset))
}

/// Configuration for the hot text index.
#[derive(Debug, Clone)]
pub struct HotTextConfig {
    /// Writer heap budget in bytes (Tantivy requires >= 15MB).
    pub writer_heap_bytes: usize,
    /// Docs per partition kept once an archive covers the older ones. Offsets
    /// no archive covers yet are kept beyond this (up to the hard cap): the
    /// hot index is the only place they can be searched.
    pub max_docs_per_partition: usize,
    /// Memory bound per partition: past it the oldest docs are dropped even
    /// when no archive covers them (counted, and logged), so a stalled indexer
    /// cannot exhaust memory.
    pub hard_max_docs_per_partition: usize,
    /// Heap of a burst writer: a partition indexing faster than one small
    /// writer keeps up with gets one (`CHRONIK_HOT_TEXT_BURST_WRITER_HEAP_BYTES`).
    pub burst_writer_heap_bytes: usize,
    /// Indexing threads of a burst writer.
    pub burst_writer_threads: usize,
    /// Burst writers at once, across all partitions (`CHRONIK_HOT_TEXT_MAX_BURST_WRITERS`;
    /// 0 turns them off).
    pub max_burst_writers: usize,
    /// Records added between two commits (100 ms apart) that make a partition
    /// a burst.
    pub burst_threshold_docs: usize,
}

impl HotTextConfig {
    /// The hard cap when none is configured: ten times the soft cap. A burst
    /// is kept until its archive is written (up to about a minute of ingest).
    pub fn default_hard_max(max_docs_per_partition: usize) -> usize {
        max_docs_per_partition.saturating_mul(10)
    }
}

impl Default for HotTextConfig {
    fn default() -> Self {
        Self {
            writer_heap_bytes: TANTIVY_MIN_HEAP,
            max_docs_per_partition: 100_000,
            hard_max_docs_per_partition: Self::default_hard_max(100_000),
            burst_writer_heap_bytes: 256 * 1024 * 1024,
            burst_writer_threads: std::thread::available_parallelism().map(|n| n.get() / 2).unwrap_or(1).clamp(1, 8),
            max_burst_writers: 4,
            burst_threshold_docs: 2_000,
        }
    }
}

/// Per-partition in-memory Tantivy index.
struct HotPartitionIndex {
    topic: String,
    /// Shared live-writer counter owned by the parent HotTextIndex; updated by
    /// ensure_writer/release_writer so add_batch can enforce the burst cap.
    live_writers: Arc<std::sync::atomic::AtomicUsize>,
    partition: i32,
    /// Burst writers live across all partitions (shared, capped).
    burst_writers: Arc<std::sync::atomic::AtomicUsize>,
    /// This partition holds (or will create) a burst writer.
    burst: bool,
    /// Records added since the last commit: the indexing rate.
    adds_since_commit: usize,
    /// Consecutive commits a burst writer saw little work.
    quiet_ticks: u32,
    index: Index,
    /// Lazily-held writer. A Tantivy writer costs a >=15MB heap budget plus
    /// indexing threads PER INSTANCE, and one instance per (topic, partition)
    /// held forever is what OOM-killed nodes at a few thousand topics
    /// (measured: hot text alone added ~11GB for 2000 topics). The writer is
    /// only needed while writes are pending: it is created on demand by
    /// `ensure_writer` and dropped by the commit timer once the partition goes
    /// idle. The in-RAM `index`/`reader` stay alive, so searchability is
    /// unaffected â only the mutation machinery is released.
    writer: Option<IndexWriter>,
    /// Uncommitted adds/deletes since the last visibility flip. Lets the
    /// commit timer skip idle partitions instead of churning every writer
    /// (and lets it know when it is safe to release the writer).
    dirty: bool,
    /// Consecutive commit-timer ticks with no pending work. The writer is
    /// released after a short grace period rather than immediately, so a
    /// steady producer slightly slower than the tick interval does not
    /// recreate its writer every other tick.
    idle_ticks: u8,
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
    /// Field-level index of JSON-object values; same field and options as the
    /// archived segments, so a field query means the same thing hot and cold.
    json_field: Field,
    doc_count: usize,
    min_offset: Option<i64>,
    max_offset: Option<i64>,
    config: HotTextConfig,
}

impl Drop for HotPartitionIndex {
    /// A partition dropped with a writer (its topic deleted, `forget_topic`)
    /// gives back its writer counts, or the caps on live and burst writers
    /// would count it forever.
    fn drop(&mut self) {
        if self.writer.take().is_some() {
            self.live_writers.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        }
        if std::mem::take(&mut self.burst) {
            self.burst_writers.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

impl HotPartitionIndex {
    fn new(
        topic: String,
        partition: i32,
        config: HotTextConfig,
        live_writers: Arc<std::sync::atomic::AtomicUsize>,
        burst_writers: Arc<std::sync::atomic::AtomicUsize>,
    ) -> Result<Self> {
        let mut schema_builder = Schema::builder();
        let topic_field = schema_builder.add_text_field("topic", STRING | STORED);
        let partition_field = schema_builder.add_i64_field(
            "partition",
            NumericOptions::default().set_indexed().set_stored(),
        );
        let offset_field = schema_builder.add_i64_field(
            "offset",
            NumericOptions::default().set_indexed().set_stored().set_fast(),
        );
        let timestamp_field = schema_builder.add_i64_field(
            "timestamp",
            NumericOptions::default().set_indexed().set_stored().set_fast(),
        );
        // `key` is STRING â Kafka keys are opaque identifiers, not prose. Indexing
        // them through the English analyzer drops stopword-shaped keys (e.g. "A",
        // "the") entirely and mangles everything else by stemming/lowercasing. Keep
        // this consistent with the cold Tantivy schema, where `_key` is also
        // STRING; otherwise hot and cold paths return different hit sets for the same
        // query. See docs/SEARCH_FIELD_MODEL_INVESTIGATION.md.
        let key_field = schema_builder.add_text_field("key", STRING | STORED);
        let value_field = schema_builder.add_text_field("value", TEXT | STORED);
        let headers_field = schema_builder.add_text_field("headers", TEXT | STORED);
        let json_field = schema_builder.add_json_field(
            chronik_storage::json_index::JSON_FIELD,
            chronik_storage::json_index::json_field_options(),
        );
        let schema = schema_builder.build();

        let index = Index::create_in_ram(schema);
        chronik_storage::register_analyzer(&index);

        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()
            .map_err(|e| anyhow!("create hot reader for {}-{}: {}", topic, partition, e))?;

        Ok(Self {
            topic,
            partition,
            index,
            live_writers,
            burst_writers,
            burst: false,
            adds_since_commit: 0,
            quiet_ticks: 0,
            // Created on first write by ensure_writer(), not up front: most
            // partitions in a many-topic deployment are idle at any moment.
            writer: None,
            dirty: false,
            idle_ticks: 0,
            reader,
            last_add_produce_ts_ms: None,
            topic_field,
            partition_field,
            offset_field,
            timestamp_field,
            key_field,
            value_field,
            headers_field,
            json_field,
            doc_count: 0,
            min_offset: None,
            max_offset: None,
            config,
        })
    }

    /// Get the writer, creating it if this partition has none. See the field
    /// docs on `writer` for why it is lazy.
    fn ensure_writer(&mut self) -> Result<&mut IndexWriter> {
        if self.writer.is_none() {
            let writer = if self.burst {
                let threads = self.config.burst_writer_threads.max(1);
                let heap = self.config.burst_writer_heap_bytes.max(TANTIVY_MIN_HEAP * threads);
                self.index.writer_with_num_threads(threads, heap)
            } else {
                self.index.writer(self.config.writer_heap_bytes.max(TANTIVY_MIN_HEAP))
            };
            let writer = writer
                .map_err(|e| anyhow!("create hot writer for {}-{}: {}", self.topic, self.partition, e))?;
            self.writer = Some(writer);
            self.live_writers.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        Ok(self.writer.as_mut().expect("writer just ensured"))
    }

    /// Drop the writer, freeing its heap arena and indexing threads. Committed
    /// docs remain searchable through `reader`; the next write recreates the
    /// writer via `ensure_writer`. Callers must only release a clean writer â
    /// dropping one with uncommitted adds would lose them.
    fn release_writer(&mut self) {
        debug_assert!(!self.dirty, "releasing a dirty hot writer would drop pending docs");
        if self.writer.take().is_some() {
            self.live_writers.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        }
        if std::mem::take(&mut self.burst) {
            self.burst_writers.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        }
        self.quiet_ticks = 0;
    }

    fn add_batch(&mut self, docs: &[HotDoc]) -> Result<()> {
        // Build documents first (immutable borrows of the schema fields), then
        // feed them to the lazily-created writer.
        let mut tantivy_docs = Vec::with_capacity(docs.len());
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
            if let Some(object) = chronik_storage::json_index::json_object(d.value.as_bytes()) {
                doc.add_object(
                    self.json_field,
                    object.into_iter().map(|(k, v)| (k, v.into())).collect(),
                );
            }
            if let Some(h) = &d.headers {
                doc.add_text(self.headers_field, h);
            }
            tantivy_docs.push(doc);
        }

        let writer = self.ensure_writer()?;
        for doc in tantivy_docs {
            writer
                .add_document(doc)
                .map_err(|e| anyhow!("hot add_document: {}", e))?;
        }
        self.dirty = true;
        self.idle_ticks = 0;
        self.adds_since_commit += docs.len();

        for d in docs {
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

    /// Flip in-memory buffer visibility. Tantivy name is `commit` â no disk writes.
    /// Returns whether there was pending work: a clean partition is a no-op, so
    /// the commit timer does not churn thousands of idle writers every tick.
    fn make_visible(&mut self) -> Result<bool> {
        if !self.dirty {
            return Ok(false);
        }
        self.ensure_writer()?
            .commit()
            .map_err(|e| anyhow!("hot writer commit: {}", e))?;
        self.reader
            .reload()
            .map_err(|e| anyhow!("hot reader reload: {}", e))?;
        self.dirty = false;
        Ok(true)
    }

    fn search(&self, query_str: &str, top_k: usize) -> Result<Vec<HotHit>> {
        if query_str.trim().is_empty() {
            return Ok(Vec::new());
        }
        // Sanitize to a bag of words. Tantivy's QueryParser treats `(`, `)`,
        // `/`, `:`, `"`, `-`, etc. as query syntax, so an Elasticsearch-style
        // `match` on `_all` carrying natural-language punctuation — or a date
        // like "2023/05/30 (Tue) 23:40" — either errors or misparses to ZERO
        // hits. The cold Tantivy path tokenizes the match text as plain terms;
        // mirror that here so hot and cold agree (before this, hot search
        // silently returned nothing for any punctuated query, and results only
        // appeared once the cold indexer caught up ~30-45s later). Callers that
        // need structured queries use `search_topic_structured`, which is
        // unaffected.
        let sanitized: String = query_str
            .chars()
            .map(|c| if c.is_alphanumeric() || c.is_whitespace() { c } else { ' ' })
            .collect();
        let sanitized = sanitized.trim();
        if sanitized.is_empty() {
            return Ok(Vec::new());
        }
        let searcher = self.reader.searcher();
        let parser = QueryParser::for_index(
            &self.index,
            vec![self.value_field, self.key_field, self.headers_field],
        );
        let query = parser
            .parse_query(sanitized)
            .map_err(|e| anyhow!("parse hot query `{}`: {}", sanitized, e))?;
        let top_docs = crate::ranking::top_docs(&searcher, &query, top_k)
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

    /// Issue #2: Execute a caller-built Tantivy query against this
    /// partition's index. Used for structured queries (bool, etc.) where
    /// the flat-string path via `search()` would lose AND semantics.
    fn search_with_query(
        &self,
        query: &dyn tantivy::query::Query,
        top_k: usize,
    ) -> Result<Vec<HotHit>> {
        let searcher = self.reader.searcher();
        let top_docs = crate::ranking::top_docs(&searcher, query, top_k)
            .map_err(|e| anyhow!("hot structured search: {}", e))?;
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

    /// After a commit (nothing pending): give a bursting partition a large
    /// writer, and take it back once the burst is over.
    ///
    /// One small writer (the minimum heap: one indexing thread) indexes about
    /// 60K records/s, so a 1M-record burst took ~15 s to become searchable; a
    /// large one, ~2 s. Small by default because every active partition holds
    /// a writer and thousands of them once exhausted memory; large ones are
    /// capped across all partitions (`max_burst_writers`).
    fn adapt_writer(&mut self) {
        let adds = std::mem::take(&mut self.adds_since_commit);
        let threshold = self.config.burst_threshold_docs.max(1);
        if self.burst {
            // Back to small after ~5 s of commits well under the burst rate.
            self.quiet_ticks = if adds * 4 < threshold { self.quiet_ticks + 1 } else { 0 };
            if self.quiet_ticks >= 50 && !self.dirty {
                self.release_writer();
            }
            return;
        }
        if adds < threshold || self.dirty {
            return;
        }
        let max = self.config.max_burst_writers;
        let reserved = self
            .burst_writers
            .fetch_update(std::sync::atomic::Ordering::Relaxed, std::sync::atomic::Ordering::Relaxed, |n| {
                (n < max).then_some(n + 1)
            })
            .is_ok();
        if !reserved {
            return;
        }
        // The writer is clean (just committed): replacing it loses nothing.
        self.release_writer();
        self.burst = true;
        if let Err(e) = self.ensure_writer() {
            debug!(topic = %self.topic, partition = self.partition, "hot: burst writer not created: {}", e);
            self.burst = false;
            self.burst_writers.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        } else {
            trace!(topic = %self.topic, partition = self.partition, adds, "hot: burst writer");
        }
    }

    /// Docs with offsets in `[lo, hi]`.
    fn offset_range(&self, lo: i64, hi: i64) -> RangeQuery {
        RangeQuery::new(
            Bound::Included(Term::from_field_i64(self.offset_field, lo)),
            Bound::Included(Term::from_field_i64(self.offset_field, hi)),
        )
    }

    /// Drop all docs with offset <= threshold: one range delete, then the
    /// counters from the offset fast field (no stored document is decoded).
    fn evict_up_to(&mut self, threshold: i64) -> Result<usize> {
        if self.min_offset.is_none_or(|min| min > threshold) {
            return Ok(0);
        }
        let evicted_before = self.doc_count;
        let query = self.offset_range(i64::MIN, threshold);
        self.ensure_writer()?
            .delete_query(Box::new(query))
            .map_err(|e| anyhow!("hot delete: {}", e))?;
        self.dirty = true;
        self.make_visible()?;
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

    /// Drop every doc at or above `from` (the log was cut there). Returns
    /// how many went.
    fn drop_from(&mut self, from: i64) -> Result<usize> {
        if self.max_offset.is_none_or(|max| max < from) {
            return Ok(0);
        }
        let before = self.doc_count;
        let query = self.offset_range(from, i64::MAX);
        self.ensure_writer()?
            .delete_query(Box::new(query))
            .map_err(|e| anyhow!("hot delete: {}", e))?;
        self.dirty = true;
        self.make_visible()?;
        self.recompute_counters()?;
        Ok(before.saturating_sub(self.doc_count))
    }

    /// Live docs and their offset range, from the reader as it stands.
    fn recompute_counters(&mut self) -> Result<()> {
        let searcher = self.reader.searcher();
        let mut count = 0usize;
        let mut min: Option<i64> = None;
        let mut max: Option<i64> = None;
        for seg_reader in searcher.segment_readers() {
            let offsets = seg_reader
                .fast_fields()
                .i64("offset")
                .map_err(|e| anyhow!("hot offset column: {}", e))?;
            for doc_id in seg_reader.doc_ids_alive() {
                count += 1;
                if let Some(off) = offsets.first(doc_id) {
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
    /// Tantivy's deletes mark docs invisible but keep their bytes in segments
    /// until merge, so this is the honest proxy for RAM pressure.
    fn total_including_deleted(&self) -> usize {
        let searcher = self.reader.searcher();
        searcher
            .segment_readers()
            .iter()
            .map(|s| s.max_doc() as usize)
            .sum()
    }

    /// Keep the partition near `max_docs_per_partition` without making any
    /// record unsearchable. Called from the commit timer.
    ///
    /// Only offsets an archive covers (`archived_through`, the highest
    /// archived offset) are dropped, and of those only what lies outside the
    /// newest `max_docs_per_partition` offsets. Until the indexer archives a
    /// burst, the hot index is the only place it can be searched, so it grows
    /// past the soft cap. `hard_max_docs_per_partition` bounds that: past it
    /// the oldest docs go regardless, and how many of them no archive covered
    /// is returned so the caller can count and report them.
    ///
    /// Returns `(evicted, evicted_unarchived)`.
    fn trim_to_capacity(&mut self, archived_through: Option<i64>) -> Result<(usize, usize)> {
        if !self.over_capacity() {
            return Ok((0, 0));
        }
        let Some(max_off) = self.max_offset else {
            return Ok((0, 0));
        };
        let covered = archived_through.unwrap_or(-1);
        let window = max_off.saturating_sub(self.config.max_docs_per_partition as i64);
        let mut threshold = window.min(covered);
        let mut unarchived = 0usize;
        if self.doc_count > self.config.hard_max_docs_per_partition {
            let hard = max_off.saturating_sub(self.config.hard_max_docs_per_partition as i64);
            if hard > threshold {
                threshold = hard;
                if hard > covered {
                    let lost = self.offset_range(covered.saturating_add(1), hard);
                    unarchived = self
                        .reader
                        .searcher()
                        .search(&lost, &Count)
                        .map_err(|e| anyhow!("hot count: {}", e))?;
                }
            }
        }
        if threshold < 0 {
            return Ok((0, 0));
        }
        Ok((self.evict_up_to(threshold)?, unarchived))
    }

    /// Reclaim the memory of deleted docs once they outnumber the live ones
    /// (and the soft cap): merge every segment, which drops them. Deleting
    /// only marks docs; under steady trimming the marked bytes would otherwise
    /// grow without bound. Merging costs O(live docs) and runs after at least
    /// as many deletes, so it is amortised O(1) per evicted doc.
    ///
    /// Returns whether a merge ran. A merge the writer's own merge policy is
    /// already running on these segments is refused by Tantivy; that is not
    /// an error, the next commit retries.
    fn purge_deleted(&mut self) -> Result<bool> {
        let deleted = self.total_including_deleted().saturating_sub(self.doc_count);
        if deleted < self.doc_count.max(self.config.max_docs_per_partition.max(1)) {
            return Ok(false);
        }
        let segments = self
            .index
            .searchable_segment_ids()
            .map_err(|e| anyhow!("hot segments: {}", e))?;
        if segments.is_empty() {
            return Ok(false);
        }
        match self.ensure_writer()?.merge(&segments).wait() {
            Ok(_) => {}
            Err(e) => {
                debug!(topic = %self.topic, partition = self.partition, "hot: merge deferred: {}", e);
                return Ok(false);
            }
        }
        self.reader
            .reload()
            .map_err(|e| anyhow!("hot reader reload: {}", e))?;
        Ok(true)
    }
}

/// Per-(topic, partition) in-memory Tantivy indexes for NRT search.
pub struct HotTextIndex {
    partitions: DashMap<(String, i32), Arc<RwLock<HotPartitionIndex>>>,
    config: HotTextConfig,
    /// Live writers across all partitions of THIS index — the burst-cap input.
    live_writers: Arc<std::sync::atomic::AtomicUsize>,
    /// Burst (large) writers across all partitions of this index.
    burst_writers: Arc<std::sync::atomic::AtomicUsize>,
    /// Highest offset an archive covers, per partition, as announced by the
    /// indexer (or found in the catalogue at boot). Kept apart from the
    /// partition entries so it survives an idle partition being reaped. Only
    /// offsets at or below it may be trimmed.
    archived: DashMap<(String, i32), i64>,
    /// When the last "evicted before archived" warning was logged (unix s).
    unarchived_warned_at: std::sync::atomic::AtomicU64,
    /// Docs each partition held at its last commit, readable without the
    /// partition lock (produce consults it for throttled topics).
    held: DashMap<(String, i32), usize>,
    /// Records handed to the hot index by produce and not yet added (the
    /// hand-off is asynchronous, and under a burst it queues).
    queued: DashMap<(String, i32), usize>,
    /// Records of throttled topics admitted by produce and not yet handed
    /// over (see `try_reserve`).
    reserved: DashMap<(String, i32), usize>,
}

impl HotTextIndex {
    pub fn new(config: HotTextConfig) -> Self {
        Self {
            partitions: DashMap::new(),
            config,
            live_writers: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            burst_writers: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            archived: DashMap::new(),
            unarchived_warned_at: std::sync::atomic::AtomicU64::new(0),
            held: DashMap::new(),
            queued: DashMap::new(),
            reserved: DashMap::new(),
        }
    }

    fn get_or_create(&self, topic: &str, partition: i32) -> Result<Arc<RwLock<HotPartitionIndex>>> {
        if let Some(entry) = self.partitions.get(&(topic.to_string(), partition)) {
            return Ok(entry.clone());
        }
        let part = HotPartitionIndex::new(
            topic.to_string(),
            partition,
            self.config.clone(),
            Arc::clone(&self.live_writers),
            Arc::clone(&self.burst_writers),
        )?;
        let arc = Arc::new(RwLock::new(part));
        self.partitions
            .insert((topic.to_string(), partition), arc.clone());
        Ok(arc)
    }

    /// Record that an archive covers the partition up to `max_offset`
    /// (inclusive). Never lowers what is known.
    pub fn note_archived(&self, topic: &str, partition: i32, max_offset: i64) {
        self.archived
            .entry((topic.to_string(), partition))
            .and_modify(|v| *v = (*v).max(max_offset))
            .or_insert(max_offset);
    }

    /// Highest offset an archive is known to cover for the partition.
    pub fn archived_through(&self, topic: &str, partition: i32) -> Option<i64> {
        self.archived.get(&(topic.to_string(), partition)).map(|v| *v)
    }

    /// Most docs a partition holds before records no archive covers are
    /// dropped. Warm-up reads at most this many.
    pub fn hard_max_docs(&self) -> usize {
        self.config.hard_max_docs_per_partition
    }

    /// Records of the partition the hot index holds or will hold: docs at its
    /// last commit plus those handed over and not yet added. Past the soft cap
    /// they are records no archive covers yet.
    pub fn pending_docs(&self, topic: &str, partition: i32) -> usize {
        let key = (topic.to_string(), partition);
        self.held.get(&key).map(|v| *v).unwrap_or(0)
            + self.queued.get(&key).map(|v| *v).unwrap_or(0)
            + self.reserved.get(&key).map(|v| *v).unwrap_or(0)
    }

    /// Admit `n` more records of the partition if, with them, it stays within
    /// `limit` pending records (or nothing is pending, so a batch larger than
    /// the limit is not refused forever). Check and reservation are one step,
    /// so concurrent produce requests (a pipelining client sends many at
    /// once) cannot all pass a check that only one of them fits. Release with
    /// [`Self::release`] once the records were handed over (or not written).
    pub fn try_reserve(&self, topic: &str, partition: i32, n: usize, limit: usize) -> bool {
        let key = (topic.to_string(), partition);
        let others = self.held.get(&key).map(|v| *v).unwrap_or(0) + self.queued.get(&key).map(|v| *v).unwrap_or(0);
        let mut reserved = self.reserved.entry(key).or_insert(0);
        let pending = others + *reserved;
        if pending + n <= limit || pending == 0 {
            *reserved += n;
            true
        } else {
            false
        }
    }

    /// See [`Self::try_reserve`].
    pub fn release(&self, topic: &str, partition: i32, n: usize) {
        if let Some(mut v) = self.reserved.get_mut(&(topic.to_string(), partition)) {
            *v = v.saturating_sub(n);
        }
    }

    /// The topic was deleted: drop everything held for it, its archived
    /// watermarks included, so a topic recreated under the name starts clean.
    pub fn forget_topic(&self, topic: &str) {
        self.partitions.retain(|(t, _), _| t != topic);
        self.archived.retain(|(t, _), _| t != topic);
        self.held.retain(|(t, _), _| t != topic);
        self.queued.retain(|(t, _), _| t != topic);
        self.reserved.retain(|(t, _), _| t != topic);
    }

    /// The partition's log was cut at `new_end` (a follower discarding a
    /// divergent tail): drop the docs at or above it, and lower the archived
    /// watermark below it, since the archive was cut too.
    pub async fn truncate(&self, topic: &str, partition: i32, new_end: i64) -> Result<usize> {
        let key = (topic.to_string(), partition);
        if let Some(mut archived) = self.archived.get_mut(&key) {
            *archived = (*archived).min(new_end - 1);
        }
        let Some(part) = self.partitions.get(&key).map(|e| e.clone()) else {
            return Ok(0);
        };
        let mut guard = part.write().await;
        let dropped = guard.drop_from(new_end)?;
        self.held.insert(key, guard.doc_count);
        Ok(dropped)
    }

    /// `n` records of the partition are on their way to `add_batch`. Pair
    /// with [`Self::note_added`] once they are added (or skipped).
    pub fn note_queued(&self, topic: &str, partition: i32, n: usize) {
        *self.queued.entry((topic.to_string(), partition)).or_insert(0) += n;
    }

    /// See [`Self::note_queued`].
    pub fn note_added(&self, topic: &str, partition: i32, n: usize) {
        if let Some(mut v) = self.queued.get_mut(&(topic.to_string(), partition)) {
            *v = v.saturating_sub(n);
        }
    }

    /// Insert a batch of docs into (topic, partition). Does not make them
    /// visible to readers â call `commit` (or rely on the background timer).
    pub async fn add_batch(&self, topic: &str, partition: i32, docs: &[HotDoc]) -> Result<()> {
        if docs.is_empty() {
            return Ok(());
        }
        // A record at or below the archived watermark is usually just late:
        // produce hands records over asynchronously, and the indexer can
        // archive them from the WAL before they arrive here. A watermark
        // from an earlier life of the partition is cleared where that life
        // ends instead (`forget_topic`, `truncate`). (Clearing it on any such
        // record, as this used to, did so on every late batch under load:
        // archived records could then no longer be trimmed, filled the hot
        // index to its hard cap and were evicted as if unarchived.)
        let part = self.get_or_create(topic, partition)?;
        let mut guard = part.write().await;
        guard.add_batch(docs)?;
        // Counted from here, not from the next commit (see pending_docs).
        self.held.insert((topic.to_string(), partition), guard.doc_count);
        chronik_monitoring::MetricsRecorder::record_hot_text_add(docs.len() as u64);
        // Burst cap: during bulk topic creation thousands of partitions acquire
        // writers within one commit window and the sequential commit timer cannot
        // release them fast enough â unbounded writers OOM the node. Once too
        // many are live, make this batch visible NOW and release the writer
        // rather than waiting for the idle timer. RAM-only commit, ~ms; steady
        // high-throughput partitions stay under the cap and keep their writers.
        if self.live_writers.load(std::sync::atomic::Ordering::Relaxed) > hot_writer_burst_cap() {
            guard.make_visible()?;
            guard.release_writer();
        }
        if guard.over_capacity() {
            // Over-capacity is recovered by the commit-timer trim once an
            // archive covers the oldest docs. Just trace here: noisy at bulk
            // throughput.
            trace!(
                topic, partition,
                doc_count = guard.doc_count,
                limit = self.config.max_docs_per_partition,
                "hot text: over capacity (trimmed on commit once archived)"
            );
        }
        Ok(())
    }

    /// How many consecutive no-work commit ticks a partition may sit on its
    /// writer before it is released. A small grace period so a producer
    /// slightly slower than the tick interval does not thrash writer
    /// create/drop on alternating ticks.
    const WRITER_IDLE_RELEASE_TICKS: u8 = 2;

    /// Flip buffer visibility for a single partition. Background-timer path.
    /// Also trims the partition to `max_docs_per_partition` if over capacity,
    /// releases the writer once the partition goes idle, and reaps the entry
    /// entirely once it is both idle and empty (cold search covers evicted
    /// offsets). The release/reap steps are what keep hot-text memory bounded
    /// by the ACTIVE partition set instead of every partition ever written â
    /// one Tantivy writer per partition held forever OOM-killed nodes at a few
    /// thousand topics.
    pub async fn commit(&self, topic: &str, partition: i32) -> Result<()> {
        let archived_through = self.archived_through(topic, partition);
        let remove_entry = {
            let Some(entry) = self.partitions.get(&(topic.to_string(), partition)) else {
                return Ok(());
            };
            let part = entry.clone();
            drop(entry);
            let mut guard = part.write().await;
            let had_work = guard.make_visible()?;

            if had_work {
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
                // Bound RAM: drop archived offsets outside the sliding window
                // (and, past the hard cap, the oldest regardless).
                let (trimmed, unarchived) = guard.trim_to_capacity(archived_through)?;
                if trimmed > 0 {
                    trace!(topic, partition, trimmed, "hot text: trimmed on commit");
                    chronik_monitoring::MetricsRecorder::record_hot_text_trim(trimmed as u64);
                }
                if unarchived > 0 {
                    chronik_monitoring::MetricsRecorder::record_hot_text_evicted_unarchived(unarchived as u64);
                    self.warn_unarchived(topic, partition, unarchived, archived_through);
                }
                // Past the soft cap the hot index holds only records no archive
                // covers yet. Halfway to the hard cap, ask the indexer to start
                // on them now instead of at its next scheduled run.
                if guard.doc_count > self.config.hard_max_docs_per_partition / 2 {
                    chronik_storage::wal_indexer::request_indexing(topic, partition);
                }
                self.held.insert((topic.to_string(), partition), guard.doc_count);
                // Deletes only mark docs; merge them away once they outnumber
                // the live ones, or the heap grows under steady trimming. (This
                // used to drop the whole partition, newest records included.)
                if guard.purge_deleted()? {
                    trace!(topic, partition, "hot text: merged away deleted docs");
                    chronik_monitoring::MetricsRecorder::record_hot_text_purge();
                }
                guard.adapt_writer();
                false
            } else {
                // Idle tick: no adds or deletes since the last flip.
                guard.idle_ticks = guard.idle_ticks.saturating_add(1);
                if guard.idle_ticks >= Self::WRITER_IDLE_RELEASE_TICKS && guard.writer.is_some() {
                    trace!(topic, partition, "hot text: releasing idle writer");
                    guard.release_writer();
                }
                // Empty + idle + writer released â nothing left worth holding:
                // reap the whole entry (index + reader). The next produce
                // recreates it; cold search serves the evicted offsets.
                guard.doc_count == 0
                    && guard.writer.is_none()
                    && guard.idle_ticks >= Self::WRITER_IDLE_RELEASE_TICKS
            }
        };
        if remove_entry {
            self.partitions.remove(&(topic.to_string(), partition));
            self.held.remove(&(topic.to_string(), partition));
        }
        Ok(())
    }

    /// Records were dropped before any archive held them: they answer no
    /// search until the indexer catches up. Logged at most once a minute.
    fn warn_unarchived(&self, topic: &str, partition: i32, count: usize, archived_through: Option<i64>) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let last = self.unarchived_warned_at.load(std::sync::atomic::Ordering::Relaxed);
        if now.saturating_sub(last) >= 60
            && self
                .unarchived_warned_at
                .compare_exchange(last, now, std::sync::atomic::Ordering::Relaxed, std::sync::atomic::Ordering::Relaxed)
                .is_ok()
        {
            warn!(
                topic, partition, count, ?archived_through,
                hard_cap = self.config.hard_max_docs_per_partition,
                "hot text: hard cap reached, dropped records no archive holds yet; they are unsearchable \
                 until indexed (raise CHRONIK_HOT_TEXT_HARD_MAX_DOCS or check indexer lag)"
            );
        }
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
        all.sort_by(hot_hit_order);
        all.truncate(top_k);
        Ok(all)
    }

    /// Issue #2: Structured search across all partitions of a topic. The
    /// caller provides a closure that builds a Tantivy query given the
    /// per-partition schema + index (each HotPartitionIndex owns its own
    /// Tantivy Index, so queries can't be shared across partitions).
    ///
    /// Unlike `search_topic` (which routes through QueryParser and a flat
    /// query string), this path preserves the structure of bool queries â
    /// `bool.must` actually ANDs its clauses, `bool.should` actually ORs,
    /// etc.
    /// Run `f` on each partition of `topic` with its archived watermark (the
    /// highest offset an archive is known to cover), its index and a
    /// searcher, collecting what it returns. For counts and aggregations,
    /// which must skip what the archive already holds.
    pub async fn collect_topic<R, F>(&self, topic: &str, mut f: F) -> Vec<R>
    where
        F: FnMut(i32, Option<i64>, &tantivy::Index, &tantivy::Searcher) -> Option<R> + Send,
    {
        let keys: Vec<(String, i32)> =
            self.partitions.iter().filter(|e| e.key().0 == topic).map(|e| e.key().clone()).collect();
        let mut out = Vec::new();
        for (t, p) in keys {
            let Some(entry) = self.partitions.get(&(t.clone(), p)) else { continue };
            let part = entry.clone();
            drop(entry);
            let archived = self.archived_through(&t, p);
            let guard = part.read().await;
            let searcher = guard.reader.searcher();
            if let Some(r) = f(p, archived, &guard.index, &searcher) {
                out.push(r);
            }
        }
        out
    }

    pub async fn search_topic_structured<F>(
        &self,
        topic: &str,
        build_query: F,
        top_k: usize,
    ) -> Result<Vec<HotHit>>
    where
        F: Fn(
            &tantivy::schema::Schema,
            &tantivy::Index,
        ) -> Result<Box<dyn tantivy::query::Query>>,
    {
        let keys: Vec<(String, i32)> = self
            .partitions
            .iter()
            .filter(|e| e.key().0 == topic)
            .map(|e| e.key().clone())
            .collect();

        let mut all = Vec::new();
        for (t, p) in keys {
            let Some(entry) = self.partitions.get(&(t.clone(), p)) else {
                continue;
            };
            let part = entry.clone();
            drop(entry);
            let guard = part.read().await;
            // Build the Tantivy query per partition using that partition's
            // Index/Schema instance (Tantivy queries bind to a specific Index).
            let query = match build_query(&guard.index.schema(), &guard.index) {
                Ok(q) => q,
                Err(e) => {
                    trace!(topic = %t, partition = p, "hot structured build: {}", e);
                    continue;
                }
            };
            let start = std::time::Instant::now();
            match guard.search_with_query(query.as_ref(), top_k) {
                Ok(mut hits) => all.append(&mut hits),
                Err(e) => trace!(topic = %t, partition = p, "hot structured search: {}", e),
            }
            chronik_monitoring::MetricsRecorder::record_hot_text_search(start.elapsed());
        }

        // Dedup + top-k, same as search_topic.
        all.sort_by(|a, b| {
            a.partition
                .cmp(&b.partition)
                .then(a.offset.cmp(&b.offset))
                .then(b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal))
        });
        all.dedup_by(|a, b| a.partition == b.partition && a.offset == b.offset);
        all.sort_by(hot_hit_order);
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

    /// The lowest and highest offsets the partition's hot index holds.
    pub async fn offset_range(&self, topic: &str, partition: i32) -> Option<(i64, i64)> {
        let part = self.partitions.get(&(topic.to_string(), partition))?.clone();
        let guard = part.read().await;
        guard.min_offset.zip(guard.max_offset)
    }

    pub fn partition_keys(&self) -> Vec<(String, i32)> {
        self.partitions.iter().map(|e| e.key().clone()).collect()
    }

    /// HP-1.4: Detached eviction helper used by the `ColdFlushListener` impl.
    /// Spawns a tokio task; safe to call from non-async contexts.
    fn spawn_eviction(self: &Arc<Self>, topic: String, partition: i32, max_offset: i64) {
        self.note_archived(&topic, partition, max_offset);
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
    /// No disk I/O â this only makes buffered in-memory docs visible to
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
/// Register the adapter â not `HotTextIndex` directly â via
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

    /// Regression: a natural-language query carrying punctuation (parentheses,
    /// slashes, colons, a date) must still match. Tantivy's QueryParser treats
    /// those characters as query syntax, so feeding the raw `_all` match text
    /// returned ZERO hits (or errored) — hot search silently failed for any
    /// punctuated query until the cold indexer caught up ~30-45s later.
    /// `search` now sanitizes to a bag of words first.
    #[tokio::test]
    async fn punctuated_query_still_matches() {
        let idx = HotTextIndex::new(HotTextConfig::default());
        idx.add_batch(
            "t",
            0,
            &[make_doc(0, "I graduated with a degree in business administration")],
        )
        .await
        .unwrap();
        idx.commit("t", 0).await.unwrap();

        // An anchored question the way the read-time path builds it — parens,
        // slashes, colons, a date, a trailing '?'. Pre-fix this returned 0 hits.
        let q = "(Today is 2023/05/30 (Tue) 23:40.) What degree did I graduate with?";
        let hits = idx.search_topic("t", q, 10).await.unwrap();
        assert_eq!(hits.len(), 1, "punctuated query must match after sanitization");
        assert_eq!(hits[0].offset, 0);

        // A query that is ONLY punctuation matches nothing — and does not error.
        let none = idx.search_topic("t", "()/:-.", 10).await.unwrap();
        assert!(none.is_empty());
    }

    /// Regression (per-topic memory OOM): an idle partition must release its
    /// Tantivy writer (>=15MB heap budget + indexing threads each) while its
    /// docs stay searchable. One writer per partition held forever OOM-killed
    /// nodes at a few thousand topics.
    #[tokio::test]
    async fn idle_partition_releases_writer_but_stays_searchable() {
        let idx = HotTextIndex::new(HotTextConfig::default());
        idx.add_batch("t", 0, &[make_doc(0, "hello world")]).await.unwrap();
        idx.commit("t", 0).await.unwrap(); // had work: commit + keep writer

        // Idle ticks: no new docs. Writer released after the grace period.
        for _ in 0..HotTextIndex::WRITER_IDLE_RELEASE_TICKS {
            idx.commit("t", 0).await.unwrap();
        }
        {
            let entry = idx.partitions.get(&("t".to_string(), 0)).expect("entry kept: docs remain");
            let part = entry.clone();
            drop(entry);
            assert!(part.read().await.writer.is_none(), "idle writer must be released");
        }

        // Still searchable without a writer.
        let hits = idx.search_partition("t", 0, "hello", 10).await.unwrap();
        assert_eq!(hits.len(), 1);

        // A new write recreates the writer and works end-to-end.
        idx.add_batch("t", 0, &[make_doc(1, "hello again")]).await.unwrap();
        idx.commit("t", 0).await.unwrap();
        let hits = idx.search_partition("t", 0, "hello", 10).await.unwrap();
        assert_eq!(hits.len(), 2);
    }

    /// Regression (per-topic memory OOM, burst): a bulk topic-creation burst
    /// must not accumulate unbounded live writers â past the burst cap,
    /// add_batch commits + releases eagerly. Docs must remain searchable.
    #[tokio::test]
    async fn bulk_burst_caps_live_writers() {
        let idx = HotTextIndex::new(HotTextConfig::default());
        const N: i32 = 90; // > default cap of 64
        for p in 0..N {
            idx.add_batch("burst", p, &[make_doc(0, &format!("burst doc {}", p))])
                .await
                .unwrap();
        }
        let live = idx.live_writers.load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            live <= hot_writer_burst_cap() + 1,
            "live hot writers must be capped during bursts, got {}",
            live
        );
        // Eagerly released partitions are already visible; the rest need a commit.
        for p in 0..N {
            idx.commit("burst", p).await.unwrap();
        }
        let hits = idx.search_topic("burst", "burst", N as usize * 2).await.unwrap();
        assert_eq!(hits.len(), N as usize, "all burst docs must stay searchable");
    }

    /// Regression (per-topic memory OOM): once a partition is empty (all docs
    /// evicted after cold flush) and idle, its entire entry â in-RAM index,
    /// reader, map slot â must be reaped, not held forever.
    #[tokio::test]
    async fn empty_idle_partition_is_reaped() {
        let idx = HotTextIndex::new(HotTextConfig::default());
        idx.add_batch("t", 0, &[make_doc(0, "ephemeral doc")]).await.unwrap();
        idx.commit("t", 0).await.unwrap();

        // Cold flush covered offset 0: evict everything.
        idx.evict_up_to("t", 0, 0).await.unwrap();
        assert_eq!(idx.doc_count("t", 0).await, 0);

        // Idle ticks after the eviction commit â entry reaped.
        for _ in 0..=HotTextIndex::WRITER_IDLE_RELEASE_TICKS {
            idx.commit("t", 0).await.unwrap();
        }
        assert!(
            idx.partitions.get(&("t".to_string(), 0)).is_none(),
            "empty idle partition must be reaped from the map"
        );

        // And a later produce transparently recreates it.
        idx.add_batch("t", 0, &[make_doc(5, "reborn")]).await.unwrap();
        idx.commit("t", 0).await.unwrap();
        let hits = idx.search_partition("t", 0, "reborn", 10).await.unwrap();
        assert_eq!(hits.len(), 1);
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
        // Every 7th doc (0, 7, 14, ..., 994) â 143 matches; top_k caps at 20.
        assert_eq!(hits.len(), 20);
        for h in &hits {
            assert!(h.offset % 7 == 0, "fox only in multiples-of-7 docs");
        }
    }

    /// The throttle's admission: a batch is admitted only if it fits with
    /// everything pending, in one step, so concurrent requests cannot all
    /// pass; a lone oversized batch is still admitted; releasing frees room.
    #[tokio::test]
    async fn try_reserve_admits_only_what_fits() {
        let idx = HotTextIndex::new(HotTextConfig::default());
        assert!(idx.try_reserve("t", 0, 600, 1_000));
        assert!(!idx.try_reserve("t", 0, 600, 1_000), "two together exceed the limit");
        assert!(idx.try_reserve("t", 0, 400, 1_000));
        assert_eq!(idx.pending_docs("t", 0), 1_000);
        idx.release("t", 0, 600);
        assert!(idx.try_reserve("t", 0, 600, 1_000), "room again once released");
        assert!(idx.try_reserve("t", 1, 5_000, 1_000), "nothing pending: an oversized batch is admitted");
    }

    /// What a throttled topic's produce compares with its limit: records on
    /// their way to the hot index count before they arrive, and once added
    /// they count as held, never neither in between.
    #[tokio::test]
    async fn pending_docs_counts_queued_and_held_records() {
        let idx = HotTextIndex::new(HotTextConfig::default());
        assert_eq!(idx.pending_docs("t", 0), 0);
        let docs: Vec<HotDoc> = (0..100).map(|i| make_doc(i, "payload")).collect();

        idx.note_queued("t", 0, docs.len());
        assert_eq!(idx.pending_docs("t", 0), 100, "queued records count before they are added");
        idx.add_batch("t", 0, &docs).await.unwrap();
        idx.note_added("t", 0, docs.len());
        assert_eq!(idx.pending_docs("t", 0), 100, "added, not yet committed: still counted");
        idx.commit("t", 0).await.unwrap();
        assert_eq!(idx.pending_docs("t", 0), 100);
        assert_eq!(idx.pending_docs("t", 1), 0, "per partition");
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
        // Same offset on different partitions â both should appear.
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

    /// A partition indexing many records between commits gets a burst writer,
    /// at most `max_burst_writers` across partitions, released once idle;
    /// nothing is lost when the writer is swapped.
    #[tokio::test]
    async fn bursting_partitions_get_a_large_writer_within_the_cap() {
        let idx = HotTextIndex::new(HotTextConfig {
            burst_writer_heap_bytes: 64_000_000,
            burst_writer_threads: 2,
            max_burst_writers: 1,
            burst_threshold_docs: 50,
            ..HotTextConfig::default()
        });
        let burst = |p: i32| {
            let idx = &idx;
            async move {
                let docs: Vec<HotDoc> = (0..100).map(|i| make_doc(i, "payload")).collect();
                idx.add_batch("t", p, &docs).await.unwrap();
                idx.commit("t", p).await.unwrap();
            }
        };
        let live = |idx: &HotTextIndex| idx.burst_writers.load(std::sync::atomic::Ordering::Relaxed);
        burst(0).await;
        assert_eq!(live(&idx), 1, "partition 0 bursts");
        burst(1).await;
        assert_eq!(live(&idx), 1, "the cap holds: partition 1 stays small");
        // Records added after the swap are indexed by the burst writer.
        let more: Vec<HotDoc> = (100..150).map(|i| make_doc(i, "payload")).collect();
        idx.add_batch("t", 0, &more).await.unwrap();
        idx.commit("t", 0).await.unwrap();
        assert_eq!(idx.search_partition("t", 0, "payload", 1000).await.unwrap().len(), 150);
        assert_eq!(idx.search_partition("t", 1, "payload", 1000).await.unwrap().len(), 100);
        // Idle: the writer goes, and the burst slot with it.
        for _ in 0..3 {
            idx.commit("t", 0).await.unwrap();
        }
        assert_eq!(live(&idx), 0);
        burst(1).await;
        assert_eq!(live(&idx), 1, "the freed slot is reused");
    }

    fn capped(max: usize, hard: usize) -> HotTextIndex {
        HotTextIndex::new(HotTextConfig {
            writer_heap_bytes: 15_000_000,
            max_docs_per_partition: max,
            hard_max_docs_per_partition: hard,
            ..HotTextConfig::default()
        })
    }

    async fn offsets(idx: &HotTextIndex) -> Vec<i64> {
        let mut o: Vec<i64> = idx
            .search_partition("t", 0, "payload", 100_000)
            .await
            .unwrap()
            .iter()
            .map(|h| h.offset)
            .collect();
        o.sort_unstable();
        o
    }

    /// Over the soft cap, nothing no archive covers is trimmed: before any
    /// archive the hot index is the only place those records can be found.
    #[tokio::test]
    async fn commit_keeps_unarchived_docs_over_the_soft_cap() {
        let idx = capped(100, 10_000);
        let docs: Vec<HotDoc> = (0..500).map(|i| make_doc(i, "payload")).collect();
        idx.add_batch("t", 0, &docs).await.unwrap();
        idx.commit("t", 0).await.unwrap();
        assert_eq!(offsets(&idx).await, (0..500).collect::<Vec<_>>());
    }

    /// Once an archive covers the oldest docs, the trim drops those outside
    /// the newest `max_docs_per_partition` offsets, and nothing above the
    /// archived watermark.
    #[tokio::test]
    async fn commit_trims_only_archived_offsets() {
        let idx = capped(100, 10_000);
        let docs: Vec<HotDoc> = (0..500).map(|i| make_doc(i, "payload")).collect();
        idx.add_batch("t", 0, &docs).await.unwrap();
        idx.commit("t", 0).await.unwrap();
        idx.note_archived("t", 0, 299);
        idx.add_batch("t", 0, &[make_doc(500, "payload")]).await.unwrap();
        idx.commit("t", 0).await.unwrap();
        assert_eq!(offsets(&idx).await, (300..=500).collect::<Vec<_>>());

        // Archived past the window: the window bounds what stays.
        idx.note_archived("t", 0, 450);
        idx.add_batch("t", 0, &[make_doc(501, "payload")]).await.unwrap();
        idx.commit("t", 0).await.unwrap();
        assert_eq!(offsets(&idx).await, (402..=501).collect::<Vec<_>>());
    }

    /// The hard cap bounds memory even when nothing is archived, and reports
    /// how many records it dropped that no archive held.
    #[tokio::test]
    async fn hard_cap_bounds_memory_and_counts_unarchived() {
        let idx = capped(50, 200);
        let docs: Vec<HotDoc> = (0..500).map(|i| make_doc(i, "payload")).collect();
        idx.add_batch("t", 0, &docs).await.unwrap();
        let part = idx.get_or_create("t", 0).unwrap();
        let mut guard = part.write().await;
        guard.make_visible().unwrap();
        let (evicted, unarchived) = guard.trim_to_capacity(None).unwrap();
        assert_eq!((evicted, unarchived), (300, 300));
        assert_eq!(guard.doc_count, 200);
        assert_eq!(guard.min_offset, Some(300));

        // Partly archived: only the part above the watermark counts as lost.
        let more: Vec<HotDoc> = (500..800).map(|i| make_doc(i, "payload")).collect();
        guard.add_batch(&more).unwrap();
        guard.make_visible().unwrap();
        let (evicted, unarchived) = guard.trim_to_capacity(Some(399)).unwrap();
        assert_eq!((evicted, unarchived), (300, 200));
        assert_eq!(guard.min_offset, Some(600));
    }

    /// Steady trimming leaves deleted docs behind; they are merged away
    /// instead of the partition being dropped (which lost the newest records
    /// too), and every live record stays searchable.
    #[tokio::test]
    async fn steady_trimming_purges_deleted_docs_and_keeps_newest() {
        let idx = capped(50, 10_000);
        for round in 0..20i64 {
            let base = round * 100;
            let docs: Vec<HotDoc> = (base..base + 100).map(|o| make_doc(o, "payload")).collect();
            idx.add_batch("t", 0, &docs).await.unwrap();
            idx.note_archived("t", 0, base + 49);
            idx.commit("t", 0).await.unwrap();
            assert_eq!(
                offsets(&idx).await,
                (base + 50..base + 100).collect::<Vec<_>>(),
                "round {round}"
            );
        }
        let part = idx.get_or_create("t", 0).unwrap();
        let guard = part.read().await;
        let total = guard.total_including_deleted();
        assert!(total <= 300, "deleted docs never reclaimed: {total} held for {} live", guard.doc_count);
    }

    /// Records arriving after the archive already covers them (produce hands
    /// them over asynchronously) keep the watermark, so they are trimmed like
    /// any archived record. Forgetting it on such a record, as add_batch used
    /// to, left archived records untrimmable under load until the hard cap
    /// evicted them as "unarchived".
    #[tokio::test]
    async fn late_records_keep_the_archived_watermark() {
        let idx = capped(10, 10_000);
        idx.note_archived("t", 0, 1_000);
        let docs: Vec<HotDoc> = (0..100).map(|i| make_doc(i, "payload")).collect();
        idx.add_batch("t", 0, &docs).await.unwrap();
        assert_eq!(idx.archived_through("t", 0), Some(1_000));
        idx.commit("t", 0).await.unwrap();
        assert_eq!(offsets(&idx).await.len(), 10, "archived records trimmed to the soft cap");
    }

    /// A deleted topic's watermark goes with it: the records of a topic
    /// recreated under the name are never trimmed as archived.
    #[tokio::test]
    async fn a_deleted_topic_forgets_its_archived_watermark() {
        let idx = capped(10, 10_000);
        idx.note_archived("t", 0, 1_000);
        idx.forget_topic("t");
        assert_eq!(idx.archived_through("t", 0), None);
        let docs: Vec<HotDoc> = (0..100).map(|i| make_doc(i, "payload")).collect();
        idx.add_batch("t", 0, &docs).await.unwrap();
        idx.commit("t", 0).await.unwrap();
        assert_eq!(offsets(&idx).await.len(), 100, "no archive covers the new topic's records");
    }

    /// A cut log: the docs at and above the cut go, the watermark drops below
    /// it, and records written after the cut (the same offsets, new records)
    /// are kept until archived.
    #[tokio::test]
    async fn truncation_drops_the_cut_and_lowers_the_watermark() {
        let idx = capped(10, 10_000);
        let docs: Vec<HotDoc> = (0..100).map(|i| make_doc(i, "payload old")).collect();
        idx.add_batch("t", 0, &docs).await.unwrap();
        idx.commit("t", 0).await.unwrap();
        idx.note_archived("t", 0, 80);

        assert_eq!(idx.truncate("t", 0, 50).await.unwrap(), 50);
        assert_eq!(idx.archived_through("t", 0), Some(49));
        assert!(offsets(&idx).await.iter().all(|&o| o < 50));

        let docs: Vec<HotDoc> = (50..100).map(|i| make_doc(i, "payload new")).collect();
        idx.add_batch("t", 0, &docs).await.unwrap();
        idx.commit("t", 0).await.unwrap();
        let held = offsets(&idx).await;
        assert!((50..100).all(|o| held.contains(&o)), "records after the cut are kept: {held:?}");
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

        // Eviction is spawned â wait for it to land.
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

    /// Issue #2 regression: `bool.must` ANDs its clauses. Previously the hot
    /// path flattened must clauses into a space-joined OR query, so two
    /// disjoint must clauses â one matching the only doc, one matching zero â
    /// returned 1 hit instead of 0.
    #[tokio::test]
    async fn structured_bool_must_ands_its_clauses() {
        use tantivy::query::{BooleanQuery, Occur, Query, QueryParser};

        let idx = HotTextIndex::new(HotTextConfig::default());
        // Produce one doc: value contains "foo", key is "alpha"
        idx.add_batch(
            "t",
            0,
            &[HotDoc {
                offset: 0,
                timestamp: 1,
                key: Some("alpha".into()),
                value: "has foo content".into(),
                headers: None,
            }],
        )
        .await
        .unwrap();
        idx.commit("t", 0).await.unwrap();

        // Sanity: value=foo alone matches the doc.
        let sanity = idx
            .search_topic_structured(
                "t",
                |_schema, index| {
                    let parser = QueryParser::for_index(
                        index,
                        vec![index.schema().get_field("value").unwrap()],
                    );
                    Ok(parser.parse_query("foo").unwrap())
                },
                10,
            )
            .await
            .unwrap();
        assert_eq!(sanity.len(), 1, "value=foo alone should hit");

        // Structured bool.must: (value=foo AND key=beta). Second clause has
        // no match, so the whole thing must return 0.
        let and_miss: Vec<_> = idx
            .search_topic_structured(
                "t",
                |_schema, index| {
                    let schema = index.schema();
                    let value_f = schema.get_field("value").unwrap();
                    let key_f = schema.get_field("key").unwrap();
                    let value_parser = QueryParser::for_index(index, vec![value_f]);
                    let key_parser = QueryParser::for_index(index, vec![key_f]);
                    let clauses: Vec<(Occur, Box<dyn Query>)> = vec![
                        (Occur::Must, value_parser.parse_query("foo").unwrap()),
                        (Occur::Must, key_parser.parse_query("beta").unwrap()),
                    ];
                    Ok(Box::new(BooleanQuery::new(clauses)) as Box<dyn Query>)
                },
                10,
            )
            .await
            .unwrap();
        assert_eq!(
            and_miss.len(),
            0,
            "bool.must[value=foo, key=beta] must AND to 0 â the fix for GitHub #2"
        );

        // Positive: (value=foo AND key=alpha) â 1 hit.
        let and_hit: Vec<_> = idx
            .search_topic_structured(
                "t",
                |_schema, index| {
                    let schema = index.schema();
                    let value_f = schema.get_field("value").unwrap();
                    let key_f = schema.get_field("key").unwrap();
                    let vp = QueryParser::for_index(index, vec![value_f]);
                    let kp = QueryParser::for_index(index, vec![key_f]);
                    let clauses: Vec<(Occur, Box<dyn Query>)> = vec![
                        (Occur::Must, vp.parse_query("foo").unwrap()),
                        (Occur::Must, kp.parse_query("alpha").unwrap()),
                    ];
                    Ok(Box::new(BooleanQuery::new(clauses)) as Box<dyn Query>)
                },
                10,
            )
            .await
            .unwrap();
        assert_eq!(and_hit.len(), 1, "bool.must AND matching both should hit");
    }
}
