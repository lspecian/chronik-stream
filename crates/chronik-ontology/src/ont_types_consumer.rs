//! `ont.types.{tenant}` registry — the ObjectType-definitions consumer.
//!
//! 7th instance of the "consumer-maintained keyed in-memory index rebuilt from a
//! compacted Kafka topic" pattern. Structurally modeled on
//! `chronik-memory/src/mem_config_consumer.rs` (per-tenant topic regex,
//! tenant-derived-from-topic, versioned JSON envelope) with the
//! apply-outcome-enum + stats shape from `task_current_consumer.rs`.
//!
//! The five pieces: (a) [`OntTypeIndex`] (`Arc<DashMap>`), (b) [`OntTypeEvent`],
//! (c) the pure [`parse_ont_type_record`], (d) the pure [`apply_event`], (e) the
//! async [`run_consumer`] + [`spawn_ont_types_consumer`] retry loop.

use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use parking_lot::Mutex;
use rdkafka::config::ClientConfig;
use rdkafka::consumer::{Consumer, StreamConsumer};
use rdkafka::Message;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::object_type::ObjectType;

/// Regex matching every per-tenant ObjectType topic: `ont.types.{tenant}`
/// (tenant carries no dots). New tenants are auto-discovered by the subscription.
pub const ONT_TYPES_TOPICS_REGEX: &str = r"^ont\.types\.[^.]+$";

/// Composite key: an ObjectType is unique per (tenant, type_name).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OntTypeKey {
    pub tenant: String,
    pub type_name: String,
}

/// Registry: a cheaply-cloneable handle to the shared keyed index. Cloning
/// yields another handle to the SAME map (one for the consumer task, one for the
/// API state).
#[derive(Debug, Default, Clone)]
pub struct OntTypeIndex {
    types: Arc<DashMap<OntTypeKey, ObjectType>>,
}

impl OntTypeIndex {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn len(&self) -> usize {
        self.types.len()
    }
    pub fn is_empty(&self) -> bool {
        self.types.is_empty()
    }
    /// Look up one ObjectType by (tenant, type_name).
    pub fn get(&self, tenant: &str, type_name: &str) -> Option<ObjectType> {
        self.types
            .get(&OntTypeKey {
                tenant: tenant.to_string(),
                type_name: type_name.to_string(),
            })
            .map(|r| r.clone())
    }
    /// All ObjectTypes defined for a tenant.
    pub fn list_for_tenant(&self, tenant: &str) -> Vec<ObjectType> {
        self.types
            .iter()
            .filter(|e| e.key().tenant == tenant)
            .map(|e| e.value().clone())
            .collect()
    }
    fn upsert(&self, key: OntTypeKey, ty: ObjectType) {
        self.types.insert(key, ty);
    }
    fn remove(&self, key: &OntTypeKey) -> bool {
        self.types.remove(key).is_some()
    }
}

/// Wire envelope stored as the Kafka message value. Versioned for forward-compat.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OntTypeRecordEnvelope {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    pub object_type: ObjectType,
}
fn default_schema_version() -> u32 {
    1
}

/// A decoded registry event.
#[derive(Debug, Clone, PartialEq)]
pub enum OntTypeEvent {
    Upsert { tenant: String, object_type: ObjectType },
    Tombstone { tenant: String, type_name: String },
}

/// Outcome of applying an event (drives stats).
#[derive(Debug, Clone, PartialEq)]
pub enum OntTypeApply {
    Upserted,
    Removed,
    /// Tombstone for a key that was not present — a harmless no-op.
    RemovedMissing,
}

/// Decode errors.
#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("topic {0:?} is not a valid ont.types.{{tenant}} topic")]
    BadTopic(String),
    #[error("key is not valid UTF-8: {0}")]
    KeyNotUtf8(String),
    #[error("key (type_name) is empty")]
    EmptyKey,
    #[error("value is not valid JSON: {0}")]
    BadJson(String),
    #[error("type_name in key ({key:?}) does not match body ({body:?})")]
    KeyBodyMismatch { key: String, body: String },
}

/// Extract the tenant from an `ont.types.{tenant}` topic name.
fn tenant_from_topic(topic: &str) -> Result<&str, ParseError> {
    let rest = topic
        .strip_prefix("ont.types.")
        .ok_or_else(|| ParseError::BadTopic(topic.to_string()))?;
    if rest.is_empty() || rest.contains('.') {
        return Err(ParseError::BadTopic(topic.to_string()));
    }
    Ok(rest)
}

/// PURE decode: `(topic, key_bytes, value_bytes) -> OntTypeEvent`. No Kafka.
///
/// The Kafka key is the `type_name`. A null value (`None`) OR an empty value
/// (`Some(b"")`) is a tombstone — rdkafka returns `None` for a true Kafka null
/// and `Some(&[])` for an empty-payload delete, so both must be handled.
pub fn parse_ont_type_record(
    topic: &str,
    key_bytes: &[u8],
    value_bytes: Option<&[u8]>,
) -> Result<OntTypeEvent, ParseError> {
    let tenant = tenant_from_topic(topic)?.to_string();
    let type_name = std::str::from_utf8(key_bytes)
        .map_err(|e| ParseError::KeyNotUtf8(e.to_string()))?
        .to_string();
    if type_name.is_empty() {
        return Err(ParseError::EmptyKey);
    }
    match value_bytes {
        None => Ok(OntTypeEvent::Tombstone { tenant, type_name }),
        Some(v) if v.is_empty() => Ok(OntTypeEvent::Tombstone { tenant, type_name }),
        Some(v) => {
            let env: OntTypeRecordEnvelope =
                serde_json::from_slice(v).map_err(|e| ParseError::BadJson(e.to_string()))?;
            if env.object_type.type_name != type_name {
                return Err(ParseError::KeyBodyMismatch {
                    key: type_name,
                    body: env.object_type.type_name,
                });
            }
            Ok(OntTypeEvent::Upsert {
                tenant,
                object_type: env.object_type,
            })
        }
    }
}

/// PURE apply: mutate the index, return the outcome. No Kafka.
pub fn apply_event(index: &OntTypeIndex, event: OntTypeEvent) -> OntTypeApply {
    match event {
        OntTypeEvent::Upsert { tenant, object_type } => {
            let key = OntTypeKey {
                tenant,
                type_name: object_type.type_name.clone(),
            };
            index.upsert(key, object_type);
            OntTypeApply::Upserted
        }
        OntTypeEvent::Tombstone { tenant, type_name } => {
            let key = OntTypeKey { tenant, type_name };
            if index.remove(&key) {
                OntTypeApply::Removed
            } else {
                OntTypeApply::RemovedMissing
            }
        }
    }
}

/// Consumer stats (exposed for observability).
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct OntTypeStats {
    pub records_processed: u64,
    pub upserts: u64,
    pub tombstones: u64,
    pub parse_errors: u64,
}

/// Consumer config.
#[derive(Debug, Clone)]
pub struct OntTypesConsumerConfig {
    pub kafka_brokers: String,
    pub group_id: String,
}
impl OntTypesConsumerConfig {
    pub fn new(kafka_brokers: impl Into<String>) -> Self {
        Self {
            kafka_brokers: kafka_brokers.into(),
            group_id: "chronik-ontology-types-consumer".to_string(),
        }
    }
    pub fn with_group_id(mut self, group_id: impl Into<String>) -> Self {
        self.group_id = group_id.into();
        self
    }
}

/// The only rdkafka-touching function. Hydrates from the beginning of the
/// compacted topic(s) via `auto.offset.reset=earliest`, and (like the tenants
/// registry) does not commit offsets — every startup rebuilds the full
/// source-of-truth registry deterministically. Bad records are logged + skipped;
/// only a transport `recv` error returns `Err` (→ reconnect in `spawn`).
pub async fn run_consumer(
    config: &OntTypesConsumerConfig,
    index: &OntTypeIndex,
    stats: &Arc<Mutex<OntTypeStats>>,
) -> Result<(), String> {
    let consumer: StreamConsumer = ClientConfig::new()
        .set("bootstrap.servers", &config.kafka_brokers)
        .set("group.id", &config.group_id)
        .set("enable.auto.commit", "false")
        .set("auto.offset.reset", "earliest")
        .create()
        .map_err(|e| format!("ont.types consumer create: {e}"))?;
    // A `^`-anchored topic string is treated by librdkafka as a subscription
    // pattern, so new `ont.types.{tenant}` topics are auto-discovered.
    consumer
        .subscribe(&[ONT_TYPES_TOPICS_REGEX])
        .map_err(|e| format!("ont.types subscribe: {e}"))?;
    info!(regex = ONT_TYPES_TOPICS_REGEX, "ont.types registry consumer started");
    loop {
        match consumer.recv().await {
            Ok(msg) => {
                let topic = msg.topic();
                let key_bytes = msg.key().unwrap_or(&[]);
                let value_bytes = msg.payload();
                match parse_ont_type_record(topic, key_bytes, value_bytes) {
                    Ok(event) => {
                        let mut s = stats.lock();
                        s.records_processed += 1;
                        match apply_event(index, event) {
                            OntTypeApply::Upserted => s.upserts += 1,
                            OntTypeApply::Removed | OntTypeApply::RemovedMissing => {
                                s.tombstones += 1
                            }
                        }
                    }
                    Err(e) => {
                        warn!(error = %e, topic = %topic, "skipping unparseable ont.types record");
                        stats.lock().parse_errors += 1;
                    }
                }
            }
            Err(e) => return Err(format!("ont.types recv: {e}")),
        }
    }
}

/// Spawn the consumer with a fixed 5s retry loop (mirrors the memory consumers).
/// Stats are created internally; the `JoinHandle` is intentionally dropped by
/// callers. (`run_consumer` keeps an explicit `stats` param for tests.)
pub fn spawn_ont_types_consumer(
    config: OntTypesConsumerConfig,
    index: OntTypeIndex,
) -> tokio::task::JoinHandle<()> {
    let stats = Arc::new(Mutex::new(OntTypeStats::default()));
    tokio::spawn(async move {
        loop {
            match run_consumer(&config, &index, &stats).await {
                Ok(()) => {
                    info!("ont.types consumer exited cleanly");
                    return;
                }
                Err(e) => {
                    warn!(error = %e, "ont.types consumer errored, retrying in 5s");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object_type::{AttrSpec, AttrType, BackingBinding, IdentitySpec};

    fn entity(type_name: &str) -> ObjectType {
        ObjectType {
            type_name: type_name.to_string(),
            attributes: vec![AttrSpec {
                name: "degree".to_string(),
                attr_type: AttrType::String,
                from_predicate: Some("has_degree".to_string()),
                multi: false,
            }],
            identity: IdentitySpec {
                id_field: "subject".to_string(),
                normalize: true,
            },
            backing: BackingBinding {
                topic_prefix: "mem.fact".to_string(),
                append_only: false,
            },
            description: None,
        }
    }

    fn value_bytes(ty: &ObjectType) -> Vec<u8> {
        serde_json::to_vec(&OntTypeRecordEnvelope {
            schema_version: 1,
            object_type: ty.clone(),
        })
        .unwrap()
    }

    // ---- tenant_from_topic --------------------------------------------------

    #[test]
    fn tenant_from_topic_ok() {
        assert_eq!(tenant_from_topic("ont.types.acme").unwrap(), "acme");
    }

    #[test]
    fn tenant_from_topic_rejects_bad_shapes() {
        assert!(matches!(tenant_from_topic("mem.fact.acme"), Err(ParseError::BadTopic(_))));
        assert!(matches!(tenant_from_topic("ont.types."), Err(ParseError::BadTopic(_))));
        assert!(matches!(tenant_from_topic("ont.types.a.b"), Err(ParseError::BadTopic(_))));
    }

    // ---- parse --------------------------------------------------------------

    #[test]
    fn parse_upsert_happy_path() {
        let ty = entity("Entity");
        let ev = parse_ont_type_record("ont.types.acme", b"Entity", Some(&value_bytes(&ty))).unwrap();
        assert_eq!(ev, OntTypeEvent::Upsert { tenant: "acme".to_string(), object_type: ty });
    }

    #[test]
    fn parse_tombstone_from_none_and_empty() {
        for v in [None, Some(&b""[..])] {
            let ev = parse_ont_type_record("ont.types.acme", b"Entity", v).unwrap();
            assert_eq!(
                ev,
                OntTypeEvent::Tombstone { tenant: "acme".to_string(), type_name: "Entity".to_string() }
            );
        }
    }

    #[test]
    fn parse_rejects_bad_key_and_body() {
        // non-UTF-8 key
        assert!(matches!(
            parse_ont_type_record("ont.types.acme", &[0xff], Some(b"{}")),
            Err(ParseError::KeyNotUtf8(_))
        ));
        // empty key
        assert!(matches!(
            parse_ont_type_record("ont.types.acme", b"", Some(b"{}")),
            Err(ParseError::EmptyKey)
        ));
        // bad JSON
        assert!(matches!(
            parse_ont_type_record("ont.types.acme", b"Entity", Some(b"not-json")),
            Err(ParseError::BadJson(_))
        ));
        // key/body type_name mismatch
        let ty = entity("Entity");
        assert!(matches!(
            parse_ont_type_record("ont.types.acme", b"Repository", Some(&value_bytes(&ty))),
            Err(ParseError::KeyBodyMismatch { .. })
        ));
        // bad topic
        assert!(matches!(
            parse_ont_type_record("mem.fact.acme", b"Entity", Some(&value_bytes(&ty))),
            Err(ParseError::BadTopic(_))
        ));
    }

    // ---- apply + index ------------------------------------------------------

    #[test]
    fn apply_upsert_then_get_then_tombstone() {
        let idx = OntTypeIndex::new();
        assert!(idx.is_empty());

        let ev = parse_ont_type_record("ont.types.acme", b"Entity", Some(&value_bytes(&entity("Entity")))).unwrap();
        assert_eq!(apply_event(&idx, ev), OntTypeApply::Upserted);
        assert_eq!(idx.len(), 1);
        assert_eq!(idx.get("acme", "Entity").unwrap().type_name, "Entity");
        assert!(idx.get("other-tenant", "Entity").is_none()); // tenant-scoped

        // tombstone removes it
        let tomb = parse_ont_type_record("ont.types.acme", b"Entity", None).unwrap();
        assert_eq!(apply_event(&idx, tomb), OntTypeApply::Removed);
        assert!(idx.get("acme", "Entity").is_none());
        assert!(idx.is_empty());
    }

    #[test]
    fn tombstone_missing_is_noop() {
        let idx = OntTypeIndex::new();
        let tomb = parse_ont_type_record("ont.types.acme", b"Ghost", None).unwrap();
        assert_eq!(apply_event(&idx, tomb), OntTypeApply::RemovedMissing);
        assert!(idx.is_empty());
    }

    #[test]
    fn upsert_latest_wins_and_list_for_tenant() {
        let idx = OntTypeIndex::new();
        apply_event(&idx, OntTypeEvent::Upsert { tenant: "acme".into(), object_type: entity("Entity") });
        apply_event(&idx, OntTypeEvent::Upsert { tenant: "acme".into(), object_type: entity("Repository") });
        apply_event(&idx, OntTypeEvent::Upsert { tenant: "beta".into(), object_type: entity("Entity") });

        let mut names: Vec<String> = idx.list_for_tenant("acme").into_iter().map(|t| t.type_name).collect();
        names.sort();
        assert_eq!(names, vec!["Entity".to_string(), "Repository".to_string()]);
        assert_eq!(idx.list_for_tenant("beta").len(), 1);
    }

    // ---- topic regex --------------------------------------------------------

    #[test]
    fn topic_regex_matches_expected() {
        let re = regex::Regex::new(ONT_TYPES_TOPICS_REGEX).unwrap();
        assert!(re.is_match("ont.types.acme"));
        assert!(re.is_match("ont.types.tenant123"));
        assert!(!re.is_match("ont.types."));
        assert!(!re.is_match("ont.types.a.b")); // sub-dotted tenant excluded
        assert!(!re.is_match("mem.fact.acme"));
        assert!(!re.is_match("ont.edges.acme"));
    }

    // ---- config -------------------------------------------------------------

    #[test]
    fn consumer_config_defaults() {
        let c = OntTypesConsumerConfig::new("localhost:9092");
        assert_eq!(c.kafka_brokers, "localhost:9092");
        assert_eq!(c.group_id, "chronik-ontology-types-consumer");
        let c = c.with_group_id("custom");
        assert_eq!(c.group_id, "custom");
    }
}
