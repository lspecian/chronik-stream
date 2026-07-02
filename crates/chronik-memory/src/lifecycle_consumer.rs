//! Kafka consumer that runs [`SemanticDedup`] against `mem.fact.{tenant}`
//! writes as they land (AM-2.3 lifecycle-controller MVP).
//!
//! # Scope
//!
//! - Reads new `MemoryRecord`s from `mem.fact.{tenant}`.
//! - Maintains an in-memory [`CandidateStore`] keyed by
//!   `(namespace, subject, predicate)` → `Vec<MemoryRecord>`. The store is
//!   the "existing memories" set passed to [`SemanticDedup::decide`].
//! - Calls [`SemanticDedup::decide`] on every new fact. Logs the outcome
//!   and increments a per-tenant counter surfaced through
//!   [`LifecycleStats`].
//!
//! # Out of scope (deliberate MVP cutoffs)
//!
//! - **Producing tombstones back to Kafka.** [`DedupDecision::Drop`] and
//!   `Supersede` currently only get logged + counted. Emitting a
//!   compaction tombstone or a versioned upsert requires the same admin
//!   producer path the memory client uses; wiring that in without a
//!   thundering-herd of self-triggered decisions is a follow-up.
//! - **Recovery on restart.** Consumer group offsets are managed by
//!   rdkafka's `enable.auto.commit=true` default; on restart, the store
//!   rehydrates from beginning-of-log (the topic is compacted, so the
//!   backlog is finite). A snapshot cache is a future optimization.
//! - **Cross-tenant sharding.** One consumer handles all `mem.fact.*`
//!   topics via a regex subscription. Real deployments may want one
//!   consumer per shard; the [`spawn`] API is per-topic so callers can
//!   fan out today.
//!
//! # Testability
//!
//! The parse + apply + store logic is pure and doesn't touch Kafka. The
//! only Kafka-touching function is [`run_consumer`]; the rest is fully
//! unit-testable and covered by the `tests` module below.

use crate::embeddings::Embedder;
use crate::error::{MemoryError, Result};
use crate::extractor::Extracted;
use crate::lifecycle::{DedupDecision, SemanticDedup};
use crate::schema::{Body, MemoryRecord};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{debug, info, warn};

/// Config for [`run_consumer`] / [`spawn`].
#[derive(Debug, Clone)]
pub struct LifecycleConfig {
    /// Kafka bootstrap servers.
    pub kafka_brokers: String,
    /// Group id. Multiple replicas can share it — rdkafka picks one active
    /// reader per partition.
    pub group_id: String,
    /// Which `mem.fact.{tenant}` topic to subscribe. Callers wanting to
    /// watch every tenant should either fan out one consumer per topic
    /// or extend this to accept a `Vec<String>` (out of MVP scope).
    pub topic: String,
}

impl LifecycleConfig {
    /// Sensible defaults for `mem.fact.{tenant}`.
    pub fn new(kafka_brokers: impl Into<String>, topic: impl Into<String>) -> Self {
        Self {
            kafka_brokers: kafka_brokers.into(),
            group_id: "chronik-memory-lifecycle-consumer".to_string(),
            topic: topic.into(),
        }
    }

    /// Override consumer group id.
    pub fn with_group_id(mut self, id: impl Into<String>) -> Self {
        self.group_id = id.into();
        self
    }
}

/// One decoded record from a `mem.fact.*` topic.
#[derive(Debug, Clone)]
pub enum FactEvent {
    /// A new (or updated) fact envelope was produced.
    Upsert(MemoryRecord),
    /// A Kafka compaction tombstone (empty / null value). The consumer
    /// forgets any cached candidate with matching key.
    Tombstone { key: String },
}

/// Decode a `(key_bytes, value_bytes)` pair into a [`FactEvent`].
///
/// Rejects non-UTF-8 keys and JSON decode failures. Both `None` value and
/// empty-value are treated as tombstones (Kafka null == compaction removal).
pub fn parse_fact_record(
    key_bytes: &[u8],
    value_bytes: Option<&[u8]>,
) -> std::result::Result<FactEvent, ParseError> {
    let key = std::str::from_utf8(key_bytes)
        .map_err(|e| ParseError::KeyNotUtf8(e.to_string()))?
        .to_string();
    match value_bytes {
        None => Ok(FactEvent::Tombstone { key }),
        Some(v) if v.is_empty() => Ok(FactEvent::Tombstone { key }),
        Some(v) => {
            let record: MemoryRecord = serde_json::from_slice(v)
                .map_err(|e| ParseError::BadJson(e.to_string()))?;
            Ok(FactEvent::Upsert(record))
        }
    }
}

/// Errors from [`parse_fact_record`].
#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    /// Kafka record key was not valid UTF-8.
    #[error("mem.fact key not utf-8: {0}")]
    KeyNotUtf8(String),
    /// Value payload failed to decode as [`MemoryRecord`].
    #[error("mem.fact value JSON decode failed: {0}")]
    BadJson(String),
}

/// In-memory index of same-key candidates. Cheap to clone (`Arc`).
///
/// Keyed by `(namespace, subject, predicate)` — the same triple
/// [`SemanticDedup`] compares against. Only Fact records are stored;
/// events / instructions / tasks are ignored (dedup only makes sense for
/// facts today).
#[derive(Debug, Default, Clone)]
pub struct CandidateStore {
    inner: Arc<DashMap<CandidateKey, Vec<MemoryRecord>>>,
}

/// Composite key for the store.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CandidateKey {
    /// Full namespace path.
    pub namespace: String,
    /// Fact subject.
    pub subject: String,
    /// Fact predicate.
    pub predicate: String,
}

impl CandidateStore {
    /// Fresh empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of `(namespace, subject, predicate)` groups indexed.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// `true` if empty.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Insert a record. No-op for tombstoned records and non-fact bodies.
    pub fn insert(&self, record: &MemoryRecord) {
        if record.tombstoned {
            return;
        }
        let Body::Fact(f) = &record.body else { return };
        let key = CandidateKey {
            namespace: record.namespace.clone(),
            subject: f.subject.clone(),
            predicate: f.predicate.clone(),
        };
        self.inner.entry(key).or_default().push(record.clone());
    }

    /// Return all candidates that share `(namespace, subject, predicate)`
    /// with the given record. Clones because the callee ([`SemanticDedup`])
    /// operates on a borrowed slice and DashMap iterators would hold locks.
    pub fn candidates_for(&self, new: &MemoryRecord) -> Vec<MemoryRecord> {
        let Body::Fact(f) = &new.body else { return Vec::new() };
        let key = CandidateKey {
            namespace: new.namespace.clone(),
            subject: f.subject.clone(),
            predicate: f.predicate.clone(),
        };
        self.inner
            .get(&key)
            .map(|v| v.value().clone())
            .unwrap_or_default()
    }

    /// Forget every candidate whose Kafka key matches `key`.
    ///
    /// The store is indexed by `(namespace, subject, predicate)` not Kafka
    /// key, so we scan — acceptable at tenant scale, and tombstones are
    /// rare relative to writes.
    pub fn forget_key(&self, key: &str) {
        // Collect keys to update to avoid holding a shard write lock while
        // matching. Small allocation but keeps DashMap semantics honest.
        let mut to_drop: Vec<CandidateKey> = Vec::new();
        self.inner.iter_mut().for_each(|mut entry| {
            entry.value_mut().retain(|r| r.key.as_deref() != Some(key));
            if entry.value().is_empty() {
                to_drop.push(entry.key().clone());
            }
        });
        for k in to_drop {
            self.inner.remove(&k);
        }
    }
}

/// Running counters emitted by the consumer. Public + snapshot-friendly
/// so callers can plug them into their own Prometheus registry.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub struct LifecycleStats {
    /// Total records the consumer parsed successfully.
    pub records_processed: u64,
    /// New facts inserted into the [`CandidateStore`].
    pub facts_indexed: u64,
    /// Tombstones observed on the topic.
    pub tombstones_observed: u64,
    /// [`DedupDecision::Keep`] outcomes.
    pub keeps: u64,
    /// [`DedupDecision::Drop`] outcomes.
    pub drops: u64,
    /// [`DedupDecision::Supersede`] outcomes.
    pub supersedes: u64,
    /// Records dropped because parse failed.
    pub parse_errors: u64,
    /// Records dropped because [`SemanticDedup::decide`] errored.
    pub decide_errors: u64,
}

impl LifecycleStats {
    /// Total dedup actions (drops + supersedes). Handy for a single-line
    /// operational health metric ("how much duplication are we catching?").
    pub fn actions(&self) -> u64 {
        self.drops + self.supersedes
    }
}

// ─────────────────────────────────────────────────────────────────
// Decision emitter — writes tombstones for Drop / Supersede outcomes.
// Extracted behind a trait so tests can capture emissions without touching
// Kafka.
// ─────────────────────────────────────────────────────────────────

/// Sink for [`DedupDecision`] outcomes. Called by the consumer after each
/// `apply_event` that returns a non-`Keep` decision.
#[async_trait::async_trait]
pub trait DecisionEmitter: Send + Sync {
    /// Produce a tombstone (or supersede envelope) for the given decision.
    ///
    /// - `source_topic` — the topic the record came from (e.g. `mem.fact.acme`).
    /// - `record` — the record the consumer just observed (the one being
    ///   dropped/superseded). Emitter implementations use its `key`
    ///   (Kafka compaction key) to write the tombstone.
    async fn emit(
        &self,
        decision: &DedupDecision,
        source_topic: &str,
        record: &MemoryRecord,
    ) -> crate::error::Result<()>;
}

/// No-op emitter — logs the decision but does not touch Kafka. Default for
/// deployments that want the visibility of a running consumer without the
/// self-emit feedback path.
#[derive(Debug, Default, Clone)]
pub struct LoggingEmitter;

#[async_trait::async_trait]
impl DecisionEmitter for LoggingEmitter {
    async fn emit(
        &self,
        decision: &DedupDecision,
        source_topic: &str,
        record: &MemoryRecord,
    ) -> crate::error::Result<()> {
        info!(
            ?decision,
            source_topic = %source_topic,
            memory_id = %record.memory_id,
            key = ?record.key,
            "lifecycle: emit (logging only)"
        );
        Ok(())
    }
}

/// In-memory emitter used by tests. Every `emit` call is appended to a
/// shared `Vec` the test can inspect.
#[derive(Debug, Default, Clone)]
pub struct CapturingEmitter {
    /// Captured `(decision, source_topic, memory_id)` triples in call order.
    pub captured: Arc<parking_lot::Mutex<Vec<(DedupDecision, String, String)>>>,
}

impl CapturingEmitter {
    /// Fresh empty capture buffer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot the captured emissions in call order.
    pub fn snapshot(&self) -> Vec<(DedupDecision, String, String)> {
        self.captured.lock().clone()
    }
}

#[async_trait::async_trait]
impl DecisionEmitter for CapturingEmitter {
    async fn emit(
        &self,
        decision: &DedupDecision,
        source_topic: &str,
        record: &MemoryRecord,
    ) -> crate::error::Result<()> {
        self.captured.lock().push((
            decision.clone(),
            source_topic.to_string(),
            record.memory_id.clone(),
        ));
        Ok(())
    }
}

/// Kafka-backed emitter — writes a compaction tombstone (null payload) to
/// `source_topic` with the record's Kafka key. Kafka compaction subsequently
/// removes the record on the next segment merge; recall's `passes_filters`
/// path also drops tombstoned records at query time.
///
/// Requires the caller to construct an rdkafka `FutureProducer` (typically
/// shared with the main Memory client).
#[derive(Clone)]
pub struct KafkaEmitter {
    producer: rdkafka::producer::FutureProducer,
    timeout: std::time::Duration,
}

impl std::fmt::Debug for KafkaEmitter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KafkaEmitter")
            .field("timeout", &self.timeout)
            .finish()
    }
}

impl KafkaEmitter {
    /// Build with a shared producer + a produce timeout (5s is a sensible
    /// default; tombstones are tiny and should ack quickly).
    pub fn new(
        producer: rdkafka::producer::FutureProducer,
        timeout: std::time::Duration,
    ) -> Self {
        Self { producer, timeout }
    }
}

#[async_trait::async_trait]
impl DecisionEmitter for KafkaEmitter {
    async fn emit(
        &self,
        decision: &DedupDecision,
        source_topic: &str,
        record: &MemoryRecord,
    ) -> crate::error::Result<()> {
        use rdkafka::producer::FutureRecord;
        use rdkafka::util::Timeout;
        // Only Drop / Supersede warrant an emission. Keep is a no-op.
        if matches!(decision, DedupDecision::Keep) {
            return Ok(());
        }
        let Some(key) = record.key.as_deref() else {
            warn!(
                memory_id = %record.memory_id,
                "lifecycle: cannot emit tombstone — record has no Kafka key"
            );
            return Ok(());
        };
        // Null payload = Kafka compaction tombstone.
        let rec: FutureRecord<'_, str, [u8]> =
            FutureRecord::to(source_topic).key(key);
        self.producer
            .send(rec, Timeout::After(self.timeout))
            .await
            .map_err(|(e, _)| {
                crate::error::MemoryError::Kafka(format!(
                    "lifecycle: emit tombstone to {source_topic}: {e}"
                ))
            })?;
        debug!(
            ?decision,
            source_topic = %source_topic,
            key = %key,
            "lifecycle: tombstone emitted"
        );
        Ok(())
    }
}

/// Convert a `MemoryRecord` into the [`Extracted`] shape that
/// [`SemanticDedup::decide`] expects. Preserves body, key, and confidence;
/// synth `source_indexes = vec![0]` because it doesn't take part in the
/// dedup decision.
pub fn as_extracted(record: &MemoryRecord) -> Extracted {
    Extracted {
        body: record.body.clone(),
        key: record.key.clone(),
        confidence: record.confidence,
        source_indexes: vec![0],
    }
}

/// Apply one [`FactEvent`] to the store + dedup + counters. Returns the
/// [`DedupDecision`] the caller can then act on (produce tombstones, etc.).
///
/// This is the entire correctness envelope of the consumer — the rest is
/// Kafka plumbing.
pub async fn apply_event<E: Embedder + Clone>(
    dedup: &SemanticDedup<E>,
    store: &CandidateStore,
    stats: &mut LifecycleStats,
    event: FactEvent,
) -> Result<Option<DedupDecision>> {
    stats.records_processed += 1;
    match event {
        FactEvent::Tombstone { key } => {
            stats.tombstones_observed += 1;
            store.forget_key(&key);
            debug!(key = %key, "lifecycle: tombstone applied");
            Ok(None)
        }
        FactEvent::Upsert(record) => {
            let extracted = as_extracted(&record);
            let existing = store.candidates_for(&record);
            let decision = dedup.decide(&extracted, &existing).await?;
            match &decision {
                DedupDecision::Keep => stats.keeps += 1,
                DedupDecision::Drop { .. } => stats.drops += 1,
                DedupDecision::Supersede { .. } => stats.supersedes += 1,
            }
            debug!(
                memory_id = %record.memory_id,
                decision = ?decision,
                "lifecycle: dedup decided"
            );
            // Always index; even records the dedup thinks are dupes remain
            // candidates for FUTURE incoming records (an older duplicate can
            // still be relevant context for a not-yet-seen third insertion).
            store.insert(&record);
            stats.facts_indexed += 1;
            Ok(Some(decision))
        }
    }
}

/// Spawn a background task that runs the lifecycle consumer forever.
/// Retries transient errors with a 5s backoff.
///
/// The `emitter` argument is called for every Drop / Supersede decision.
/// Pass [`LoggingEmitter`] for observability without side effects, or a
/// [`KafkaEmitter`] to actually produce compaction tombstones.
///
/// Returns the JoinHandle so callers can await + tear down gracefully.
pub fn spawn<E>(
    config: LifecycleConfig,
    dedup: Arc<SemanticDedup<E>>,
    store: CandidateStore,
    stats: Arc<parking_lot::Mutex<LifecycleStats>>,
    emitter: Arc<dyn DecisionEmitter>,
) -> tokio::task::JoinHandle<()>
where
    E: Embedder + Clone + Send + Sync + 'static,
{
    tokio::spawn(async move {
        loop {
            match run_consumer(&config, &dedup, &store, &stats, emitter.as_ref()).await {
                Ok(()) => {
                    info!("mem.fact consumer exited cleanly");
                    return;
                }
                Err(e) => {
                    warn!(error = %e, "mem.fact consumer errored, retrying in 5s");
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
            }
        }
    })
}

/// The rdkafka-backed consumer loop. Public so integration tests can drive
/// it directly.
pub async fn run_consumer<E>(
    config: &LifecycleConfig,
    dedup: &SemanticDedup<E>,
    store: &CandidateStore,
    stats: &Arc<parking_lot::Mutex<LifecycleStats>>,
    emitter: &dyn DecisionEmitter,
) -> Result<()>
where
    E: Embedder + Clone,
{
    use rdkafka::consumer::{Consumer, StreamConsumer};
    use rdkafka::message::Message;
    use rdkafka::ClientConfig;

    let consumer: StreamConsumer = ClientConfig::new()
        .set("bootstrap.servers", &config.kafka_brokers)
        .set("group.id", &config.group_id)
        .set("enable.auto.commit", "true")
        .set("auto.offset.reset", "earliest")
        .create()
        .map_err(|e| MemoryError::Kafka(format!("lifecycle consumer: create client: {e}")))?;

    consumer.subscribe(&[&config.topic]).map_err(|e| {
        MemoryError::Kafka(format!("lifecycle consumer: subscribe {}: {e}", config.topic))
    })?;

    info!(
        topic = %config.topic,
        brokers = %config.kafka_brokers,
        group_id = %config.group_id,
        "mem.fact lifecycle consumer started"
    );

    loop {
        match consumer.recv().await {
            Ok(msg) => {
                let key_bytes = msg.key().unwrap_or(&[]);
                let value_bytes = msg.payload();
                match parse_fact_record(key_bytes, value_bytes) {
                    Ok(event) => {
                        // Snapshot + release the guard BEFORE the await, then
                        // fold the local delta back into shared state
                        // afterwards. `MutexGuard` isn't `Send`, so any
                        // guard held across an .await breaks tokio::spawn's
                        // `Future: Send` requirement.
                        let mut local_stats: LifecycleStats = {
                            let s = stats.lock();
                            s.clone()
                        };
                        // Keep a reference to the source record so we can
                        // pass it to the emitter after apply_event consumes
                        // the event.
                        let source_record: Option<MemoryRecord> = match &event {
                            FactEvent::Upsert(r) => Some(r.clone()),
                            FactEvent::Tombstone { .. } => None,
                        };
                        let result =
                            apply_event(dedup, store, &mut local_stats, event).await;
                        match result {
                            Ok(Some(decision)) => {
                                *stats.lock() = local_stats;
                                // Emit for Drop / Supersede outcomes. Keep
                                // is a no-op — no tombstone needed. Emit
                                // failure is logged and swallowed; the
                                // record stays in the topic + the next
                                // consumer pass may retry.
                                if !matches!(decision, DedupDecision::Keep) {
                                    if let Some(record) = source_record {
                                        if let Err(e) = emitter
                                            .emit(&decision, &config.topic, &record)
                                            .await
                                        {
                                            warn!(
                                                error = %e,
                                                memory_id = %record.memory_id,
                                                "lifecycle: emit failed"
                                            );
                                        }
                                    }
                                }
                            }
                            Ok(None) => {
                                *stats.lock() = local_stats;
                            }
                            Err(e) => {
                                let mut s = stats.lock();
                                s.decide_errors += 1;
                                warn!(
                                    error = %e,
                                    partition = msg.partition(),
                                    offset = msg.offset(),
                                    "lifecycle: dedup decide failed"
                                );
                            }
                        }
                    }
                    Err(e) => {
                        let mut s = stats.lock();
                        s.parse_errors += 1;
                        warn!(
                            error = %e,
                            partition = msg.partition(),
                            offset = msg.offset(),
                            "lifecycle: skipping unparseable record"
                        );
                    }
                }
            }
            Err(e) => {
                return Err(MemoryError::Kafka(format!(
                    "lifecycle consumer: recv: {e}"
                )));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embeddings::Embedder;
    use crate::lifecycle::SemanticDedup;
    use crate::schema::{Body, FactBody, Source};
    use async_trait::async_trait;
    use chrono::Utc;
    use std::sync::Arc as StdArc;

    #[derive(Debug, Clone)]
    struct DeterministicEmbedder;

    /// Every text embeds to a unit vector whose first component is
    /// `sum_of_char_codes % 100 / 100.0` and second component is
    /// `sqrt(1 - first^2)`. Similar texts share character codes and thus
    /// get near-identical embeddings — good enough for unit tests.
    #[async_trait]
    impl Embedder for DeterministicEmbedder {
        async fn embed(&self, text: &str) -> Result<Vec<f32>> {
            let sum: u32 = text.chars().map(|c| c as u32).sum();
            let a = ((sum % 100) as f32) / 100.0;
            let b = (1.0 - a * a).sqrt();
            Ok(vec![a, b])
        }

        async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            let mut out = Vec::with_capacity(texts.len());
            for t in texts {
                out.push(self.embed(t).await?);
            }
            Ok(out)
        }

        fn id(&self) -> &str {
            "deterministic-test"
        }
    }

    fn fact_record(
        memory_id: &str,
        namespace: &str,
        subject: &str,
        predicate: &str,
        text: &str,
        confidence: f32,
    ) -> MemoryRecord {
        MemoryRecord {
            memory_id: memory_id.into(),
            tenant_id: namespace
                .split(':')
                .next()
                .unwrap_or(namespace)
                .to_string(),
            namespace: namespace.into(),
            key: Some(format!("{subject}|{predicate}")),
            version: 1,
            created_at: Utc::now(),
            valid_from: Utc::now(),
            valid_to: None,
            confidence,
            source: Source {
                topic: "mem.fact.acme".into(),
                offsets: vec![0],
                extractor: "test".into(),
            },
            tombstoned: false,
            body: Body::Fact(FactBody {
                subject: subject.into(),
                predicate: predicate.into(),
                object: serde_json::Value::Null,
                polarity: "asserted".into(),
                text: text.into(),
            }),
        }
    }

    #[test]
    fn parse_upsert_happy_path() {
        let record = fact_record("id-1", "acme:agent:bot:user:luis", "user", "budget", "hi", 1.0);
        let bytes = serde_json::to_vec(&record).unwrap();
        let event = parse_fact_record(b"user|budget", Some(&bytes)).unwrap();
        match event {
            FactEvent::Upsert(r) => assert_eq!(r.memory_id, "id-1"),
            _ => panic!("expected Upsert"),
        }
    }

    #[test]
    fn parse_tombstone_none_value() {
        let event = parse_fact_record(b"user|budget", None).unwrap();
        match event {
            FactEvent::Tombstone { key } => assert_eq!(key, "user|budget"),
            _ => panic!("expected Tombstone"),
        }
    }

    #[test]
    fn parse_tombstone_empty_value() {
        let event = parse_fact_record(b"user|budget", Some(b"")).unwrap();
        assert!(matches!(event, FactEvent::Tombstone { .. }));
    }

    #[test]
    fn parse_rejects_non_utf8_key() {
        let err = parse_fact_record(&[0xff], None).unwrap_err();
        assert!(matches!(err, ParseError::KeyNotUtf8(_)));
    }

    #[test]
    fn parse_rejects_bad_json() {
        let err = parse_fact_record(b"k", Some(b"not-json")).unwrap_err();
        assert!(matches!(err, ParseError::BadJson(_)));
    }

    #[test]
    fn candidate_store_groups_by_ns_subject_predicate() {
        let store = CandidateStore::new();
        let a = fact_record("a", "acme:ns", "user", "budget", "budget $4000", 1.0);
        let b = fact_record("b", "acme:ns", "user", "budget", "budget four thousand", 0.9);
        let c = fact_record("c", "acme:ns", "user", "color", "loves blue", 1.0);
        store.insert(&a);
        store.insert(&b);
        store.insert(&c);
        assert_eq!(store.len(), 2, "budget+color -> 2 groups");
        let cands = store.candidates_for(&a);
        assert_eq!(cands.len(), 2, "user|budget group has 2 candidates");
    }

    #[test]
    fn candidate_store_skips_tombstoned_records() {
        let store = CandidateStore::new();
        let mut a = fact_record("a", "acme:ns", "user", "budget", "budget $4000", 1.0);
        a.tombstoned = true;
        store.insert(&a);
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn candidate_store_forget_key_removes_matching_and_prunes_empty_groups() {
        let store = CandidateStore::new();
        let a = fact_record("a", "acme:ns", "user", "budget", "hi", 1.0);
        let c = fact_record("c", "acme:ns", "user", "color", "blue", 1.0);
        store.insert(&a);
        store.insert(&c);
        assert_eq!(store.len(), 2);
        store.forget_key("user|budget");
        assert_eq!(store.len(), 1, "empty group pruned");
        // color group still there.
        assert_eq!(store.candidates_for(&c).len(), 1);
    }

    #[tokio::test]
    async fn apply_event_keep_on_empty_store() {
        let dedup = SemanticDedup::new(DeterministicEmbedder);
        let store = CandidateStore::new();
        let mut stats = LifecycleStats::default();
        let record = fact_record("a", "acme:ns", "user", "budget", "budget $4000", 1.0);
        let decision =
            apply_event(&dedup, &store, &mut stats, FactEvent::Upsert(record))
                .await
                .unwrap()
                .unwrap();
        assert!(matches!(decision, DedupDecision::Keep));
        assert_eq!(stats.records_processed, 1);
        assert_eq!(stats.facts_indexed, 1);
        assert_eq!(stats.keeps, 1);
        assert_eq!(store.len(), 1, "store should now hold the new fact");
    }

    #[tokio::test]
    async fn apply_event_drop_on_near_duplicate_with_lower_confidence() {
        // The deterministic embedder returns the same vector for identical
        // texts → similarity = 1.0 → above the 0.97 default threshold.
        let dedup = SemanticDedup::new(DeterministicEmbedder);
        let store = CandidateStore::new();
        let mut stats = LifecycleStats::default();
        // Seed the store with a high-confidence record.
        let seed = fact_record("a", "acme:ns", "user", "budget", "budget $4000", 1.0);
        store.insert(&seed);
        // New record has LOWER confidence — should Drop.
        let new = fact_record("b", "acme:ns", "user", "budget", "budget $4000", 0.7);
        let decision =
            apply_event(&dedup, &store, &mut stats, FactEvent::Upsert(new))
                .await
                .unwrap()
                .unwrap();
        match decision {
            DedupDecision::Drop { existing_id, .. } => assert_eq!(existing_id, "a"),
            other => panic!("expected Drop, got {other:?}"),
        }
        assert_eq!(stats.drops, 1);
    }

    #[tokio::test]
    async fn apply_event_supersede_on_higher_confidence_dup() {
        let dedup = SemanticDedup::new(DeterministicEmbedder);
        let store = CandidateStore::new();
        let mut stats = LifecycleStats::default();
        let seed = fact_record("a", "acme:ns", "user", "budget", "budget $4000", 0.6);
        store.insert(&seed);
        let new = fact_record("b", "acme:ns", "user", "budget", "budget $4000", 0.95);
        let decision =
            apply_event(&dedup, &store, &mut stats, FactEvent::Upsert(new))
                .await
                .unwrap()
                .unwrap();
        match decision {
            DedupDecision::Supersede { existing_id, .. } => assert_eq!(existing_id, "a"),
            other => panic!("expected Supersede, got {other:?}"),
        }
        assert_eq!(stats.supersedes, 1);
    }

    #[tokio::test]
    async fn apply_event_tombstone_removes_from_store_and_counts() {
        let dedup = SemanticDedup::new(DeterministicEmbedder);
        let store = CandidateStore::new();
        let mut stats = LifecycleStats::default();
        let seed = fact_record("a", "acme:ns", "user", "budget", "hi", 1.0);
        store.insert(&seed);
        let decision = apply_event(
            &dedup,
            &store,
            &mut stats,
            FactEvent::Tombstone {
                key: "user|budget".into(),
            },
        )
        .await
        .unwrap();
        assert!(decision.is_none(), "tombstones return None (no dedup call)");
        assert_eq!(stats.tombstones_observed, 1);
        assert_eq!(store.len(), 0);
    }

    #[tokio::test]
    async fn distinct_predicates_do_not_dedup_each_other() {
        let dedup = SemanticDedup::new(DeterministicEmbedder);
        let store = CandidateStore::new();
        let mut stats = LifecycleStats::default();
        store.insert(&fact_record("a", "acme:ns", "user", "budget", "text", 1.0));
        let new = fact_record("b", "acme:ns", "user", "color", "text", 1.0);
        let decision =
            apply_event(&dedup, &store, &mut stats, FactEvent::Upsert(new))
                .await
                .unwrap()
                .unwrap();
        assert!(matches!(decision, DedupDecision::Keep));
    }

    #[test]
    fn lifecycle_stats_actions_sum() {
        let s = LifecycleStats {
            drops: 3,
            supersedes: 2,
            ..Default::default()
        };
        assert_eq!(s.actions(), 5);
    }

    #[test]
    fn config_defaults() {
        let c = LifecycleConfig::new("localhost:9092", "mem.fact.acme");
        assert_eq!(c.group_id, "chronik-memory-lifecycle-consumer");
        assert_eq!(c.topic, "mem.fact.acme");
        let c2 = c.with_group_id("custom");
        assert_eq!(c2.group_id, "custom");
    }

    // ─── DecisionEmitter tests ─────────────────────────────────────

    #[tokio::test]
    async fn capturing_emitter_records_drop() {
        let emitter = CapturingEmitter::new();
        let rec = fact_record("m", "acme:ns", "user", "budget", "hi", 1.0);
        let decision = DedupDecision::Drop {
            existing_id: "prev".into(),
            similarity: 0.99,
        };
        emitter
            .emit(&decision, "mem.fact.acme", &rec)
            .await
            .unwrap();
        let snap = emitter.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].1, "mem.fact.acme");
        assert_eq!(snap[0].2, "m");
        assert!(matches!(snap[0].0, DedupDecision::Drop { .. }));
    }

    #[tokio::test]
    async fn logging_emitter_is_a_noop() {
        // LoggingEmitter never fails and captures nothing external.
        let emitter = LoggingEmitter::default();
        let rec = fact_record("m", "acme:ns", "user", "budget", "hi", 1.0);
        emitter
            .emit(
                &DedupDecision::Supersede {
                    existing_id: "p".into(),
                    similarity: 0.99,
                },
                "mem.fact.acme",
                &rec,
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn capturing_emitter_records_supersede() {
        let emitter = CapturingEmitter::new();
        let rec = fact_record("newer", "acme:ns", "user", "budget", "hi", 1.0);
        let decision = DedupDecision::Supersede {
            existing_id: "older".into(),
            similarity: 0.98,
        };
        emitter
            .emit(&decision, "mem.fact.acme", &rec)
            .await
            .unwrap();
        assert_eq!(emitter.snapshot().len(), 1);
    }

    #[tokio::test]
    async fn capturing_emitter_across_multiple_calls() {
        let emitter = CapturingEmitter::new();
        let a = fact_record("a", "acme:ns", "user", "budget", "hi", 1.0);
        let b = fact_record("b", "acme:ns", "user", "color", "blue", 1.0);
        emitter
            .emit(
                &DedupDecision::Drop {
                    existing_id: "x".into(),
                    similarity: 0.99,
                },
                "mem.fact.acme",
                &a,
            )
            .await
            .unwrap();
        emitter
            .emit(
                &DedupDecision::Supersede {
                    existing_id: "y".into(),
                    similarity: 0.98,
                },
                "mem.fact.beta",
                &b,
            )
            .await
            .unwrap();
        let snap = emitter.snapshot();
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0].2, "a");
        assert_eq!(snap[0].1, "mem.fact.acme");
        assert_eq!(snap[1].2, "b");
        assert_eq!(snap[1].1, "mem.fact.beta");
    }
}
