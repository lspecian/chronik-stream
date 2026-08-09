//! In-memory index from `memory_id → Source` for `GET /memory/v1/{id}/source`
//! (AM-2.6 provenance walk).
//!
//! # What this indexes
//!
//! For every `MemoryRecord` observed on a `mem.{type}.{tenant}` topic, the
//! index stores:
//!
//! - `memory_id` (key)
//! - `MemoryRecord.source` — the caller-visible provenance pointer:
//!   `(topic, offsets[], extractor)`, where `topic` is a `mem.raw.*` topic
//!   and `offsets` name specific turns within it.
//!
//! # What this does NOT do
//!
//! - It does not fetch the raw conversation turns. That's a Kafka read
//!   the source-endpoint handler performs on demand (or the caller can
//!   do themselves against the returned pointer).
//! - It does not enforce tenant isolation. The `authorize_namespace`
//!   middleware already scopes queries — this index just holds a
//!   `HashMap` and trusts callers to check first.
//!
//! # Populating
//!
//! [`spawn`] runs a background rdkafka consumer with a topic-regex
//! subscription that matches every `mem.fact.*`, `mem.event.*`,
//! `mem.instruction.*`, `mem.task.*`, and `mem.concept.*` topic. Records
//! are decoded via the shared [`parse_memory_record`] helper (also usable
//! by tests without any Kafka involvement).

use crate::error::MemoryError;
use crate::schema::{MemoryRecord, Source};
use dashmap::DashMap;
use std::sync::Arc;
use tracing::{debug, info, warn};

/// Topics-of-interest regex for the rdkafka consumer.
///
/// Matches typed-memory topics: `mem.<type>.<tenant>` for the five known
/// memory types + the compacted task-current view. Excludes `mem.raw.*`
/// (raw conversation turns), `mem.audit.*`, `mem.tenants`, `mem.feedback.*`,
/// and any future non-fact topics.
pub const INDEX_TOPICS_REGEX: &str =
    r"^mem\.(fact|event|instruction|task|task\.current|concept)\.[^.]+$";

/// In-memory `memory_id → Source` index. Cheap to clone (`Arc` interior).
#[derive(Debug, Default, Clone)]
pub struct MemoryIndex {
    inner: Arc<DashMap<String, Source>>,
}

impl MemoryIndex {
    /// Fresh empty index.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of memory_ids indexed.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// `true` if empty.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Look up the [`Source`] pointer for `memory_id`. `None` if not indexed.
    pub fn lookup(&self, memory_id: &str) -> Option<Source> {
        self.inner.get(memory_id).map(|v| v.value().clone())
    }

    /// Insert (or replace) the pointer for `memory_id`.
    ///
    /// No-op for tombstoned records — tombstones are handled via
    /// [`remove`](Self::remove) driven by the parser.
    pub fn insert(&self, record: &MemoryRecord) {
        if record.tombstoned {
            return;
        }
        self.inner
            .insert(record.memory_id.clone(), record.source.clone());
    }

    /// Remove `memory_id` from the index. Returns whether it was present.
    ///
    /// Called on tombstones and via administrative endpoints (right-to-erasure).
    pub fn remove(&self, memory_id: &str) -> bool {
        self.inner.remove(memory_id).is_some()
    }

    /// Test helper: iterate over all `(memory_id, Source)` pairs.
    #[doc(hidden)]
    pub fn iter_snapshot(&self) -> Vec<(String, Source)> {
        self.inner
            .iter()
            .map(|kv| (kv.key().clone(), kv.value().clone()))
            .collect()
    }
}

/// One decoded record from a memory topic.
#[derive(Debug, Clone)]
pub enum IndexEvent {
    /// A new (or updated) memory envelope was produced.
    Upsert(MemoryRecord),
    /// The record was a tombstone. Use `memory_id` from the tombstone
    /// envelope OR the Kafka record key (they should agree — the caller
    /// picks whichever is available).
    Tombstone { memory_id: String },
}

/// Pure decoder: `(key_bytes, value_bytes)` → `IndexEvent`. Handles both
/// tombstone envelopes (`MemoryRecord.tombstoned=true`) and empty-value
/// records (Kafka compaction null).
pub fn parse_memory_record(
    key_bytes: &[u8],
    value_bytes: Option<&[u8]>,
) -> Result<IndexEvent, ParseError> {
    let key = std::str::from_utf8(key_bytes)
        .map_err(|e| ParseError::KeyNotUtf8(e.to_string()))?
        .to_string();

    match value_bytes {
        None => Ok(IndexEvent::Tombstone { memory_id: key }),
        Some(v) if v.is_empty() => Ok(IndexEvent::Tombstone { memory_id: key }),
        Some(v) => {
            let record: MemoryRecord = serde_json::from_slice(v)
                .map_err(|e| ParseError::BadJson(e.to_string()))?;
            if record.tombstoned {
                Ok(IndexEvent::Tombstone {
                    memory_id: record.memory_id,
                })
            } else {
                Ok(IndexEvent::Upsert(record))
            }
        }
    }
}

/// Apply one decoded event to the index. Pure lock-scoped mutation.
pub fn apply_event(index: &MemoryIndex, event: IndexEvent) {
    match event {
        IndexEvent::Upsert(record) => {
            let mid = record.memory_id.clone();
            index.insert(&record);
            debug!(memory_id = %mid, "MemoryIndex upsert");
        }
        IndexEvent::Tombstone { memory_id } => {
            let removed = index.remove(&memory_id);
            debug!(
                memory_id = %memory_id,
                was_present = removed,
                "MemoryIndex tombstone"
            );
        }
    }
}

/// Errors from [`parse_memory_record`].
#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    /// Kafka record key was not valid UTF-8.
    #[error("memory index key not utf-8: {0}")]
    KeyNotUtf8(String),
    /// Value payload failed to decode as [`MemoryRecord`].
    #[error("memory index value JSON decode failed: {0}")]
    BadJson(String),
}

// ─────────────────────────────────────────────────────────────────
// Live consumer.
// ─────────────────────────────────────────────────────────────────

/// Config for [`run_consumer`] / [`spawn`].
#[derive(Debug, Clone)]
pub struct IndexConsumerConfig {
    /// Kafka bootstrap servers.
    pub kafka_brokers: String,
    /// Consumer group id.
    pub group_id: String,
}

impl IndexConsumerConfig {
    /// Sensible defaults for `kafka_brokers=localhost:9092`.
    pub fn new(kafka_brokers: impl Into<String>) -> Self {
        Self {
            kafka_brokers: kafka_brokers.into(),
            group_id: "chronik-memory-index-consumer".to_string(),
        }
    }

    /// Override the consumer group id.
    pub fn with_group_id(mut self, id: impl Into<String>) -> Self {
        self.group_id = id.into();
        self
    }
}

/// Spawn a background task that runs the memory-index consumer forever.
pub fn spawn(
    config: IndexConsumerConfig,
    index: MemoryIndex,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match run_consumer(&config, &index).await {
                Ok(()) => {
                    info!("memory-index consumer exited cleanly");
                    return;
                }
                Err(e) => {
                    warn!(error = %e, "memory-index consumer errored, retrying in 5s");
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
            }
        }
    })
}

/// The rdkafka-backed consumer loop. Subscribes to every typed-memory topic
/// via [`INDEX_TOPICS_REGEX`].
pub async fn run_consumer(
    config: &IndexConsumerConfig,
    index: &MemoryIndex,
) -> Result<(), MemoryError> {
    use rdkafka::consumer::{Consumer, StreamConsumer};
    use rdkafka::message::Message;
    use rdkafka::ClientConfig;

    let consumer: StreamConsumer = ClientConfig::new()
        .set("bootstrap.servers", &config.kafka_brokers)
        .set("group.id", &config.group_id)
        .set("enable.auto.commit", "true")
        .set("auto.offset.reset", "earliest")
        .create()
        .map_err(|e| {
            MemoryError::Kafka(format!("memory-index consumer: create client: {e}"))
        })?;

    // Subscribe by regex — matches every current tenant's memory topics
    // plus any future tenants that appear later.
    consumer
        .subscribe(&[INDEX_TOPICS_REGEX])
        .map_err(|e| {
            MemoryError::Kafka(format!(
                "memory-index consumer: subscribe {INDEX_TOPICS_REGEX}: {e}"
            ))
        })?;

    info!(
        pattern = %INDEX_TOPICS_REGEX,
        brokers = %config.kafka_brokers,
        group_id = %config.group_id,
        "memory-index consumer started"
    );

    loop {
        match consumer.recv().await {
            Ok(msg) => {
                let key_bytes = msg.key().unwrap_or(&[]);
                let value_bytes = msg.payload();
                match parse_memory_record(key_bytes, value_bytes) {
                    Ok(event) => apply_event(index, event),
                    Err(e) => {
                        warn!(
                            error = %e,
                            partition = msg.partition(),
                            offset = msg.offset(),
                            "memory-index: skipping unparseable record"
                        );
                    }
                }
            }
            Err(e) => {
                return Err(MemoryError::Kafka(format!(
                    "memory-index consumer: recv: {e}"
                )));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{Body, FactBody, Source};
    use chrono::Utc;

    fn record(memory_id: &str, tombstoned: bool) -> MemoryRecord {
        MemoryRecord {
            memory_id: memory_id.into(),
            tenant_id: "acme".into(),
            namespace: "acme:ns".into(),
            key: Some("user|budget".into()),
            version: 1,
            created_at: Utc::now(),
            valid_from: Utc::now(),
            valid_to: None,
            confidence: 1.0,
            source: Source {
                topic: "mem.raw.acme.agent.bot.user.luis".into(),
                offsets: vec![127, 128],
                extractor: "anthropic-v3".into(), excerpt: None,
            },
            tombstoned,
            body: Body::Fact(FactBody {
                subject: "user".into(),
                predicate: "budget".into(),
                object: serde_json::Value::Null,
                polarity: "asserted".into(),
                text: "budget $4000".into(),
                speaker: "user".into(),
            }),
        }
    }

    #[test]
    fn parse_upsert_happy_path() {
        let r = record("id-1", false);
        let bytes = serde_json::to_vec(&r).unwrap();
        let event = parse_memory_record(b"id-1", Some(&bytes)).unwrap();
        match event {
            IndexEvent::Upsert(r) => assert_eq!(r.memory_id, "id-1"),
            _ => panic!("expected Upsert"),
        }
    }

    #[test]
    fn parse_tombstone_from_envelope_flag() {
        let r = record("id-1", true);
        let bytes = serde_json::to_vec(&r).unwrap();
        let event = parse_memory_record(b"id-1", Some(&bytes)).unwrap();
        match event {
            IndexEvent::Tombstone { memory_id } => assert_eq!(memory_id, "id-1"),
            other => panic!("expected Tombstone, got {other:?}"),
        }
    }

    #[test]
    fn parse_tombstone_from_null_value() {
        let event = parse_memory_record(b"id-1", None).unwrap();
        match event {
            IndexEvent::Tombstone { memory_id } => assert_eq!(memory_id, "id-1"),
            _ => panic!("expected Tombstone"),
        }
    }

    #[test]
    fn parse_tombstone_from_empty_value() {
        let event = parse_memory_record(b"id-1", Some(b"")).unwrap();
        assert!(matches!(event, IndexEvent::Tombstone { .. }));
    }

    #[test]
    fn parse_rejects_non_utf8_key() {
        let err = parse_memory_record(&[0xff], None).unwrap_err();
        assert!(matches!(err, ParseError::KeyNotUtf8(_)));
    }

    #[test]
    fn parse_rejects_bad_json() {
        let err = parse_memory_record(b"k", Some(b"nope")).unwrap_err();
        assert!(matches!(err, ParseError::BadJson(_)));
    }

    #[test]
    fn index_starts_empty() {
        let idx = MemoryIndex::new();
        assert_eq!(idx.len(), 0);
        assert!(idx.is_empty());
        assert!(idx.lookup("anything").is_none());
    }

    #[test]
    fn insert_records_the_source_pointer() {
        let idx = MemoryIndex::new();
        idx.insert(&record("id-1", false));
        assert_eq!(idx.len(), 1);
        let src = idx.lookup("id-1").expect("indexed");
        assert_eq!(src.topic, "mem.raw.acme.agent.bot.user.luis");
        assert_eq!(src.offsets, vec![127, 128]);
        assert_eq!(src.extractor, "anthropic-v3");
    }

    #[test]
    fn insert_skips_tombstoned_records() {
        let idx = MemoryIndex::new();
        idx.insert(&record("id-1", true));
        assert_eq!(idx.len(), 0);
    }

    #[test]
    fn remove_deletes_and_returns_presence() {
        let idx = MemoryIndex::new();
        idx.insert(&record("id-1", false));
        assert!(idx.remove("id-1"));
        assert!(!idx.remove("id-1"), "second remove returns false");
        assert_eq!(idx.len(), 0);
    }

    #[test]
    fn apply_event_upsert_and_tombstone_full_lifecycle() {
        let idx = MemoryIndex::new();
        // Upsert.
        let up = IndexEvent::Upsert(record("id-1", false));
        apply_event(&idx, up);
        assert!(idx.lookup("id-1").is_some());
        // Tombstone via parser (envelope flag).
        let r = record("id-1", true);
        let bytes = serde_json::to_vec(&r).unwrap();
        let ev = parse_memory_record(b"id-1", Some(&bytes)).unwrap();
        apply_event(&idx, ev);
        assert!(idx.lookup("id-1").is_none());
    }

    #[test]
    fn apply_event_tombstone_on_missing_is_noop() {
        let idx = MemoryIndex::new();
        apply_event(&idx, IndexEvent::Tombstone { memory_id: "ghost".into() });
        assert_eq!(idx.len(), 0);
    }

    #[test]
    fn upsert_of_existing_id_replaces_source() {
        let idx = MemoryIndex::new();
        let mut r = record("id-1", false);
        idx.insert(&r);
        r.source.topic = "mem.raw.other.topic".into();
        r.source.offsets = vec![999];
        idx.insert(&r);
        assert_eq!(idx.len(), 1);
        let src = idx.lookup("id-1").unwrap();
        assert_eq!(src.topic, "mem.raw.other.topic");
        assert_eq!(src.offsets, vec![999]);
    }

    #[test]
    fn config_defaults() {
        let c = IndexConsumerConfig::new("localhost:9092");
        assert_eq!(c.kafka_brokers, "localhost:9092");
        assert_eq!(c.group_id, "chronik-memory-index-consumer");
        let c2 = c.with_group_id("custom");
        assert_eq!(c2.group_id, "custom");
    }

    #[test]
    fn regex_matches_expected_topics_and_excludes_others() {
        let re = regex::Regex::new(INDEX_TOPICS_REGEX).unwrap();
        // Included.
        assert!(re.is_match("mem.fact.acme"));
        assert!(re.is_match("mem.event.beta"));
        assert!(re.is_match("mem.instruction.gamma"));
        assert!(re.is_match("mem.task.delta"));
        assert!(re.is_match("mem.task.current.delta"));
        assert!(re.is_match("mem.concept.acme"));
        // Excluded.
        assert!(!re.is_match("mem.raw.acme.agent.bot.user.luis"));
        assert!(!re.is_match("mem.audit.acme"));
        assert!(!re.is_match("mem.tenants"));
        assert!(!re.is_match("mem.feedback.acme"));
        assert!(!re.is_match("other.fact.acme"));
    }
}
