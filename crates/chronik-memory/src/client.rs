//! `Memory` — top-level client struct and builder.
//!
//! Holds the rdkafka producer, the HTTP client, the topic layout, the optional
//! extractor, and the idempotency cache. Cheap to clone (`Arc` interior).

use crate::error::{MemoryError, Result};
use crate::extractor::{Extracted, Extractor, Turn};
use crate::forget::resolve_tombstone_target;
use crate::idempotency::IdempotencyCache;
use crate::ingest::{turn_record_key, validate_turn, IngestAck, RawTurnRecord};
use crate::recall::RecallBuilder;
use crate::remember::{build_envelope, record_key};
use crate::schema::{Body, MemoryRecord, MemoryType, TaskBody, TaskState};
use crate::topics::{NamespacePath, TopicConfig, TopicLayout};
use chrono::Utc;
use parking_lot::Mutex;
use rdkafka::admin::{AdminClient, AdminOptions, NewTopic, TopicReplication};
use rdkafka::client::DefaultClientContext;
use rdkafka::producer::{FutureProducer, FutureRecord};
use rdkafka::ClientConfig;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info, instrument, warn};

/// Default produce timeout for synchronous awaits (in [`Memory::ingest`]).
const DEFAULT_PRODUCE_TIMEOUT: Duration = Duration::from_secs(5);

/// Default idempotency cache size.
const DEFAULT_IDEMPOTENCY_CAPACITY: usize = 1024;

/// Default idempotency TTL (5 minutes per AMS-1.2).
const DEFAULT_IDEMPOTENCY_TTL: Duration = Duration::from_secs(300);

/// Top-level SDK client.
///
/// Build with [`Memory::builder`]. Cheap to clone (`Arc<Inner>`).
#[derive(Clone)]
pub struct Memory {
    inner: Arc<Inner>,
}

struct Inner {
    namespace: NamespacePath,
    layout: TopicLayout,
    chronik_api: String,
    /// Bootstrap-server string used for the producer + admin (re-used by the
    /// background worker, AMS-2.2, when constructing its consumer).
    kafka_brokers: String,
    producer: FutureProducer,
    admin: AdminClient<DefaultClientContext>,
    /// Reused by recall (AMS-1.4) for `/_search` / `/_vector` fan-out.
    #[allow(dead_code)]
    http: reqwest::Client,
    extractor: Option<Arc<dyn Extractor>>,
    idempotency: Mutex<IdempotencyCache>,
}

impl std::fmt::Debug for Memory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Memory")
            .field("namespace", &self.inner.namespace.full)
            .field("chronik_api", &self.inner.chronik_api)
            .field(
                "extractor",
                &self.inner.extractor.as_ref().map(|e| e.id()),
            )
            .finish()
    }
}

impl Memory {
    /// Start building a new client.
    pub fn builder() -> MemoryBuilder {
        MemoryBuilder::default()
    }

    /// Full namespace path (including tenant).
    pub fn namespace(&self) -> &str {
        &self.inner.namespace.full
    }

    /// Tenant identifier (first segment of the namespace).
    pub fn tenant(&self) -> &str {
        self.inner.layout.tenant()
    }

    /// Topic layout for this namespace.
    pub fn topic_layout(&self) -> &TopicLayout {
        &self.inner.layout
    }

    /// Chronik unified-API base URL (`http://host:6092`).
    pub fn chronik_api(&self) -> &str {
        &self.inner.chronik_api
    }

    /// Kafka bootstrap-server string (post-scheme-strip). Exposed so the
    /// background worker (AMS-2.2) can construct a consumer pointed at the
    /// same brokers as the client's producer.
    pub fn kafka_brokers(&self) -> &str {
        &self.inner.kafka_brokers
    }

    /// Reference to the optional extractor — used by Phase 1 sync-extract path
    /// (AMS-1.3) and the Phase 2 background worker (AMS-2.2).
    pub fn extractor(&self) -> Option<&Arc<dyn Extractor>> {
        self.inner.extractor.as_ref()
    }

    /// HTTP client — used by the recall path (AMS-1.4).
    pub(crate) fn http(&self) -> &reqwest::Client {
        &self.inner.http
    }

    /// Build a recall query against this namespace's typed topics.
    ///
    /// See [`RecallBuilder`](crate::recall::RecallBuilder) for available knobs.
    pub fn recall(&self, query: impl Into<String>) -> RecallBuilder<'_> {
        RecallBuilder::new(self, query)
    }

    /// Test-only: peek at whether a given turn would be classified as a
    /// duplicate by the in-process LRU cache *without* producing to Kafka.
    /// Returns `true` if the cache already holds a fresh entry for this
    /// turn's record key.
    ///
    /// Used by the AMS-2.6 idempotency tests to verify the dedup path.
    /// Production code should call [`ingest`](Self::ingest) /
    /// [`ingest_turn`](Self::ingest_turn) directly — those return an
    /// [`IngestAck`] whose `deduped` field reports the same information after
    /// the produce decision is made.
    pub fn dedup_state(&self) -> DedupState<'_> {
        DedupState { memory: self }
    }
}

/// Inspector returned by [`Memory::dedup_state`] — used in tests to introspect
/// the SDK-local idempotency LRU.
#[derive(Debug)]
pub struct DedupState<'a> {
    memory: &'a Memory,
}

impl<'a> DedupState<'a> {
    /// Number of entries currently in the cache.
    pub fn len(&self) -> usize {
        self.memory.inner.idempotency.lock().len()
    }

    /// True when the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.memory.inner.idempotency.lock().is_empty()
    }

    /// Check whether ingesting `turn` right now would short-circuit on the
    /// LRU cache (`true` = deduped, no Kafka produce). **Calling this method
    /// also consumes a slot in the cache** — same semantics as
    /// [`crate::idempotency::IdempotencyCache::check_and_insert`].
    ///
    /// Use only for tests / introspection. Production code should call
    /// `ingest_turn` and read `IngestAck::deduped` instead.
    pub fn check_and_insert(&self, turn: &Turn) -> bool {
        let key = turn_record_key(&self.memory.inner.namespace.full, turn);
        self.memory.inner.idempotency.lock().check_and_insert(key)
    }
}

impl Memory {

    /// Create the Phase 1 topic set: `mem.raw.*`, `mem.fact.{tenant}`, `mem.event.{tenant}`.
    /// Idempotent — existing topics are left alone.
    ///
    /// Phase 2 (AMS-2.1) extends this to also create instruction / task / task.current.
    pub async fn init_namespace(&self) -> Result<()> {
        let layout = &self.inner.layout;
        let configs = vec![
            TopicConfig::raw(layout.raw()),
            TopicConfig::fact(layout.typed(MemoryType::Fact)),
            TopicConfig::event(layout.typed(MemoryType::Event)),
        ];
        self.create_topics_idempotent(&configs).await
    }

    /// Phase 2 helper — creates the full Phase 2 topic set: raw + fact + event
    /// + instruction + task. Tasks are stored on a single compacted topic
    /// (`mem.task.{tenant}`) keyed by `task_id`; the legacy "current view"
    /// topic is no longer needed (see `topics::TopicConfig::task` docs).
    pub async fn init_namespace_full(&self) -> Result<()> {
        let layout = &self.inner.layout;
        let configs = vec![
            TopicConfig::raw(layout.raw()),
            TopicConfig::fact(layout.typed(MemoryType::Fact)),
            TopicConfig::event(layout.typed(MemoryType::Event)),
            TopicConfig::instruction(layout.typed(MemoryType::Instruction)),
            TopicConfig::task(layout.typed(MemoryType::Task)),
            TopicConfig::audit(layout.audit()), // AM-2.6
        ];
        self.create_topics_idempotent(&configs).await
    }

    async fn create_topics_idempotent(&self, configs: &[TopicConfig]) -> Result<()> {
        let new_topics: Vec<NewTopic<'_>> = configs
            .iter()
            .map(|c| {
                let mut t = NewTopic::new(&c.name, 1, TopicReplication::Fixed(1))
                    .set("cleanup.policy", c.cleanup_policy);
                // `bm25_enabled` is intentionally NOT sent — it's not a
                // recognised per-topic key. Search is enabled globally on the
                // broker via `CHRONIK_DEFAULT_SEARCHABLE=true`.
                if c.vector_enabled {
                    t = t.set("vector.enabled", "true");
                }
                if c.columnar_enabled {
                    t = t.set("columnar.enabled", "true");
                }
                t
            })
            .collect();

        let opts = AdminOptions::new().request_timeout(Some(Duration::from_secs(10)));
        let results = self
            .inner
            .admin
            .create_topics(new_topics.iter(), &opts)
            .await
            .map_err(|e| MemoryError::Kafka(format!("create_topics: {e}")))?;

        for r in results {
            match r {
                Ok(name) => debug!(topic = %name, "topic created"),
                Err((name, code)) => {
                    // AlreadyExists is fine — idempotent.
                    if matches!(code, rdkafka::types::RDKafkaErrorCode::TopicAlreadyExists) {
                        debug!(topic = %name, "topic already exists");
                    } else {
                        warn!(topic = %name, ?code, "create_topic returned error");
                        return Err(MemoryError::Kafka(format!(
                            "create_topic({name}): {code:?}"
                        )));
                    }
                }
            }
        }
        info!(
            namespace = %self.inner.namespace.full,
            n_topics = configs.len(),
            "namespace initialized"
        );
        Ok(())
    }

    /// Convenience: ingest a single `(role, content)` pair with default timestamp.
    pub async fn ingest(
        &self,
        role: impl Into<String>,
        content: impl Into<String>,
    ) -> Result<IngestAck> {
        self.ingest_turn(Turn {
            role: role.into(),
            content: content.into(),
            ts: None,
            channel: None,
            external_id: None,
        })
        .await
    }

    /// Ingest a single fully-specified [`Turn`].
    #[instrument(skip(self, turn), fields(role = %turn.role, topic = %self.inner.layout.raw()))]
    pub async fn ingest_turn(&self, turn: Turn) -> Result<IngestAck> {
        validate_turn(&turn)?;
        let topic = self.inner.layout.raw();
        let key = turn_record_key(&self.inner.namespace.full, &turn);

        // In-process idempotency check — best-effort, per-process.
        if self.inner.idempotency.lock().check_and_insert(key.clone()) {
            debug!(topic = %topic, key = %key, "ingest deduped (in-process LRU hit)");
            return Ok(IngestAck::deduped(topic));
        }

        let now = Utc::now();
        let payload = serde_json::to_vec(&RawTurnRecord {
            role: &turn.role,
            content: &turn.content,
            ts: turn.ts.unwrap_or(now),
            received_at: now,
            channel: turn.channel.as_deref(),
            external_id: turn.external_id.as_deref(),
            namespace: &self.inner.namespace.full,
        })?;

        let record = FutureRecord::to(topic.as_str())
            .key(key.as_str())
            .payload(payload.as_slice());

        match self.inner.producer.send(record, DEFAULT_PRODUCE_TIMEOUT).await {
            Ok((partition, offset)) => Ok(IngestAck {
                topic,
                partition,
                offset,
                deduped: false,
            }),
            Err((e, _msg)) => Err(MemoryError::Kafka(format!("produce {topic}: {e}"))),
        }
    }

    /// Batch ingest. Iterates sequentially in Phase 1 — Phase 2 worker handles
    /// high-throughput pipelining.
    pub async fn ingest_batch(&self, turns: Vec<Turn>) -> Result<Vec<IngestAck>> {
        let mut acks = Vec::with_capacity(turns.len());
        for t in turns {
            acks.push(self.ingest_turn(t).await?);
        }
        Ok(acks)
    }

    /// Direct write to a typed memory topic — bypasses extraction.
    ///
    /// Validates that compactable types (fact / instruction / task) carry a
    /// non-empty key; events may pass `None`.
    #[instrument(skip(self, body), fields(kind = ?body.kind(), key = ?key))]
    pub async fn remember(
        &self,
        body: Body,
        key: Option<String>,
        confidence: f32,
    ) -> Result<IngestAck> {
        let raw_topic = self.inner.layout.raw();
        let extractor_id = self
            .inner
            .extractor
            .as_ref()
            .map(|e| e.id().to_string())
            .unwrap_or_else(|| "direct@1".to_string());

        let env = build_envelope(
            self.inner.layout.tenant(),
            &self.inner.namespace.full,
            body,
            key,
            confidence,
            &extractor_id,
            &raw_topic,
        )?;
        self.produce_memory(&env).await
    }

    /// Lower-level: produce a fully-formed [`MemoryRecord`] envelope. Used by the
    /// extractor's sync-mode path (AMS-1.3) and by callers importing memories
    /// from another system.
    pub async fn remember_record(&self, env: &MemoryRecord) -> Result<IngestAck> {
        self.produce_memory(env).await
    }

    async fn produce_memory(&self, env: &MemoryRecord) -> Result<IngestAck> {
        let topic = self.inner.layout.typed(env.body.kind());
        let key = record_key(env);
        let payload = serde_json::to_vec(env)?;
        let record = FutureRecord::to(topic.as_str())
            .key(key.as_str())
            .payload(payload.as_slice());

        match self.inner.producer.send(record, DEFAULT_PRODUCE_TIMEOUT).await {
            Ok((partition, offset)) => Ok(IngestAck {
                topic,
                partition,
                offset,
                deduped: false,
            }),
            Err((e, _)) => Err(MemoryError::Kafka(format!("produce typed: {e}"))),
        }
    }

    /// Phase 1 sync-mode ingest: produce raw turns, then immediately run the
    /// configured extractor and produce all extracted typed memories.
    ///
    /// Returns the offsets of every produced record (raw + typed) plus the raw
    /// extractions for inspection. Errors if no extractor was set on the builder.
    ///
    /// **Production note**: This is a Phase 1 helper for testing and demos.
    /// Production deployments use the Phase 2 background worker (AMS-2.2),
    /// which decouples ingest latency from LLM round-trip.
    #[instrument(skip(self, turns), fields(n_turns = turns.len(), namespace = %self.inner.namespace.full))]
    pub async fn ingest_with_extraction(
        &self,
        turns: Vec<Turn>,
    ) -> Result<ExtractionAck> {
        let extractor = self
            .inner
            .extractor
            .as_ref()
            .cloned()
            .ok_or_else(|| {
                MemoryError::Config(
                    "ingest_with_extraction requires .extractor(...) on the builder".into(),
                )
            })?;

        // Validate all up front so we don't half-produce.
        for t in &turns {
            validate_turn(t)?;
        }

        // 1. Produce raw turns serially, collecting actual Kafka offsets.
        // No dedup in this path — caller passed an explicit batch and expects
        // all turns to be visible to the extractor.
        let raw_topic = self.inner.layout.raw();
        let mut raw_acks = Vec::with_capacity(turns.len());
        for t in &turns {
            let now = Utc::now();
            let key = turn_record_key(&self.inner.namespace.full, t);
            let payload = serde_json::to_vec(&RawTurnRecord {
                role: &t.role,
                content: &t.content,
                ts: t.ts.unwrap_or(now),
                received_at: now,
                channel: t.channel.as_deref(),
                external_id: t.external_id.as_deref(),
                namespace: &self.inner.namespace.full,
            })?;
            let record = FutureRecord::to(raw_topic.as_str())
                .key(key.as_str())
                .payload(payload.as_slice());
            let (partition, offset) = self
                .inner
                .producer
                .send(record, DEFAULT_PRODUCE_TIMEOUT)
                .await
                .map_err(|(e, _)| MemoryError::Kafka(format!("produce raw: {e}")))?;
            raw_acks.push(IngestAck {
                topic: raw_topic.clone(),
                partition,
                offset,
                deduped: false,
            });
        }

        // 2. Run extraction against the in-memory turns (uses batch indexes).
        let extracted = extractor.extract(&turns).await?;

        // 3. For each extraction, map batch indexes → Kafka offsets and produce.
        let mut typed_acks = Vec::with_capacity(extracted.len());
        let extractor_id = extractor.id().to_string();
        for ex in &extracted {
            let offsets: Vec<i64> = ex
                .source_indexes
                .iter()
                .filter_map(|&i| raw_acks.get(i).map(|a| a.offset))
                .collect();
            if offsets.is_empty() {
                // Shouldn't happen — extractor's filter_and_convert rejects empty
                // citations — but if it does, drop the extraction.
                continue;
            }

            let mut env = build_envelope(
                self.inner.layout.tenant(),
                &self.inner.namespace.full,
                ex.body.clone(),
                ex.key.clone(),
                ex.confidence,
                &extractor_id,
                &raw_topic,
            )?;
            env.source.offsets = offsets;
            typed_acks.push(self.produce_memory(&env).await?);
        }

        Ok(ExtractionAck {
            raw_acks,
            typed_acks,
            extracted,
        })
    }

    /// Phase 2: Task state-machine helper. Produces a record on `mem.task.{tenant}`
    /// keyed by `task_id` with the new state. The topic is compacted, so the
    /// most recent `update_task` call wins per-task.
    ///
    /// Use [`Self::current_tasks`] to read the latest state across all tasks
    /// for this namespace.
    pub async fn update_task(
        &self,
        task_id: impl Into<String>,
        title: impl Into<String>,
        state: TaskState,
        due_at: Option<chrono::DateTime<Utc>>,
        owner: Option<String>,
    ) -> Result<IngestAck> {
        let task_id = task_id.into();
        if task_id.is_empty() {
            return Err(MemoryError::InvalidArgument("task_id is empty".into()));
        }
        let body = Body::Task(TaskBody {
            task_id: task_id.clone(),
            title: title.into(),
            state,
            due_at,
            owner,
            depends_on: vec![],
        });
        // Confidence is 1.0 — caller is asserting state directly, not extracting.
        self.remember(body, Some(task_id), 1.0).await
    }

    /// Phase 2: Read all current task states for this namespace.
    ///
    /// Issues a `match_all` query against the compacted task topic and filters
    /// by namespace + (optionally) state. Order is unspecified — caller sorts.
    pub async fn current_tasks(
        &self,
        state_filter: Option<TaskState>,
    ) -> Result<Vec<TaskBody>> {
        let topic = self.inner.layout.typed(MemoryType::Task);
        let url = format!(
            "{}/_search",
            self.inner.chronik_api.trim_end_matches('/')
        );
        let body = serde_json::json!({
            "index": topic,
            "size": 10_000,
            "query": {"match_all": {}},
        });

        let resp = self
            .inner
            .http
            .post(&url)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let s = resp.status();
            // 404 on missing topic = no tasks yet.
            if s.as_u16() == 404 {
                return Ok(vec![]);
            }
            let txt = resp.text().await.unwrap_or_default();
            return Err(MemoryError::Http(format!("search {url} {s}: {txt}")));
        }

        let parsed: serde_json::Value = resp.json().await?;
        let hits = parsed
            .pointer("/hits/hits")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        let mut out: Vec<TaskBody> = Vec::with_capacity(hits.len());
        let want_namespace = self.inner.namespace.full.as_str();
        for h in hits {
            let source = match h.get("_source") {
                Some(s) => s,
                None => continue,
            };
            let envelope = match crate::recall::parse_envelope_from_source(source).ok() {
                Some(m) => m,
                None => continue,
            };
            // Filter to this namespace; the topic is shared across all
            // namespaces under the same tenant.
            if envelope.namespace != want_namespace {
                continue;
            }
            if envelope.tombstoned {
                continue;
            }
            if let Body::Task(t) = envelope.body {
                if let Some(want) = state_filter {
                    if t.state != want {
                        continue;
                    }
                }
                out.push(t);
            }
        }
        Ok(out)
    }

    /// AMS-3.7: Write (or supersede) a concept page for an entity.
    ///
    /// `entity_id` is the namespace-style id of the thing the page is about
    /// (`user:luis`, `property:rua_das_flores_42`, `documentary:our_planet`).
    /// The Kafka record key is `{namespace}|concept:{entity_id}` so log
    /// compaction keeps only the latest page per `(namespace, entity_id)`.
    ///
    /// Concept pages are produced by:
    /// 1. The future concept worker (a separate binary that consumes from
    ///    `mem.fact.*` / `mem.event.*` and synthesises pages on a regen
    ///    cadence — pending in AMS-3.7).
    /// 2. Direct callers — useful for importing pre-synthesised pages from
    ///    another system, or for tests / fixtures.
    pub async fn remember_concept(
        &self,
        body: crate::schema::ConceptBody,
        confidence: f32,
    ) -> Result<IngestAck> {
        let entity_id = body.entity_id.clone();
        if entity_id.is_empty() {
            return Err(MemoryError::InvalidArgument(
                "ConceptBody.entity_id must not be empty".into(),
            ));
        }
        let key = format!("concept:{entity_id}");
        let raw_topic = self.inner.layout.raw();
        let extractor_id = "concept-direct@1";
        let env = build_envelope(
            self.inner.layout.tenant(),
            &self.inner.namespace.full,
            crate::schema::Body::Concept(body),
            Some(key),
            confidence,
            extractor_id,
            &raw_topic,
        )?;
        self.produce_memory(&env).await
    }

    /// AMS-3.7: Read the latest concept page for an entity, or `None` if
    /// no page exists yet.
    ///
    /// Issues a `match_all` query against the compacted concept topic and
    /// filters by `(namespace, entity_id)`. Returns `Ok(None)` if no page
    /// has been written for this entity yet (typical for entities that
    /// haven't crossed the regen-cadence threshold).
    pub async fn concept(
        &self,
        entity_id: &str,
    ) -> Result<Option<crate::schema::ConceptBody>> {
        if entity_id.is_empty() {
            return Err(MemoryError::InvalidArgument(
                "concept(entity_id) must not be empty".into(),
            ));
        }
        let topic = self.inner.layout.concept();
        let url = format!(
            "{}/_search",
            self.inner.chronik_api.trim_end_matches('/')
        );
        // Query for the entity_id token — Tantivy will match docs whose
        // `_value` contains it. Concept-page bodies always include the
        // entity_id, so this hits at most a small set; we then filter by
        // namespace and entity_id exact match in-memory.
        let body = serde_json::json!({
            "index": topic,
            "size": 50,
            "query": {"match": {"_all": entity_id}},
        });
        let resp = self
            .inner
            .http
            .post(&url)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await?;
        if !resp.status().is_success() {
            // 404 on missing topic = no concept pages have been written yet.
            if resp.status().as_u16() == 404 {
                return Ok(None);
            }
            let s = resp.status();
            let txt = resp.text().await.unwrap_or_default();
            return Err(MemoryError::Http(format!("search {url} {s}: {txt}")));
        }
        let parsed: serde_json::Value = resp.json().await?;
        let hits = parsed
            .pointer("/hits/hits")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let want_namespace = self.inner.namespace.full.as_str();
        // Take the latest by `last_synthesized_at`, ignoring tombstones and
        // cross-namespace leakers. Compaction means the topic should already
        // contain only one record per (namespace, entity_id), but be
        // defensive in case the index has both old and new versions visible.
        let mut latest: Option<crate::schema::ConceptBody> = None;
        let mut latest_ts: Option<chrono::DateTime<chrono::Utc>> = None;
        for h in hits {
            let source = match h.get("_source") {
                Some(s) => s,
                None => continue,
            };
            let envelope = match crate::recall::parse_envelope_from_source(source).ok() {
                Some(m) => m,
                None => continue,
            };
            if envelope.namespace != want_namespace || envelope.tombstoned {
                continue;
            }
            if let crate::schema::Body::Concept(c) = envelope.body {
                if c.entity_id != entity_id {
                    continue;
                }
                if latest_ts.map_or(true, |t| c.last_synthesized_at > t) {
                    latest_ts = Some(c.last_synthesized_at);
                    latest = Some(c);
                }
            }
        }
        Ok(latest)
    }

    /// Emit an audit event for this `(tenant, namespace)` (AM-2.6).
    ///
    /// Best-effort: returns the [`IngestAck`] on success; logs and returns
    /// `Err` on Kafka failure. Callers in `chronik-server` handlers should
    /// log the error and continue — audit failure must never block the
    /// user-visible response.
    ///
    /// The audit topic (`mem.audit.{tenant}`) is created by
    /// [`Memory::init_namespace_full`]; out-of-band production of audit
    /// records into a missing topic fails cleanly with a Kafka error.
    pub async fn audit(&self, event: &crate::audit::AuditEvent) -> Result<IngestAck> {
        crate::audit::emit_audit(&self.inner.producer, event).await
    }

    /// Tombstone a memory by `key` (for compactable types) or by `memory_id`.
    /// Pass exactly one.
    ///
    /// Forget produces a *tombstone envelope* — a normal `MemoryRecord` with
    /// `tombstoned=true`, the same key (or `memory_id`) as the target,
    /// `version` bumped past the active record, and a minimal placeholder
    /// `body`. This is what the recall path's [`RecallBuilder`'s
    /// `passes_filters`](crate::recall::RecallBuilder) drops — without an
    /// envelope-shape tombstone, the search index would still surface the
    /// pre-forget record because Kafka null-value tombstones don't propagate
    /// to Tantivy. The Kafka null-value path is left to log compaction at
    /// the storage layer.
    #[instrument(skip(self), fields(kind = ?kind))]
    pub async fn forget(
        &self,
        kind: MemoryType,
        key: Option<&str>,
        memory_id: Option<&str>,
    ) -> Result<IngestAck> {
        // Validate the (key, memory_id) shape before constructing anything.
        let _ = resolve_tombstone_target(
            self.inner.layout.tenant(),
            &self.inner.namespace.full,
            kind,
            key,
            memory_id,
        )?;
        let raw_topic = self.inner.layout.raw();
        let extractor_id = "tombstone@1";
        // Body is a minimal valid placeholder per type — the envelope is
        // tombstoned so contents won't surface in recall, but the field is
        // required by the schema and must round-trip serde correctly.
        let body = match kind {
            MemoryType::Fact => crate::schema::Body::Fact(crate::schema::FactBody {
                subject: "tombstone".into(),
                predicate: "tombstone".into(),
                object: serde_json::Value::Null,
                polarity: "tombstone".into(),
                text: String::new(),
            }),
            MemoryType::Event => crate::schema::Body::Event(crate::schema::EventBody {
                actor: "tombstone".into(),
                verb: "tombstone".into(),
                object: None,
                channel: None,
                context: None,
                ts: chrono::Utc::now(),
            }),
            MemoryType::Instruction => {
                crate::schema::Body::Instruction(crate::schema::InstructionBody {
                    scope: "tombstone".into(),
                    rule: String::new(),
                    trigger: "tombstone".into(),
                    priority: 0,
                })
            }
            MemoryType::Task => crate::schema::Body::Task(crate::schema::TaskBody {
                task_id: key.unwrap_or("tombstone").to_string(),
                title: String::new(),
                state: crate::schema::TaskState::Cancelled,
                due_at: None,
                owner: None,
                depends_on: vec![],
            }),
            MemoryType::Concept => {
                // Concept tombstones aren't supported yet — concept pages
                // are synthesized server-side and forgetting them means
                // re-running synthesis with the relevant atomic memories
                // tombstoned, not direct deletion. Surface as a clear
                // error rather than producing a malformed envelope.
                return Err(MemoryError::InvalidArgument(
                    "forget(MemoryType::Concept) is not supported — \
                     forget the underlying facts/events instead and the next \
                     concept synthesis will reflect the deletion (AMS-3.7)"
                        .into(),
                ));
            }
        };
        // For compactable types, the envelope's `key` drives the Kafka record key.
        // For events, `key` is None and the Kafka record key is the memory_id.
        let env_key = match (kind, key, memory_id) {
            (MemoryType::Event, None, Some(_)) => None,
            (_, Some(k), _) => Some(k.to_string()),
            _ => None,
        };
        let mut env = build_envelope(
            self.inner.layout.tenant(),
            &self.inner.namespace.full,
            body,
            env_key,
            1.0,
            extractor_id,
            &raw_topic,
        )?;
        env.tombstoned = true;
        // Bump version so dedup_results_keep_max_score picks the tombstone
        // over any prior active record at the same (namespace, key).
        // u64::MAX is intentional — anything else risks colliding with a
        // future remember() write at version N+1.
        env.version = u64::MAX;
        // For event tombstones (memory_id-keyed), reuse the original memory_id
        // so the Kafka record key matches the original record's key.
        if kind == MemoryType::Event {
            if let Some(mid) = memory_id {
                env.memory_id = mid.to_string();
            }
        }
        self.produce_memory(&env).await
    }
}

/// Returned by [`Memory::ingest_with_extraction`]: contains both the raw produce
/// acks (one per input turn), the typed produce acks (one per produced memory),
/// and the extractions themselves for inspection / eval.
#[derive(Debug, Clone)]
pub struct ExtractionAck {
    /// Acks for each raw turn produced to `mem.raw.{ns}` (in input order).
    pub raw_acks: Vec<IngestAck>,
    /// Acks for each typed memory produced to `mem.{type}.{tenant}`.
    pub typed_acks: Vec<IngestAck>,
    /// The extractions themselves, in extractor output order.
    pub extracted: Vec<Extracted>,
}

/// Builder for [`Memory`].
///
/// Required fields: `chronik_kafka`, `chronik_api`, `namespace`. Extractor is
/// optional (without one, `ingest_with_extraction` is unavailable but `ingest`,
/// `remember`, `forget`, and `recall` work).
#[derive(Default)]
pub struct MemoryBuilder {
    kafka_brokers: Option<String>,
    chronik_api: Option<String>,
    namespace: Option<String>,
    extractor: Option<Arc<dyn Extractor>>,
    idempotency_capacity: Option<usize>,
    idempotency_ttl: Option<Duration>,
    request_timeout: Option<Duration>,
}

impl MemoryBuilder {
    /// Kafka bootstrap servers — accepts `kafka://host:port,...` or `host:port,...`.
    pub fn chronik_kafka(mut self, brokers: impl Into<String>) -> Self {
        self.kafka_brokers = Some(brokers.into());
        self
    }

    /// Chronik unified-API base URL — `http://host:6092` (no trailing slash).
    pub fn chronik_api(mut self, api: impl Into<String>) -> Self {
        self.chronik_api = Some(api.into());
        self
    }

    /// Namespace — typically `tenant:agent:bot:user:luis` style.
    pub fn namespace(mut self, ns: impl Into<String>) -> Self {
        self.namespace = Some(ns.into());
        self
    }

    /// Optional extractor used for sync-extract ingest and the Phase 2 worker.
    pub fn extractor<E: Extractor + 'static>(mut self, e: E) -> Self {
        self.extractor = Some(Arc::new(e));
        self
    }

    /// Same as [`extractor`](Self::extractor) but accepts an already-`Arc`-wrapped
    /// trait object. Used by [`MemoryRegistry`](crate::registry::MemoryRegistry)
    /// so multiple `Memory` instances can share one extractor without
    /// double-`Arc` indirection.
    pub fn extractor_arc(mut self, e: Arc<dyn Extractor>) -> Self {
        self.extractor = Some(e);
        self
    }

    /// Override default idempotency cache capacity (default 1024).
    pub fn idempotency_capacity(mut self, n: usize) -> Self {
        self.idempotency_capacity = Some(n);
        self
    }

    /// Override default idempotency TTL (default 5 minutes).
    pub fn idempotency_ttl(mut self, ttl: Duration) -> Self {
        self.idempotency_ttl = Some(ttl);
        self
    }

    /// Override the HTTP request timeout used for Chronik API calls (default 10s).
    pub fn request_timeout(mut self, ttl: Duration) -> Self {
        self.request_timeout = Some(ttl);
        self
    }

    /// Build the client. Returns `Err` if a required field is missing or the
    /// underlying clients fail to construct.
    pub async fn build(self) -> Result<Memory> {
        let brokers_raw = self
            .kafka_brokers
            .ok_or_else(|| MemoryError::Config("chronik_kafka(...) is required".into()))?;
        let brokers = strip_kafka_scheme(&brokers_raw).to_string();
        let chronik_api = self
            .chronik_api
            .ok_or_else(|| MemoryError::Config("chronik_api(...) is required".into()))?;
        let ns_raw = self
            .namespace
            .ok_or_else(|| MemoryError::Config("namespace(...) is required".into()))?;
        let namespace = NamespacePath::parse(ns_raw)?;
        let layout = TopicLayout::new(namespace.clone());

        // Producer config notes (Phase 1 dogfood findings):
        // - `enable.idempotence` intentionally NOT set: Kafka's per-PID sequence
        //   protocol conflicts with fresh-namespace-per-session and surfaces as
        //   `OutOfOrderSequenceNumber` against Chronik. SDK-level idempotency is
        //   handled by Kafka record key + the LRU cache.
        // - `compression.type=none`: rdkafka's LZ4 batches currently fail to
        //   decompress on Chronik with `LZ4 decompression failed: the offset to
        //   copy is not contained in the decompressed buffer`. Until that round-
        //   trip works, leave compression off. (Chronik issue, to be filed.)
        // - `acks=all` is sufficient for the durability guarantee.
        let producer: FutureProducer = ClientConfig::new()
            .set("bootstrap.servers", &brokers)
            .set("message.timeout.ms", "10000")
            .set("compression.type", "none")
            .set("acks", "all")
            .create()
            .map_err(|e| MemoryError::Kafka(format!("producer init: {e}")))?;

        let admin: AdminClient<DefaultClientContext> = ClientConfig::new()
            .set("bootstrap.servers", &brokers)
            .create()
            .map_err(|e| MemoryError::Kafka(format!("admin init: {e}")))?;

        let http = reqwest::Client::builder()
            .timeout(self.request_timeout.unwrap_or(Duration::from_secs(10)))
            .build()
            .map_err(|e| MemoryError::Http(format!("http client: {e}")))?;

        let cache = IdempotencyCache::new(
            self.idempotency_capacity
                .unwrap_or(DEFAULT_IDEMPOTENCY_CAPACITY),
            self.idempotency_ttl.unwrap_or(DEFAULT_IDEMPOTENCY_TTL),
        );

        Ok(Memory {
            inner: Arc::new(Inner {
                namespace,
                layout,
                chronik_api,
                kafka_brokers: brokers,
                producer,
                admin,
                http,
                extractor: self.extractor,
                idempotency: Mutex::new(cache),
            }),
        })
    }
}

fn strip_kafka_scheme(s: &str) -> &str {
    s.strip_prefix("kafka://").unwrap_or(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn build_requires_kafka_api_namespace() {
        // Missing namespace.
        let err = Memory::builder()
            .chronik_kafka("localhost:9092")
            .chronik_api("http://localhost:6092")
            .build()
            .await
            .unwrap_err();
        assert!(matches!(err, MemoryError::Config(_)));
    }

    #[tokio::test]
    async fn build_strips_kafka_scheme() {
        // We can't actually construct a real producer pointing at nothing in
        // tests, but rdkafka init is lazy enough that the broker isn't contacted
        // until first send. So we just verify build() succeeds.
        let mem = Memory::builder()
            .chronik_kafka("kafka://localhost:9092")
            .chronik_api("http://localhost:6092")
            .namespace("acme:agent:test")
            .build()
            .await;
        // build() should succeed (rdkafka producer init is offline).
        assert!(mem.is_ok(), "build returned: {:?}", mem.err());
    }

    #[tokio::test]
    async fn topic_layout_accessible() {
        let mem = Memory::builder()
            .chronik_kafka("localhost:9092")
            .chronik_api("http://localhost:6092")
            .namespace("acme:agent:test")
            .build()
            .await
            .unwrap();
        assert_eq!(mem.tenant(), "acme");
        assert_eq!(mem.topic_layout().raw(), "mem.raw.acme.agent.test");
        assert_eq!(
            mem.topic_layout().typed(MemoryType::Fact),
            "mem.fact.acme"
        );
    }

    #[test]
    fn strip_scheme_works() {
        assert_eq!(strip_kafka_scheme("kafka://localhost:9092"), "localhost:9092");
        assert_eq!(strip_kafka_scheme("localhost:9092"), "localhost:9092");
        assert_eq!(
            strip_kafka_scheme("kafka://a:9092,b:9092"),
            "a:9092,b:9092"
        );
    }

    /// AMS-2.6: in-process idempotency cache deduplicates same-content turns.
    /// Pure offline check — verifies the LRU path before the Kafka produce.
    #[tokio::test]
    async fn idempotency_cache_dedups_same_content() {
        let mem = Memory::builder()
            .chronik_kafka("localhost:9092")
            .chronik_api("http://localhost:6092")
            .namespace("acme:agent:test")
            .build()
            .await
            .unwrap();

        let turn = Turn {
            role: "user".into(),
            content: "I want a 3BR in Lapa".into(),
            ts: None,
            channel: None,
            external_id: None,
        };
        // First insert: not a duplicate.
        assert!(!mem.dedup_state().check_and_insert(&turn));
        // Second insert with same content → duplicate.
        assert!(mem.dedup_state().check_and_insert(&turn));
        assert_eq!(mem.dedup_state().len(), 1);
    }

    /// AMS-2.6: explicit `external_id` produces a different cache key than
    /// the auto SHA-256 hash of `(namespace, role, content)`.
    #[tokio::test]
    async fn idempotency_external_id_dominates_content_hash() {
        let mem = Memory::builder()
            .chronik_kafka("localhost:9092")
            .chronik_api("http://localhost:6092")
            .namespace("acme:agent:test")
            .build()
            .await
            .unwrap();

        let with_eid = Turn {
            role: "user".into(),
            content: "same content".into(),
            ts: None,
            channel: None,
            external_id: Some("wa-msg-001".into()),
        };
        let without_eid = Turn {
            external_id: None,
            ..with_eid.clone()
        };
        // External_id and content-hash produce DIFFERENT keys → both
        // count as fresh inserts.
        assert!(!mem.dedup_state().check_and_insert(&with_eid));
        assert!(!mem.dedup_state().check_and_insert(&without_eid));
        assert_eq!(mem.dedup_state().len(), 2);
    }

    /// AMS-2.6: namespaces partition the cache — same content in different
    /// namespaces is NOT a duplicate.
    #[tokio::test]
    async fn idempotency_namespace_isolation() {
        let mem_a = Memory::builder()
            .chronik_kafka("localhost:9092")
            .chronik_api("http://localhost:6092")
            .namespace("acme:agent:a")
            .build()
            .await
            .unwrap();
        let mem_b = Memory::builder()
            .chronik_kafka("localhost:9092")
            .chronik_api("http://localhost:6092")
            .namespace("acme:agent:b")
            .build()
            .await
            .unwrap();

        let turn = Turn {
            role: "user".into(),
            content: "hello".into(),
            ts: None,
            channel: None,
            external_id: None,
        };
        // Each namespace's cache is independent (different Memory instances)
        // AND the content-hash key includes the namespace, so even a shared
        // cache would dedup them as different.
        assert!(!mem_a.dedup_state().check_and_insert(&turn));
        assert!(!mem_b.dedup_state().check_and_insert(&turn));
    }
}
