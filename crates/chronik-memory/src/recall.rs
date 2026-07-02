//! Recall — multi-channel hybrid retrieval against the Chronik unified API.
//!
//! Phase 2 (AMS-2.5) ships all five channels:
//!
//! | Channel | Source | Notes |
//! |---|---|---|
//! | [`Channel::Bm25`] | `POST /_search` | text relevance via Tantivy. Always available. |
//! | [`Channel::Vector`] | `POST /_vector/{topic}/search` | semantic similarity. Embedding done server-side; no client `Embedder` needed. |
//! | [`Channel::KeyMatch`] | `POST /_search` with subject-extracted phrase | rule-based subject extraction (`user`, `user:luis`, `agent:bot`). |
//! | [`Channel::Hyde`] | [`TextGenerator`] → `POST /_vector/.../search` | hypothetical-document embedding; opt-in via `with_text_generator`. |
//! | [`Channel::Sql`] | `POST /_sql` | structured filter; opt-in via `with_sql_filter`. Gracefully degrades if `/_sql/tables` is empty. |
//!
//! Channels run in parallel via `futures::future::try_join_all`. Results are
//! fused with type-weighted RRF: each channel contributes
//! `1 / (k + rank) * channel_weight * type_weight` to a memory's final score,
//! multiplied by [`crate::ranking::decay_factor`] and the memory's `confidence`.
//!
//! # Example
//!
//! ```no_run
//! # use chronik_memory::{Memory, MemoryType, Channel};
//! # async fn run(mem: Memory) -> chronik_memory::Result<()> {
//! let results = mem.recall("what neighborhoods does this user prefer?")
//!     .types(&[MemoryType::Fact])
//!     .channels(&[Channel::Bm25, Channel::Vector, Channel::KeyMatch])
//!     .k(10)
//!     .send()
//!     .await?;
//! # Ok(())
//! # }
//! ```

use crate::client::Memory;
use crate::embeddings::TextGenerator;
use crate::error::{MemoryError, Result};
use crate::ranking::{
    decay_factor, detect_intent, half_life, intent_boost, rrf_contribution, type_weight,
    Channel, QueryIntent, RRF_K,
};
use crate::schema::{MemoryRecord, MemoryType};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;

/// Default size of the fan-out window per channel before RRF.
const DEFAULT_FANOUT_SIZE: usize = 50;

/// One ranked result from [`Memory::recall`].
#[derive(Debug, Clone)]
pub struct RecallResult {
    /// Final fused score (RRF × decay × confidence × type_weight). Higher is better.
    pub score: f64,
    /// The memory itself.
    pub memory: MemoryRecord,
    /// Per-channel raw RRF contributions for explainability / debugging.
    pub channels: HashMap<Channel, f64>,
}

/// Output of [`RecallBuilder::synthesize`] — the LLM's synthesized answer plus
/// the supporting memories that fed the prompt.
///
/// `abstained = true` when the model returned the canonical "I don't know"
/// abstention string (caller should treat that as a confident no-answer rather
/// than an arbitrary one-line answer the way it would treat a normal
/// completion). The supporting memories are still returned so the caller can
/// fall back to surfacing them, log the inputs, or build a UI that shows
/// "we know X, Y, Z but not the answer".
#[derive(Debug, Clone)]
pub struct SynthesizedAnswer {
    /// The model's free-form answer, or [`ABSTAIN_LITERAL`] when the model
    /// declined to answer (in which case `abstained` is `true`).
    pub answer: String,
    /// `true` iff the model returned the abstention literal.
    pub abstained: bool,
    /// The memories that fed the prompt, in the order they were ranked.
    pub supporting: Vec<RecallResult>,
}

/// Canonical abstention string we instruct the synthesis LLM to emit when the
/// supplied memories don't answer the question. Public so callers (e.g. UI
/// layers, eval harnesses) can compare against it directly.
pub const ABSTAIN_LITERAL: &str = "I don't know.";

/// Typed structured filter for the [`Channel::Sql`] channel.
///
/// Each predicate maps to a SQL `WHERE` clause fragment. The Chronik `/_sql`
/// endpoint exposes the typed-memory topic as a DataFusion table whose columns
/// mirror the envelope (`namespace`, `tenant_id`, `confidence`, `body.*`, etc).
#[derive(Debug, Clone, Default)]
pub struct SqlFilter {
    /// `WHERE valid_from >= ?` (RFC-3339).
    pub since: Option<DateTime<Utc>>,
    /// `WHERE valid_from <= ?` (RFC-3339).
    pub before: Option<DateTime<Utc>>,
    /// `WHERE confidence >= ?`.
    pub min_confidence: Option<f32>,
    /// Free-form additional WHERE clause appended verbatim — caller's
    /// responsibility to escape values. Use only for trusted application code.
    pub extra_where: Option<String>,
}

/// Typed builder produced by [`Memory::recall`].
#[derive(Clone)]
pub struct RecallBuilder<'a> {
    memory: &'a Memory,
    query: String,
    types: Vec<MemoryType>,
    k: usize,
    channels: Vec<Channel>,
    min_confidence: Option<f32>,
    since: Option<DateTime<Utc>>,
    fanout_size: usize,
    text_generator: Option<Arc<dyn TextGenerator>>,
    sql_filter: Option<SqlFilter>,
    /// AMS-3.7 path B: when `true`, fetch the single best-matching concept
    /// page from `mem.concept.{tenant}` and inline it into the result set
    /// before the atomic memories. Cheap (one extra `/_search` against the
    /// compacted concept topic) and the concept page acts as a
    /// pre-aggregated synthesis that `synthesize()` can reference at fusion
    /// time. Default: off — opt-in via `include_concepts(true)`.
    include_concepts: bool,
}

impl<'a> std::fmt::Debug for RecallBuilder<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecallBuilder")
            .field("query", &self.query)
            .field("types", &self.types)
            .field("k", &self.k)
            .field("channels", &self.channels)
            .field("has_text_generator", &self.text_generator.is_some())
            .field("has_sql_filter", &self.sql_filter.is_some())
            .field("include_concepts", &self.include_concepts)
            .finish()
    }
}

impl<'a> RecallBuilder<'a> {
    pub(crate) fn new(memory: &'a Memory, query: impl Into<String>) -> Self {
        Self {
            memory,
            query: query.into(),
            types: vec![MemoryType::Fact, MemoryType::Event],
            k: 10,
            channels: vec![Channel::Bm25],
            min_confidence: None,
            since: None,
            fanout_size: DEFAULT_FANOUT_SIZE,
            text_generator: None,
            sql_filter: None,
            include_concepts: false,
        }
    }

    /// Restrict results to the given memory types. Default: Fact + Event.
    pub fn types(mut self, types: &[MemoryType]) -> Self {
        self.types = types.to_vec();
        self
    }

    /// Number of results to return after RRF fusion. Default: 10.
    pub fn k(mut self, k: usize) -> Self {
        self.k = k;
        self
    }

    /// Explicit channel set. Default: `[Bm25]`. Builder convenience methods
    /// (`with_vector`, `with_key_match`, `with_text_generator`, `with_sql_filter`)
    /// also enable channels — calling [`channels`](Self::channels) overrides
    /// any of those.
    pub fn channels(mut self, channels: &[Channel]) -> Self {
        self.channels = channels.to_vec();
        self
    }

    /// Enable the [`Channel::Vector`] channel. Vector search uses Chronik's
    /// server-side embedding (the topic must be configured with
    /// `vector.enabled=true`); no client-side `Embedder` is required.
    pub fn with_vector(mut self) -> Self {
        if !self.channels.contains(&Channel::Vector) {
            self.channels.push(Channel::Vector);
        }
        self
    }

    /// Enable the [`Channel::KeyMatch`] channel — rule-based subject
    /// extraction from the query, then a phrase-boosted `/_search`.
    pub fn with_key_match(mut self) -> Self {
        if !self.channels.contains(&Channel::KeyMatch) {
            self.channels.push(Channel::KeyMatch);
        }
        self
    }

    /// Provide a [`TextGenerator`] and enable the [`Channel::Hyde`] channel.
    /// Generates a hypothetical answer to the query, then runs vector search
    /// against the hypothetical's text.
    pub fn with_text_generator(mut self, gen: Arc<dyn TextGenerator>) -> Self {
        self.text_generator = Some(gen);
        if !self.channels.contains(&Channel::Hyde) {
            self.channels.push(Channel::Hyde);
        }
        self
    }

    /// Provide a structured [`SqlFilter`] and enable the [`Channel::Sql`]
    /// channel. Gracefully degrades to a no-op if Chronik's `/_sql` surface
    /// reports no tables for this topic.
    pub fn with_sql_filter(mut self, filter: SqlFilter) -> Self {
        self.sql_filter = Some(filter);
        if !self.channels.contains(&Channel::Sql) {
            self.channels.push(Channel::Sql);
        }
        self
    }

    /// Drop memories with confidence below this threshold.
    pub fn min_confidence(mut self, c: f32) -> Self {
        self.min_confidence = Some(c);
        self
    }

    /// Drop memories whose `valid_from` is older than this timestamp.
    pub fn since(mut self, since: DateTime<Utc>) -> Self {
        self.since = Some(since);
        self
    }

    /// Override the per-channel fan-out window (default 50).
    pub fn fanout_size(mut self, n: usize) -> Self {
        self.fanout_size = n;
        self
    }

    /// Inline the single best-matching concept page (`mem.concept.{tenant}`)
    /// at the top of the result list before atomic memories.
    ///
    /// Concept pages (AMS-3.7) are pre-aggregated synthesis pages keyed by
    /// `concept:{entity_id}`, written by either
    /// [`crate::Memory::remember_concept`] directly or the concept-page
    /// worker. Inlining one above atomic memories gives downstream consumers
    /// (UI, prompt, [`RecallBuilder::synthesize`]) a denser starting point —
    /// the concept page already fuses what would otherwise need to be
    /// reconstructed from many atomic memories at every recall.
    ///
    /// Cheap: one extra `/_search` against the compacted concept topic in
    /// parallel with the regular fan-out. Returns nothing (no boost) if no
    /// concept page exists for the tenant — graceful no-op.
    ///
    /// Default: `false` (opt-in). Default may flip to `true` once the
    /// concept worker is shipping per-tenant pages reliably.
    pub fn include_concepts(mut self, enabled: bool) -> Self {
        self.include_concepts = enabled;
        self
    }

    /// Execute the recall.
    #[tracing::instrument(skip(self), fields(query = %self.query, k = self.k, n_types = self.types.len(), n_channels = self.channels.len()))]
    pub async fn send(self) -> Result<Vec<RecallResult>> {
        if self.query.is_empty() {
            return Err(MemoryError::InvalidArgument("recall query is empty".into()));
        }
        if self.types.is_empty() {
            return Err(MemoryError::InvalidArgument(
                "recall requires at least one memory type".into(),
            ));
        }
        if self.channels.is_empty() {
            return Err(MemoryError::InvalidArgument(
                "recall requires at least one channel".into(),
            ));
        }

        let now = Utc::now();
        let layout = self.memory.topic_layout();
        let api = self.memory.chronik_api().trim_end_matches('/').to_string();
        let http = self.memory.http().clone();
        let namespace = self.memory.namespace().to_string();

        // Fan out: each enabled channel runs in parallel and returns its
        // ranked candidates as either FullCandidates ([`EnrichedRecord`] —
        // envelope + the typed `(topic, partition, offset)` it lives at) or
        // OffsetCandidates (vector/HyDE — id-only `(topic, partition, offset)`,
        // used to boost existing FullCandidate scores).
        let mut full_per_channel: HashMap<Channel, Vec<EnrichedRecord>> = HashMap::new();
        let mut id_per_channel: HashMap<Channel, Vec<TypedLoc>> = HashMap::new();

        let mut bm25_fut = None;
        let mut vector_fut = None;
        let mut keymatch_fut = None;
        let mut hyde_fut = None;
        let mut sql_fut = None;

        for ch in &self.channels {
            match ch {
                Channel::Bm25 => {
                    bm25_fut = Some(run_bm25(
                        http.clone(),
                        api.clone(),
                        layout,
                        &namespace,
                        &self.types,
                        &self.query,
                        self.fanout_size,
                    ));
                }
                Channel::Vector => {
                    vector_fut = Some(run_vector(
                        http.clone(),
                        api.clone(),
                        layout,
                        &self.types,
                        self.query.clone(),
                        self.fanout_size,
                    ));
                }
                Channel::KeyMatch => {
                    keymatch_fut = Some(run_key_match(
                        http.clone(),
                        api.clone(),
                        layout,
                        &namespace,
                        &self.types,
                        &self.query,
                        self.fanout_size,
                    ));
                }
                Channel::Hyde => {
                    if let Some(gen) = self.text_generator.clone() {
                        hyde_fut = Some(run_hyde(
                            http.clone(),
                            api.clone(),
                            layout,
                            &self.types,
                            self.query.clone(),
                            gen,
                            self.fanout_size,
                        ));
                    } else {
                        return Err(MemoryError::Config(
                            "Channel::Hyde requires .with_text_generator(...) on the builder".into(),
                        ));
                    }
                }
                Channel::Sql => {
                    if let Some(filter) = self.sql_filter.clone() {
                        sql_fut = Some(run_sql(
                            http.clone(),
                            api.clone(),
                            layout,
                            &namespace,
                            &self.types,
                            filter,
                            self.fanout_size,
                        ));
                    } else {
                        return Err(MemoryError::Config(
                            "Channel::Sql requires .with_sql_filter(...) on the builder".into(),
                        ));
                    }
                }
            }
        }

        // Run all enabled channels concurrently. Each future resolves to its
        // own typed list — we collect via Tokio's join! after Optionifying.
        let (b, v, km, h, s) = tokio::join!(
            opt_fut(bm25_fut),
            opt_fut(vector_fut),
            opt_fut(keymatch_fut),
            opt_fut(hyde_fut),
            opt_fut(sql_fut),
        );

        if let Some(out) = b? {
            full_per_channel.insert(Channel::Bm25, out);
        }
        if let Some(out) = v? {
            id_per_channel.insert(Channel::Vector, out);
        }
        if let Some(out) = km? {
            full_per_channel.insert(Channel::KeyMatch, out);
        }
        if let Some(out) = h? {
            id_per_channel.insert(Channel::Hyde, out);
        }
        if let Some(out) = s? {
            full_per_channel.insert(Channel::Sql, out);
        }

        // Build master result map keyed by memory_id. Channels that produce
        // full envelopes (BM25, KeyMatch, Sql) seed the map; id-only channels
        // (Vector, HyDE) boost RRF scores for already-present entries.
        //
        // Also maintain `loc_to_id` so id-only channels can resolve a typed
        // `(topic, partition, offset)` back to a `memory_id` in O(1).
        let mut master: HashMap<String, RecallResult> = HashMap::new();
        let mut loc_to_id: HashMap<TypedLoc, String> = HashMap::new();
        for (ch, recs) in &full_per_channel {
            for (rank, enriched) in recs.iter().enumerate() {
                let m = &enriched.record;
                // Note: `passes_filters` deliberately *does not* drop
                // tombstoned records here. They have to reach
                // `dedup_results_keep_max_score` so that a tombstone with
                // version=u64::MAX wins the (namespace, key) bucket and
                // supersedes the original; only after dedup do we drop the
                // tombstone via the post-dedup filter below. Dropping
                // tombstones here would let the original record bypass
                // dedup and still surface — exactly the bug
                // `cluster_smoke.rs` catches.
                if !self.passes_pre_dedup_filters(m) {
                    continue;
                }
                let entry = master.entry(m.memory_id.clone()).or_insert_with(|| {
                    RecallResult {
                        score: 0.0,
                        memory: m.clone(),
                        channels: HashMap::new(),
                    }
                });
                let contribution = rrf_contribution(rank, RRF_K) * ch.default_weight();
                *entry.channels.entry(*ch).or_insert(0.0) += contribution;
                if let Some(loc) = &enriched.typed_loc {
                    loc_to_id
                        .entry(loc.clone())
                        .or_insert_with(|| m.memory_id.clone());
                }
            }
        }

        // AMS-3.7 path B: when `include_concepts` is on, fetch the single
        // best-matching concept page from `mem.concept.{tenant}` and seed
        // it into master with a high RRF contribution (rank 0, BM25
        // channel weight). The concept page is a pre-aggregated synthesis
        // — its `markdown` body already fuses many atomic facts, so it
        // deserves to surface above raw memories. Graceful no-op if the
        // concept topic is empty / 404 / un-indexed. The fetch is
        // sequential here (after the channel join) for code simplicity;
        // the cost is one extra round-trip on top of the channel fan-out.
        if self.include_concepts {
            let concept_topic = layout.concept();
            let url = format!("{api}/_search");
            let body = bm25_query_body(&concept_topic, &namespace, &self.query, 1);
            if let Ok(records) = post_search(http.clone(), url, body).await {
                for (rank, enriched) in records.iter().enumerate() {
                    let m = &enriched.record;
                    if !self.passes_pre_dedup_filters(m) {
                        continue;
                    }
                    let entry = master.entry(m.memory_id.clone()).or_insert_with(|| {
                        RecallResult {
                            score: 0.0,
                            memory: m.clone(),
                            channels: HashMap::new(),
                        }
                    });
                    // Concept pages join via the BM25 channel slot — same
                    // weighting as a standard BM25 hit. The concept's own
                    // `type_weight` (1.5 for Concept) handles the rollup
                    // boost in the final score multiplication below.
                    let contribution =
                        rrf_contribution(rank, RRF_K) * Channel::Bm25.default_weight();
                    *entry.channels.entry(Channel::Bm25).or_insert(0.0) += contribution;
                }
            }
        }

        // Apply id-only channel boosts: a vector/HyDE hit at typed
        // `(topic, partition, offset)` boosts whichever master entry was
        // produced from that exact location. Any vector/HyDE hit that doesn't
        // match a full-envelope hit is skipped (we can't surface it without
        // the envelope).
        for (ch, hits) in &id_per_channel {
            for (rank, loc) in hits.iter().enumerate() {
                let contribution = rrf_contribution(rank, RRF_K) * ch.default_weight();
                if let Some(mem_id) = loc_to_id.get(loc) {
                    if let Some(result) = master.get_mut(mem_id) {
                        *result.channels.entry(*ch).or_insert(0.0) += contribution;
                    }
                }
            }
        }

        // Detect the query's intent once — used to boost value-bearing
        // memories on numeric / temporal queries (LongMemEval levers #2 + #3).
        let intent = detect_intent(&self.query);

        // Final score = sum of channel contributions × type_weight × decay × confidence × intent_boost.
        // AM-2.3: consult MemConfig for per-tenant half-life overrides before
        // falling back to the ranking constants.
        let mem_config = self.memory.mem_config().cloned();
        for result in master.values_mut() {
            let ty = result.memory.body.kind();
            let hl = mem_config
                .as_ref()
                .map(|c| c.half_life(&result.memory.tenant_id, ty))
                .unwrap_or_else(|| half_life(ty));
            let decay = decay_factor(result.memory.valid_from, now, hl);
            let conf = result.memory.confidence as f64;
            let type_w = type_weight(ty);
            let boost = intent_boost(intent, &result.memory);
            let channel_sum: f64 = result.channels.values().sum();
            result.score = channel_sum * type_w * decay * conf * boost;
        }
        if !matches!(intent, QueryIntent::Identity) {
            tracing::debug!(
                query = %self.query,
                intent = ?intent,
                "applied question-aware intent boost"
            );
        }

        // Compaction-style dedup on (namespace, key) keeping highest version,
        // tie-broken by score. Tombstones reach this step (with
        // version=u64::MAX) so they win their (namespace, key) bucket and
        // supersede the original.
        let mut out: Vec<RecallResult> = master.into_values().collect();
        out = dedup_results_keep_max_score(out);
        // Drop the tombstones now that they've done their dedup job. Net
        // result for a forgotten key: zero rows.
        out.retain(|r| !r.memory.tombstoned);
        out.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        out.truncate(self.k);
        Ok(out)
    }

    /// Recall the top-`k` memories AND ask `generator` to synthesize an
    /// answer to the original query against them.
    ///
    /// This is the on-demand half of AMS-3.7 concept-pages — instead of
    /// pre-synthesizing entity rollup pages in a worker, the recall path
    /// asks the model to fuse atomic memories at query time. It directly
    /// attacks the LongMemEval gap on multi-fact arithmetic
    /// ("what's the user's max budget?" with multiple budget statements
    /// over time), temporal reasoning, and abstention.
    ///
    /// Behaviour:
    /// - Runs the configured channel fan-out exactly the way `send()` does.
    /// - If the recall returns zero memories, short-circuits to
    ///   [`SynthesizedAnswer`] with `abstained = true` (no LLM call).
    /// - Otherwise builds a structured prompt — typed fields per memory,
    ///   timestamps, channel-relevance order — and asks the generator to
    ///   answer using ONLY the supplied memories, or emit the abstention
    ///   literal.
    /// - Returns the answer alongside the supporting memories so the caller
    ///   can show provenance / fall back to listing memories on abstention.
    ///
    /// **Cost**: one LLM call per synthesize invocation (in addition to the
    /// recall fan-out). Token budget: the prompt caps each memory at
    /// `MAX_SYNTH_SNIPPET_CHARS` so a `k=10` recall produces a ~3 KB prompt.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use std::sync::Arc;
    /// # use chronik_memory::{Memory, MemoryType, Channel, embeddings::TextGenerator};
    /// # async fn run(mem: Memory, gen: Arc<dyn TextGenerator>) -> chronik_memory::Result<()> {
    /// let answer = mem.recall("what's the user's max budget?")
    ///     .types(&[MemoryType::Fact])
    ///     .channels(&[Channel::Bm25, Channel::Vector])
    ///     .k(10)
    ///     .synthesize(gen)
    ///     .await?;
    /// if answer.abstained {
    ///     // Fall back to showing supporting memories or telling the user
    ///     // we don't have the answer.
    /// } else {
    ///     println!("{}", answer.answer);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    #[tracing::instrument(skip(self, generator), fields(query = %self.query, k = self.k))]
    pub async fn synthesize(
        self,
        generator: Arc<dyn TextGenerator>,
    ) -> Result<SynthesizedAnswer> {
        let question = self.query.clone();
        let memories = self.send().await?;
        if memories.is_empty() {
            return Ok(SynthesizedAnswer {
                answer: ABSTAIN_LITERAL.to_string(),
                abstained: true,
                supporting: vec![],
            });
        }
        let prompt = build_synthesis_prompt(&question, &memories);
        let raw = generator.complete(&prompt).await?;
        let trimmed = raw.trim();
        let abstained = is_abstention(trimmed);
        let answer = if abstained {
            ABSTAIN_LITERAL.to_string()
        } else {
            trimmed.to_string()
        };
        Ok(SynthesizedAnswer {
            answer,
            abstained,
            supporting: memories,
        })
    }

    /// Filters that run BEFORE `dedup_results_keep_max_score`. These exclude
    /// records that are categorically wrong for this recall (cross-namespace
    /// leakers, low-confidence, expired). Tombstones are *not* filtered here
    /// — they must reach dedup so they can win the (namespace, key) bucket
    /// and hide the original. The tombstone itself is dropped after dedup.
    fn passes_pre_dedup_filters(&self, m: &MemoryRecord) -> bool {
        // Namespace isolation: typed topics (`mem.fact.{tenant}` etc.) are
        // shared across all conversations in the tenant. The recall builder
        // is constructed against a specific namespace; never surface records
        // from a different namespace. Without this filter, high-cardinality
        // tenants leak conversation-A's records into conversation-B's recall
        // results, which destroys both privacy and ranking quality.
        if m.namespace != self.memory.namespace() {
            return false;
        }
        if let Some(min_c) = self.min_confidence {
            if m.confidence < min_c {
                return false;
            }
        }
        if let Some(since) = self.since {
            if m.valid_from < since {
                return false;
            }
        }
        true
    }
}

async fn opt_fut<T, F: std::future::Future<Output = Result<T>>>(
    f: Option<F>,
) -> Result<Option<T>> {
    match f {
        Some(fut) => fut.await.map(Some),
        None => Ok(None),
    }
}

/// Typed-topic Kafka location: `(topic, partition, offset)`.
///
/// `source` on the envelope itself stores the **raw**-topic location (the
/// `mem.raw.{ns}` topic the extraction was sourced from). To match a vector /
/// HyDE id-only hit (which fires against a **typed** topic) back to a master
/// entry, we have to thread the typed location separately — that's what
/// [`EnrichedRecord::typed_loc`] is for, populated by [`parse_envelope_from_source`]
/// when the wrapped Chronik `_source` shape is in use.
type TypedLoc = (String, i32, i64);

/// A [`MemoryRecord`] together with the typed-topic Kafka location it was
/// fetched from, when known. Channels that hit Chronik's wrapped-shape
/// `_search` always populate `typed_loc`; channels that synthesise records
/// (or hit Chronik via the direct shape — e.g. SQL today) leave it `None`,
/// which simply means id-only channels can't boost those rows.
#[derive(Debug, Clone)]
struct EnrichedRecord {
    record: MemoryRecord,
    typed_loc: Option<TypedLoc>,
}

/// Dedup [`RecallResult`] using compaction semantics: highest version per
/// `(namespace, key)`, tie-broken by score. Events pass through unchanged.
fn dedup_results_keep_max_score(results: Vec<RecallResult>) -> Vec<RecallResult> {
    let mut max_version: HashMap<(String, String), u64> = HashMap::new();
    for r in &results {
        if let Some(k) = &r.memory.key {
            let dedup_key = (r.memory.namespace.clone(), k.clone());
            max_version
                .entry(dedup_key)
                .and_modify(|v| *v = (*v).max(r.memory.version))
                .or_insert(r.memory.version);
        }
    }

    let mut best: HashMap<(String, String), RecallResult> = HashMap::new();
    let mut events: Vec<RecallResult> = Vec::new();
    for r in results {
        match &r.memory.key {
            None => events.push(r),
            Some(k) => {
                let dedup_key = (r.memory.namespace.clone(), k.clone());
                let max_v = max_version.get(&dedup_key).copied().unwrap_or(0);
                if r.memory.version != max_v {
                    continue;
                }
                match best.get(&dedup_key) {
                    Some(existing) if existing.score >= r.score => {}
                    _ => {
                        best.insert(dedup_key, r);
                    }
                }
            }
        }
    }
    let mut out: Vec<RecallResult> = best.into_values().collect();
    out.append(&mut events);
    out
}

// ============================================================================
// Channel implementations
// ============================================================================

/// Build a BM25 query body that biases toward a single namespace.
///
/// Typed topics are tenant-shared (`mem.fact.{tenant}` etc.), so without a
/// namespace bias the BM25 fanout is filled with results from other
/// conversations under the same tenant. We can't use `bool.filter.term` —
/// Chronik 2.5 doesn't index per-field terms, only `_all`-flavoured text —
/// so we instead concatenate the namespace into the `_all` match. The
/// namespace ULID is high-IDF, so concrete records from this namespace
/// dominate the top-`fanout` window. The final
/// [`RecallBuilder::passes_filters`] still drops any cross-namespace
/// leakers for correctness.
fn bm25_query_body(
    topic: &str,
    namespace: &str,
    text: &str,
    fanout: usize,
) -> serde_json::Value {
    let combined = if text.is_empty() {
        namespace.to_string()
    } else {
        format!("{text} {namespace}")
    };
    serde_json::json!({
        "index": topic,
        "size": fanout,
        "query": {"match": {"_all": combined}}
    })
}

async fn run_bm25(
    http: reqwest::Client,
    api: String,
    layout: &crate::topics::TopicLayout,
    namespace: &str,
    types: &[MemoryType],
    query: &str,
    fanout: usize,
) -> Result<Vec<EnrichedRecord>> {
    let mut futures = Vec::with_capacity(types.len());
    for ty in types {
        let topic = layout.typed(*ty);
        let url = format!("{api}/_search");
        let body = bm25_query_body(&topic, namespace, query, fanout);
        futures.push(post_search(http.clone(), url, body));
    }
    let results = futures::future::try_join_all(futures).await?;
    Ok(results.into_iter().flatten().collect())
}

async fn run_key_match(
    http: reqwest::Client,
    api: String,
    layout: &crate::topics::TopicLayout,
    namespace: &str,
    types: &[MemoryType],
    query: &str,
    fanout: usize,
) -> Result<Vec<EnrichedRecord>> {
    // Rule-based subject extraction: any token of the form `word` or
    // `word:word` (lowercase ascii + digits + colons + underscores) that
    // appears in the query AND looks namespace-stylish (i.e. not a stopword).
    let subjects = extract_subject_candidates(query);
    if subjects.is_empty() {
        return Ok(vec![]);
    }
    // Build a phrase-OR query that boosts hits with the subject phrase.
    let phrases: Vec<&str> = subjects.iter().map(|s| s.as_str()).collect();
    let joined = phrases.join(" ");
    let mut futures = Vec::with_capacity(types.len());
    for ty in types {
        let topic = layout.typed(*ty);
        let url = format!("{api}/_search");
        let body = bm25_query_body(&topic, namespace, &joined, fanout);
        futures.push(post_search(http.clone(), url, body));
    }
    let results = futures::future::try_join_all(futures).await?;
    Ok(results.into_iter().flatten().collect())
}

async fn run_vector(
    http: reqwest::Client,
    api: String,
    layout: &crate::topics::TopicLayout,
    types: &[MemoryType],
    query: String,
    fanout: usize,
) -> Result<Vec<TypedLoc>> {
    // Opt-in cross-encoder reranking. When `CHRONIK_MEMORY_RECALL_RERANK=1`
    // is set, ask the vector endpoint to overretrieve and rerank via the
    // configured reranker (VO-4). The server-side reranker falls back to
    // no-op when no endpoint is configured, so this is safe to leave on.
    let rerank_enabled = std::env::var("CHRONIK_MEMORY_RECALL_RERANK")
        .ok()
        .as_deref()
        == Some("1");
    let mut futures = Vec::with_capacity(types.len());
    for ty in types {
        let topic = layout.typed(*ty);
        let url = format!("{api}/_vector/{topic}/search");
        let body = if rerank_enabled {
            serde_json::json!({"query": query, "k": fanout, "rerank": true})
        } else {
            serde_json::json!({"query": query, "k": fanout})
        };
        let topic_owned = topic.clone();
        let http_owned = http.clone();
        futures.push(async move {
            post_vector_search(http_owned, url, topic_owned, body).await
        });
    }
    let nested = futures::future::try_join_all(futures).await?;
    Ok(nested.into_iter().flatten().collect())
}

async fn run_hyde(
    http: reqwest::Client,
    api: String,
    layout: &crate::topics::TopicLayout,
    types: &[MemoryType],
    query: String,
    gen: Arc<dyn TextGenerator>,
    fanout: usize,
) -> Result<Vec<TypedLoc>> {
    let prompt = format!(
        "Write a short, factual hypothetical answer (one or two sentences) \
that would be retrieved from an agent-memory database to answer this \
question. Do not preface or hedge — just the answer.\n\nQuestion: {query}"
    );
    let hypothetical = gen.complete(&prompt).await?;
    if hypothetical.trim().is_empty() {
        return Ok(vec![]);
    }
    // Use the hypothetical text as the vector-search query.
    run_vector(http, api, layout, types, hypothetical, fanout).await
}

async fn run_sql(
    http: reqwest::Client,
    api: String,
    layout: &crate::topics::TopicLayout,
    namespace: &str,
    types: &[MemoryType],
    filter: SqlFilter,
    fanout: usize,
) -> Result<Vec<EnrichedRecord>> {
    // Probe Chronik for available SQL tables. If empty, this surface isn't
    // ready in this deployment; degrade gracefully.
    let tables_url = format!("{api}/_sql/tables");
    let tables_resp = http.get(&tables_url).send().await;
    let tables_known: bool = match tables_resp {
        Ok(r) if r.status().is_success() => {
            match r.json::<serde_json::Value>().await {
                Ok(v) => v
                    .pointer("/tables")
                    .and_then(|t| t.as_array())
                    .map(|a| !a.is_empty())
                    .unwrap_or(false),
                Err(_) => false,
            }
        }
        _ => false,
    };
    if !tables_known {
        tracing::debug!(
            "SQL channel: /_sql/tables empty in this deployment — degrading gracefully"
        );
        return Ok(vec![]);
    }

    let mut out: Vec<EnrichedRecord> = Vec::new();
    for ty in types {
        let topic = layout.typed(*ty);
        // Namespace isolation in SQL — escape single quotes naively. The
        // namespace string is application-controlled, but we still defend.
        let ns_escaped = namespace.replace('\'', "''");
        let mut where_parts: Vec<String> = vec![format!("namespace = '{ns_escaped}'")];
        if let Some(t) = &filter.since {
            where_parts.push(format!("valid_from >= '{}'", t.to_rfc3339()));
        }
        if let Some(t) = &filter.before {
            where_parts.push(format!("valid_from <= '{}'", t.to_rfc3339()));
        }
        if let Some(c) = filter.min_confidence {
            where_parts.push(format!("confidence >= {c}"));
        }
        if let Some(extra) = &filter.extra_where {
            where_parts.push(format!("({extra})"));
        }
        // `where_parts` always has at least the namespace filter we just pushed.
        let where_clause = format!(" WHERE {}", where_parts.join(" AND "));
        let sql = format!(
            "SELECT * FROM \"{topic}\"{where_clause} LIMIT {fanout}"
        );
        let url = format!("{api}/_sql");
        let body = serde_json::json!({"query": sql});
        let resp = match http.post(&url).json(&body).send().await {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!(error = %e, "SQL channel: query failed, skipping");
                continue;
            }
        };
        if !resp.status().is_success() {
            continue;
        }
        let parsed: serde_json::Value = match resp.json().await {
            Ok(v) => v,
            Err(_) => continue,
        };
        if let Some(rows) = parsed.pointer("/rows").and_then(|r| r.as_array()) {
            for row in rows {
                let typed_loc = extract_typed_loc(row, &topic);
                if let Ok(record) = parse_envelope_from_source(row) {
                    out.push(EnrichedRecord { record, typed_loc });
                }
            }
        }
    }
    Ok(out)
}

/// Extract namespace-style subject ids and capitalized noun-ish tokens from
/// `query`. Used by [`Channel::KeyMatch`] internally and exposed publicly so
/// tooling (concept-page synthesizer, eval harness) can pick "what is this
/// query about" without re-implementing the same heuristic.
///
/// Returns deduplicated lowercase candidates. Drops stopwords and tokens
/// that don't match either form (`word:word` or `Capitalized`).
pub fn extract_subject_candidates(query: &str) -> Vec<String> {
    // Rule: namespace-style ids or capitalized noun-ish tokens.
    // We're permissive — the BM25 boost handles ranking.
    let mut out = Vec::new();
    for tok in query.split(|c: char| !(c.is_alphanumeric() || c == ':' || c == '_' || c == '-')) {
        if tok.is_empty() {
            continue;
        }
        if tok.contains(':') {
            // Looks like a namespace-style id (`user:luis`, `agent:bot`).
            out.push(tok.to_string());
            continue;
        }
        let lc = tok.to_lowercase();
        if STOPWORDS.contains(&lc.as_str()) {
            continue;
        }
        if tok.chars().next().map(|c| c.is_uppercase()).unwrap_or(false) {
            // Proper noun-ish — include lowercased form as a candidate.
            out.push(lc);
        }
    }
    out.sort();
    out.dedup();
    out
}

const STOPWORDS: &[&str] = &[
    // articles + copulas
    "a", "an", "the", "is", "are", "was", "were", "am", "be", "been", "being",
    // wh-words
    "what", "who", "how", "why", "where", "when", "which", "whom", "whose",
    // demonstratives + prepositions + conjunctions
    "this", "that", "these", "those", "of", "for", "to", "from", "in", "on",
    "with", "by", "at", "as", "and", "or", "but", "if", "then", "so",
    // do/have/say auxiliaries
    "did", "do", "does", "doing", "done", "have", "has", "had", "having",
    "say", "said", "says",
    // modal verbs — picked up by the bare-token rule even when lowercased
    "can", "could", "may", "might", "must", "should", "would", "will", "shall",
    // pronouns — `extract_subject_candidates` lowercases proper-noun tokens,
    // and the LongMemEval pilot 5 turned up "i" / "you" / "we" appearing as
    // false candidates from sentences like "How long have I had X?". They
    // never refer to a concrete entity we'd want to synthesise a concept
    // page for, so filter them here.
    "i", "you", "your", "yours", "yourself", "yourselves",
    "we", "us", "our", "ours", "they", "them", "their", "theirs",
    "he", "she", "him", "her", "his", "hers", "it", "its",
    "myself", "ourselves", "themselves", "himself", "herself", "itself",
    // common conversational verbs / domain noise
    "user", "users", "agent", "tell", "me", "about", "show", "find", "get",
    "want", "wants", "need", "needs", "like", "likes", "feel", "feels",
];

// ============================================================================
// On-demand synthesis (AMS-3.7) helpers
// ============================================================================

/// Per-memory snippet cap inside the synthesis prompt. 280 chars is enough
/// for a fact-text or event-context line plus structured fields, keeps the
/// prompt under ~3 KB at `k=10`, and matches the cap used by the LongMemEval
/// LLM-judge prompt for consistency.
const MAX_SYNTH_SNIPPET_CHARS: usize = 280;

/// Render one [`MemoryRecord`] as a single bulleted line for the synthesis
/// prompt. Includes type + valid_from + the most-informative typed fields so
/// the model can reason about recency and structured content (subject /
/// predicate / object for facts, actor / verb for events, etc.).
fn render_memory_for_synthesis(m: &MemoryRecord) -> String {
    use crate::schema::Body::*;
    let typed = match &m.body {
        Fact(f) => format!(
            "fact subject={} predicate={} object={} polarity={} text={}",
            f.subject,
            f.predicate,
            serde_json::to_string(&f.object).unwrap_or_default(),
            f.polarity,
            f.text
        ),
        Event(e) => format!(
            "event actor={} verb={} object={} context={}",
            e.actor,
            e.verb,
            e.object.as_deref().unwrap_or(""),
            e.context.as_deref().unwrap_or("")
        ),
        Instruction(i) => format!(
            "instruction scope={} trigger={} rule={}",
            i.scope, i.trigger, i.rule
        ),
        Task(t) => format!(
            "task title={} state={:?} task_id={}",
            t.title, t.state, t.task_id
        ),
        Concept(c) => format!(
            "concept entity_type={} title={} markdown={}",
            c.entity_type, c.title, c.markdown
        ),
    };
    let truncated: String = typed.chars().take(MAX_SYNTH_SNIPPET_CHARS).collect();
    format!("({}) {}", m.valid_from.to_rfc3339(), truncated)
}

/// Build the synthesis prompt. Format is deliberately minimal — bulleted
/// memories with timestamps, then the question, then a strict instruction
/// to either answer concisely or emit the abstention literal verbatim. The
/// abstention contract is what makes this useful for LongMemEval: under a
/// substring scorer "I don't know." is treated as a miss, but caller code
/// can detect it via `SynthesizedAnswer::abstained` and avoid surfacing
/// hallucinated answers.
fn build_synthesis_prompt(question: &str, memories: &[RecallResult]) -> String {
    // Env-gated A/B: `CHRONIK_MEMORY_SYNTH_PROMPT=v2` selects the
    // assistant-fact-committing variant (Option B from the LongMemEval pilot
    // ladder). Default stays v1 so measurements against the pilot-7/8 baseline
    // aren't disturbed unless the operator explicitly opts in.
    match std::env::var("CHRONIK_MEMORY_SYNTH_PROMPT").as_deref() {
        Ok("v2") => build_synthesis_prompt_v2(question, memories),
        _ => build_synthesis_prompt_v1(question, memories),
    }
}

/// v1 — original pilot-7 anti-abstention prompt. Kept for baseline comparison.
fn build_synthesis_prompt_v1(question: &str, memories: &[RecallResult]) -> String {
    let mut bullets = String::new();
    for (i, r) in memories.iter().enumerate() {
        let line = render_memory_for_synthesis(&r.memory);
        bullets.push_str(&format!("[{}] {}\n", i + 1, line));
    }
    format!(
        "You are a precise question-answering assistant working with an agent's long-term memory. \
Answer the user's question using ONLY the provided memories.\n\
\n\
Rules:\n\
- When two memories conflict (e.g. an updated preference, a changed budget), prefer the one with the most recent timestamp.\n\
- **Arithmetic questions** (\"how many\", \"total\", \"sum\", \"average\", \"how much\"): if you see ANY value-bearing memories — counts, durations, dollar amounts, quantities — even just two of them, COMPUTE the aggregate from those values. Do NOT abstain just because the question asks for a number; commit to the sum / count / average of what you actually see. Return only the computed value (e.g. \"$720\", \"3\", \"four weeks\"). Show the arithmetic only if asked.\n\
- **Temporal questions** (\"how long ago\", \"how many days\", \"when did I last\"): reason from memory timestamps and any time-anchored content (a memory dated 2026-04-01 about an event implies \"~four weeks ago\" relative to the recent memories). Output the duration / date naturally (\"four weeks\", \"about two hours\", \"over a year\").\n\
- Be concise — one sentence whenever possible. No preamble, no \"Based on the memories...\", just the answer.\n\
- **Abstain only when no relevant memory exists.** Reply EXACTLY: {ABSTAIN_LITERAL} if (and only if) the memories carry nothing that could plausibly answer the question even after computation or temporal reasoning. Don't abstain just because the answer requires combining facts — that's the job.\n\
\n\
Memories (relevance order, with timestamps):\n\
{bullets}\n\
Question: {question}\n\
\n\
Answer:"
    )
}

/// v2 — commits to assistant-stated facts (Option B).
///
/// Pilot 8/9 diagnosis: `single-session-assistant` items stayed 0/3 not because
/// extraction missed the facts (pilot 9's TwoPassExtractor confirmed the facts
/// DO reach recall) but because the synthesizer refused to commit to them when
/// the user hadn't explicitly restated the assistant's statement. This variant
/// tells the model that assistant-stated facts the user was present for are
/// authoritative and should be committed to.
///
/// It also tightens the "single clear candidate" case: when exactly one memory
/// plausibly answers the question, commit rather than hedge.
fn build_synthesis_prompt_v2(question: &str, memories: &[RecallResult]) -> String {
    let mut bullets = String::new();
    for (i, r) in memories.iter().enumerate() {
        let line = render_memory_for_synthesis(&r.memory);
        bullets.push_str(&format!("[{}] {}\n", i + 1, line));
    }
    format!(
        "You are a precise question-answering assistant working with an agent's long-term memory. \
Answer the user's question using ONLY the provided memories.\n\
\n\
Rules:\n\
- When two memories conflict (e.g. an updated preference, a changed budget), prefer the one with the most recent timestamp.\n\
- **Assistant-stated facts count.** These memories were extracted from a conversation the user was present for. If a memory records the assistant naming, describing, recommending, or quantifying something — a person's clothing, a place, a food, a brand, a duration, a count — and the user did not object in a later turn, treat that as a settled fact. Commit to it. Do NOT abstain because the user did not restate it.\n\
- **One clear candidate wins.** If the retrieved memories contain exactly one fact that plausibly answers the question, commit to it. Do NOT abstain because you are not 100% certain — abstention costs more than a specific answer in this task.\n\
- **Arithmetic questions** (\"how many\", \"total\", \"sum\", \"average\", \"how much\"): if you see ANY value-bearing memories — counts, durations, dollar amounts, quantities — even just two of them, COMPUTE the aggregate from those values. Return only the computed value (e.g. \"$720\", \"3\", \"four weeks\"). Show the arithmetic only if asked.\n\
- **Temporal questions** (\"how long ago\", \"how many days\", \"when did I last\"): reason from memory timestamps and any time-anchored content. Output the duration / date naturally (\"four weeks\", \"about two hours\", \"over a year\").\n\
- Be concise — one sentence whenever possible. No preamble, no \"Based on the memories...\", just the answer.\n\
- **Abstain only when NO retrieved memory is topically relevant.** Reply EXACTLY: {ABSTAIN_LITERAL} only if every listed memory is clearly unrelated to the question. Do NOT abstain when memories touch the topic but require you to commit to a specific value, entity, or fact — committing IS the job.\n\
\n\
Memories (relevance order, with timestamps):\n\
{bullets}\n\
Question: {question}\n\
\n\
Answer:"
    )
}

/// Detect the abstention literal in a tolerant way — match the first
/// non-whitespace, non-punctuation sentence against canonical phrasings.
/// We accept the exact literal `"I don't know."` plus a few common
/// variants the model may emit despite the EXACTLY instruction
/// (`"I don't know"` without the period, `"I do not know."`).
fn is_abstention(answer: &str) -> bool {
    let trimmed = answer.trim().to_lowercase();
    let cleaned: String = trimmed
        .trim_start_matches(|c: char| !c.is_alphanumeric())
        .chars()
        .take(40)
        .collect();
    cleaned.starts_with("i don't know")
        || cleaned.starts_with("i do not know")
        || cleaned.starts_with("i dont know")
}

// ============================================================================
// HTTP wire types
// ============================================================================

#[derive(Debug, Clone, Deserialize)]
struct SearchResponse {
    #[allow(dead_code)]
    #[serde(default)]
    took: u64,
    hits: HitsInfo,
}

#[derive(Debug, Clone, Deserialize)]
struct HitsInfo {
    #[allow(dead_code)]
    #[serde(default)]
    total: serde_json::Value,
    #[serde(default)]
    hits: Vec<Hit>,
}

#[derive(Debug, Clone, Deserialize)]
struct Hit {
    #[allow(dead_code)]
    #[serde(default)]
    _index: String,
    #[allow(dead_code)]
    #[serde(default)]
    _id: String,
    #[serde(default)]
    _score: Option<f32>,
    _source: serde_json::Value,
}

#[derive(Debug, Clone, Deserialize)]
struct VectorSearchResponse {
    #[serde(default)]
    results: Vec<VectorSearchHit>,
}

#[derive(Debug, Clone, Deserialize)]
struct VectorSearchHit {
    partition: i32,
    offset: i64,
    #[allow(dead_code)]
    score: f32,
}

async fn post_search(
    http: reqwest::Client,
    url: String,
    body: serde_json::Value,
) -> Result<Vec<EnrichedRecord>> {
    let topic_hint = body
        .get("index")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let resp = http
        .post(&url)
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| MemoryError::Http(format!("{url}: {e}")))?;

    let status = resp.status();
    if !status.is_success() {
        if status.as_u16() == 404 {
            return Ok(vec![]);
        }
        let txt = resp.text().await.unwrap_or_default();
        return Err(MemoryError::Http(format!(
            "search {url} returned {status}: {txt}"
        )));
    }

    let parsed: SearchResponse = resp
        .json()
        .await
        .map_err(|e| MemoryError::Http(format!("parse {url}: {e}")))?;

    let mut out = Vec::with_capacity(parsed.hits.hits.len());
    for h in parsed.hits.hits {
        let _ = h._score;
        let typed_loc = extract_typed_loc_with_hint(&h._source, &h._index, topic_hint.as_deref());
        match parse_envelope_from_source(&h._source) {
            Ok(record) => out.push(EnrichedRecord { record, typed_loc }),
            Err(e) => {
                tracing::warn!(error = %e, "skipping hit with un-parseable _source");
            }
        }
    }
    Ok(out)
}

async fn post_vector_search(
    http: reqwest::Client,
    url: String,
    topic: String,
    body: serde_json::Value,
) -> Result<Vec<TypedLoc>> {
    let resp = match http
        .post(&url)
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!(error = %e, %url, "vector search failed; degrading channel");
            return Ok(vec![]);
        }
    };
    let status = resp.status();
    if !status.is_success() {
        if status.as_u16() == 404 {
            return Ok(vec![]);
        }
        tracing::debug!(%status, %url, "vector search non-200; degrading channel");
        return Ok(vec![]);
    }
    let parsed: VectorSearchResponse = match resp.json().await {
        Ok(p) => p,
        Err(e) => {
            tracing::debug!(error = %e, "vector search response unparseable; degrading");
            return Ok(vec![]);
        }
    };
    Ok(parsed
        .results
        .into_iter()
        .map(|h| (topic.clone(), h.partition, h.offset))
        .collect())
}

/// Extract typed `(topic, partition, offset)` from a wrapped Chronik `_source`
/// when the wrapped shape is in use. Returns `None` for the direct shape (no
/// Kafka coordinates surfaced) — id-only channels then can't boost that row,
/// which is fine: the row still scores via its full-envelope channel.
///
/// Tolerates two field-naming flavours of the wrapped shape:
/// - **bare**: `topic`, `partition`, `offset` (older Chronik responses, also
///   what the test fixtures use)
/// - **underscored**: `_topic`, `_partition`, `_offset` (Chronik 2.5+ index-
///   doc indexing — observed against a live cluster)
///
/// `topic_fallback` is the typed topic the caller queried against — used
/// when neither shape surfaces its own topic name.
fn extract_typed_loc(
    source: &serde_json::Value,
    topic_fallback: &str,
) -> Option<TypedLoc> {
    let partition = source
        .get("partition")
        .or_else(|| source.get("_partition"))
        .and_then(|v| v.as_i64())? as i32;
    let offset = source
        .get("offset")
        .or_else(|| source.get("_offset"))
        .and_then(|v| v.as_i64())?;
    let topic = source
        .get("topic")
        .or_else(|| source.get("_topic"))
        .and_then(|v| v.as_str())
        .unwrap_or(topic_fallback)
        .to_string();
    Some((topic, partition, offset))
}

/// Same as [`extract_typed_loc`] but consults two fallback hints in order:
/// the `_index` field on the hit (set by Tantivy from the indexed topic) and
/// the `index` field of the original request body. Either one resolves to the
/// typed-topic name we queried.
fn extract_typed_loc_with_hint(
    source: &serde_json::Value,
    index_hint: &str,
    body_index_hint: Option<&str>,
) -> Option<TypedLoc> {
    let fallback = if !index_hint.is_empty() {
        index_hint
    } else {
        body_index_hint.unwrap_or("")
    };
    extract_typed_loc(source, fallback)
}

/// Chronik's `/_search` returns the record envelope under several shapes,
/// depending on indexing pipeline version:
///
/// 1. **Direct**: `_source` IS the envelope:
///    `{ "memory_id": "...", "type": "fact", "body": {...} }`
/// 2. **Wrapped (bare)**: `_source` is a Kafka-record meta object whose
///    `value` field is the JSON-encoded envelope:
///    `{ "topic": "...", "partition": 0, "offset": 11, "key": "...",
///       "timestamp": ..., "value": "<JSON envelope>" }`
/// 3. **Wrapped (underscored, Chronik 2.5+)**: same idea but with leading
///    underscores on every meta field, plus a `_json_content` mirror:
///    `{ "_topic": "...", "_partition": 0, "_offset": 11, "_key": "...",
///       "_value": "<JSON envelope>", "_json_content": "<same JSON>" }`
pub(crate) fn parse_envelope_from_source(
    source: &serde_json::Value,
) -> Result<MemoryRecord> {
    if let Ok(m) = serde_json::from_value::<MemoryRecord>(source.clone()) {
        return Ok(m);
    }
    // Try bare `value` first (older shape), then `_value`, then `_json_content`
    // (Chronik 2.5 index-doc shape).
    for field in &["value", "_value", "_json_content"] {
        if let Some(value_field) = source.get(*field) {
            if let Some(s) = value_field.as_str() {
                if let Ok(m) = serde_json::from_str::<MemoryRecord>(s) {
                    return Ok(m);
                }
            } else if let Ok(m) = serde_json::from_value::<MemoryRecord>(value_field.clone()) {
                return Ok(m);
            }
        }
    }
    Err(MemoryError::Schema(format!(
        "search hit _source has no envelope or wrapped value field: {source}"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{Body, EventBody, FactBody, Source};

    fn fact_record(version: u64, key: &str, ns: &str) -> MemoryRecord {
        let now = Utc::now();
        MemoryRecord {
            memory_id: format!("id-{version}"),
            tenant_id: "t".into(),
            namespace: ns.into(),
            key: Some(key.into()),
            version,
            created_at: now,
            valid_from: now,
            valid_to: None,
            confidence: 1.0,
            source: Source {
                topic: "mem.raw.t".into(),
                offsets: vec![1],
                extractor: "x@1".into(),
            },
            tombstoned: false,
            body: Body::Fact(FactBody {
                subject: "s".into(),
                predicate: "p".into(),
                object: serde_json::json!("o"),
                polarity: "asserted".into(),
                text: "t".into(),
            }),
        }
    }

    fn event_record(id: &str, ns: &str) -> MemoryRecord {
        let now = Utc::now();
        MemoryRecord {
            memory_id: id.into(),
            tenant_id: "t".into(),
            namespace: ns.into(),
            key: None,
            version: 1,
            created_at: now,
            valid_from: now,
            valid_to: None,
            confidence: 1.0,
            source: Source {
                topic: "mem.raw.t".into(),
                offsets: vec![1],
                extractor: "x@1".into(),
            },
            tombstoned: false,
            body: Body::Event(EventBody {
                actor: "u".into(),
                verb: "v".into(),
                object: None,
                channel: None,
                context: None,
                ts: now,
            }),
        }
    }

    fn scored(score: f64, m: MemoryRecord) -> RecallResult {
        let mut chans = HashMap::new();
        chans.insert(Channel::Bm25, score);
        RecallResult {
            score,
            memory: m,
            channels: chans,
        }
    }

    #[test]
    fn dedup_keeps_higher_version() {
        let r1 = scored(0.10, fact_record(1, "k", "n"));
        let r2 = scored(0.05, fact_record(2, "k", "n"));
        let out = dedup_results_keep_max_score(vec![r1, r2]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].memory.version, 2);
    }

    #[test]
    fn dedup_distinct_namespaces_pass_through() {
        let r1 = scored(0.3, fact_record(1, "k", "ns1"));
        let r2 = scored(0.2, fact_record(1, "k", "ns2"));
        let out = dedup_results_keep_max_score(vec![r1, r2]);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn dedup_passes_events_through() {
        let r1 = scored(0.3, event_record("e1", "ns"));
        let r2 = scored(0.2, event_record("e2", "ns"));
        let out = dedup_results_keep_max_score(vec![r1, r2]);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn parse_envelope_handles_wrapped_shape() {
        let envelope = serde_json::json!({
            "memory_id": "01HV5",
            "tenant_id": "acme",
            "namespace": "acme:agent:bot",
            "key": "user:luis|prefers",
            "version": 1,
            "created_at": "2026-04-25T10:00:00Z",
            "valid_from": "2026-04-25T10:00:00Z",
            "confidence": 0.9,
            "source": {"topic": "mem.raw.acme", "offsets": [1], "extractor": "x@1"},
            "type": "fact",
            "body": {"subject": "user:luis", "predicate": "prefers", "object": "Lapa", "polarity": "asserted", "text": "Luis prefers Lapa"}
        });
        let wrapped = serde_json::json!({
            "topic": "mem.fact.acme",
            "partition": 0,
            "offset": 11,
            "key": "user:luis|prefers",
            "value": envelope.to_string()
        });
        let m = parse_envelope_from_source(&wrapped).unwrap();
        assert_eq!(m.body.kind(), MemoryType::Fact);
    }

    #[test]
    fn parse_envelope_handles_direct_shape() {
        let envelope = serde_json::json!({
            "memory_id": "01HV5",
            "tenant_id": "acme",
            "namespace": "acme:agent:bot",
            "key": "k",
            "version": 1,
            "created_at": "2026-04-25T10:00:00Z",
            "valid_from": "2026-04-25T10:00:00Z",
            "confidence": 0.9,
            "source": {"topic": "mem.raw.acme", "offsets": [1], "extractor": "x@1"},
            "type": "fact",
            "body": {"subject": "s", "predicate": "p", "object": "o", "polarity": "asserted", "text": "t"}
        });
        let m = parse_envelope_from_source(&envelope).unwrap();
        assert_eq!(m.body.kind(), MemoryType::Fact);
    }

    #[test]
    fn extract_subjects_finds_namespace_ids() {
        let s = extract_subject_candidates("tell me about user:luis and agent:bot");
        assert!(s.contains(&"user:luis".to_string()));
        assert!(s.contains(&"agent:bot".to_string()));
    }

    #[test]
    fn extract_subjects_handles_capitalized_nouns() {
        let s = extract_subject_candidates("Where is Lapa neighborhood?");
        assert!(s.contains(&"lapa".to_string()));
        // "neighborhood" isn't capitalized so not extracted; "where", "is" filtered
        assert!(!s.contains(&"is".to_string()));
        assert!(!s.contains(&"where".to_string()));
    }

    #[test]
    fn extract_subjects_filters_stopwords() {
        let s = extract_subject_candidates("what is this user");
        // "what", "is", "this", "user" all stopwords
        assert!(s.is_empty(), "got: {s:?}");
    }

    #[test]
    fn sql_filter_default_is_unrestricted() {
        let f = SqlFilter::default();
        assert!(f.since.is_none());
        assert!(f.before.is_none());
        assert!(f.min_confidence.is_none());
    }

    #[test]
    fn extract_typed_loc_from_wrapped_source() {
        let wrapped = serde_json::json!({
            "topic": "mem.fact.acme",
            "partition": 3,
            "offset": 42,
            "key": "user:luis|prefers",
            "value": "{}"
        });
        let loc = extract_typed_loc(&wrapped, "mem.fact.acme").expect("loc");
        assert_eq!(loc, ("mem.fact.acme".to_string(), 3, 42));
    }

    #[test]
    fn extract_typed_loc_falls_back_to_topic_hint_when_topic_missing() {
        let wrapped = serde_json::json!({
            "partition": 0,
            "offset": 7,
            "value": "{}"
        });
        let loc = extract_typed_loc(&wrapped, "mem.fact.acme").expect("loc");
        assert_eq!(loc, ("mem.fact.acme".to_string(), 0, 7));
    }

    #[test]
    fn extract_typed_loc_returns_none_for_direct_envelope_shape() {
        let direct = serde_json::json!({
            "memory_id": "01HV5",
            "tenant_id": "acme",
            "type": "fact",
            "body": {"subject": "s", "predicate": "p", "object": "o", "polarity": "asserted", "text": "t"}
        });
        assert!(extract_typed_loc(&direct, "mem.fact.acme").is_none());
    }

    #[test]
    fn extract_typed_loc_with_hint_uses_index_field_when_topic_missing() {
        let wrapped = serde_json::json!({
            "partition": 1,
            "offset": 99,
            "value": "{}"
        });
        let loc = extract_typed_loc_with_hint(&wrapped, "mem.fact.acme", None)
            .expect("loc");
        assert_eq!(loc, ("mem.fact.acme".to_string(), 1, 99));
    }

    #[test]
    fn extract_typed_loc_with_hint_falls_back_to_body_index_when_index_blank() {
        let wrapped = serde_json::json!({
            "partition": 1,
            "offset": 99,
            "value": "{}"
        });
        let loc = extract_typed_loc_with_hint(&wrapped, "", Some("mem.fact.acme"))
            .expect("loc");
        assert_eq!(loc, ("mem.fact.acme".to_string(), 1, 99));
    }

    #[test]
    fn extract_typed_loc_handles_underscored_chronik_25_shape() {
        let wrapped = serde_json::json!({
            "_id": "mem.fact.acme-0-11",
            "_topic": "mem.fact.acme",
            "_partition": 0,
            "_offset": 11,
            "_key": "user:luis|prefers",
            "_value": "{}"
        });
        let loc = extract_typed_loc(&wrapped, "mem.fact.acme").expect("loc");
        assert_eq!(loc, ("mem.fact.acme".to_string(), 0, 11));
    }

    #[test]
    fn parse_envelope_handles_underscored_value_field() {
        let envelope = serde_json::json!({
            "memory_id": "01HV5",
            "tenant_id": "acme",
            "namespace": "acme:agent:bot",
            "key": "user:luis|prefers",
            "version": 1,
            "created_at": "2026-04-25T10:00:00Z",
            "valid_from": "2026-04-25T10:00:00Z",
            "confidence": 0.9,
            "source": {"topic": "mem.raw.acme", "offsets": [1], "extractor": "x@1"},
            "type": "fact",
            "body": {"subject": "user:luis", "predicate": "prefers", "object": "Lapa", "polarity": "asserted", "text": "Luis prefers Lapa"}
        });
        let wrapped = serde_json::json!({
            "_id": "mem.fact.acme-0-11",
            "_topic": "mem.fact.acme",
            "_partition": 0,
            "_offset": 11,
            "_key": "user:luis|prefers",
            "_value": envelope.to_string()
        });
        let m = parse_envelope_from_source(&wrapped).expect("parse");
        assert_eq!(m.body.kind(), MemoryType::Fact);
    }

    #[test]
    fn is_abstention_recognizes_canonical_literal() {
        assert!(is_abstention("I don't know."));
        assert!(is_abstention("  I don't know.  "));
        assert!(is_abstention("\"I don't know.\""));
        // Variants the model sometimes emits despite EXACTLY instruction.
        assert!(is_abstention("I don't know"));
        assert!(is_abstention("I do not know."));
        assert!(is_abstention("I dont know"));
    }

    #[test]
    fn is_abstention_rejects_real_answers() {
        assert!(!is_abstention("800000"));
        assert!(!is_abstention("The user prefers Lapa."));
        assert!(!is_abstention("Yes, the user is allergic to peanuts."));
        assert!(!is_abstention(""));
    }

    #[test]
    fn render_memory_for_synthesis_includes_typed_fields_and_timestamp() {
        let r = fact_record(1, "user:luis|prefers", "ns");
        let line = render_memory_for_synthesis(&r);
        // Must include the memory's valid_from rfc3339 prefix for recency reasoning.
        assert!(line.starts_with('('));
        // Must include the typed-fact shape so the LLM can reason structurally.
        assert!(line.contains("fact subject="));
        assert!(line.contains("predicate="));
    }

    #[test]
    fn build_synthesis_prompt_renders_bullets_and_abstention_rule() {
        let memories = vec![scored(0.5, fact_record(1, "k", "ns"))];
        let p = build_synthesis_prompt("What is X?", &memories);
        assert!(p.contains("[1]"));
        assert!(p.contains("Question: What is X?"));
        assert!(p.contains(ABSTAIN_LITERAL));
        // Must instruct the model to prefer the most recent timestamp on conflict.
        assert!(p.contains("most recent"));
    }

    #[test]
    fn parse_envelope_handles_json_content_mirror_shape() {
        let envelope = serde_json::json!({
            "memory_id": "01HV5",
            "tenant_id": "acme",
            "namespace": "acme:agent:bot",
            "key": "user|customer_id",
            "version": 1,
            "created_at": "2026-04-25T10:00:00Z",
            "valid_from": "2026-04-25T10:00:00Z",
            "confidence": 1.0,
            "source": {"topic": "mem.raw.acme", "offsets": [2], "extractor": "x@1"},
            "type": "fact",
            "body": {"subject": "user", "predicate": "customer_id", "object": "C-9182", "polarity": "asserted", "text": "User's customer ID is C-9182"}
        });
        // Chronik 2.5 ships envelope on `_json_content` and `_value`. Drop
        // `_value` here to verify `_json_content` works on its own.
        let wrapped = serde_json::json!({
            "_id": "mem.fact.acme-0-11",
            "_topic": "mem.fact.acme",
            "_partition": 0,
            "_offset": 11,
            "_json_content": envelope.to_string()
        });
        let m = parse_envelope_from_source(&wrapped).expect("parse");
        assert_eq!(m.body.kind(), MemoryType::Fact);
    }
}
