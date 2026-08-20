//! `ont.links.{tenant}` → LinkType registry (O-1 Link Types, the inverse-aware
//! relation surface).
//!
//! **9th instance** of the consumer-maintained keyed-index pattern
//! (cf. [`crate::ont_types_consumer`]). Its job is to remove the incoming /
//! outgoing *direction flag* that confuses agents: a LinkType declares a
//! relation with an **inverse name**, so an agent selects a NAMED relation that
//! matches the question ("what blocks X" → `blocked_by`; "what X blocks" →
//! `blocks`) and the registry maps it to the right walk direction over the
//! [`crate::edge_index`]. This is the roadmap §5 stance ("select named relations,
//! don't invent identifiers or reason about direction").

use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use parking_lot::Mutex;
use rdkafka::config::ClientConfig;
use rdkafka::consumer::{Consumer, StreamConsumer};
use rdkafka::Message;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::edge_index::Direction;

/// Regex matching every per-tenant LinkType topic: `ont.links.{tenant}`.
pub const ONT_LINKS_TOPICS_REGEX: &str = r"^ont\.links\.[^.]+$";

/// A user-declared relation over the fact graph. `predicate` is the underlying
/// edge predicate stored in `mem.fact`; `inverse` is the name of the reverse
/// reading (walking the same predicate the other direction).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkType {
    pub name: String,
    pub predicate: String,
    #[serde(default)]
    pub inverse: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}

/// What a relation name resolves to: the predicate to walk and which direction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelationTarget {
    pub predicate: String,
    pub direction: Direction,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RelKey {
    tenant: String,
    relation: String,
}

/// Registry: relation name (forward OR inverse) -> how to walk it, plus the raw
/// definitions for listing. Cheaply cloneable (shared maps).
#[derive(Debug, Default, Clone)]
pub struct LinkTypeIndex {
    rels: Arc<DashMap<RelKey, RelationTarget>>,
    defs: Arc<DashMap<RelKey, LinkType>>,
}

impl LinkTypeIndex {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn is_empty(&self) -> bool {
        self.defs.is_empty()
    }
    pub fn len(&self) -> usize {
        self.defs.len()
    }

    /// Resolve a relation name to `(predicate, direction)`. Matches both the
    /// forward name (→ outgoing) and the inverse name (→ incoming).
    pub fn resolve(&self, tenant: &str, relation: &str) -> Option<RelationTarget> {
        self.rels
            .get(&RelKey { tenant: tenant.to_string(), relation: relation.to_string() })
            .map(|r| r.clone())
    }

    /// The LinkTypes declared for a tenant (for a `relations` listing).
    pub fn list_for_tenant(&self, tenant: &str) -> Vec<LinkType> {
        self.defs
            .iter()
            .filter(|e| e.key().tenant == tenant)
            .map(|e| e.value().clone())
            .collect()
    }

    fn upsert(&self, tenant: &str, lt: LinkType) {
        let fwd = RelKey { tenant: tenant.to_string(), relation: lt.name.clone() };
        // Drop any previous inverse mapping this definition owned (name may have
        // changed its inverse) before re-inserting.
        if let Some(prev) = self.defs.get(&fwd) {
            if let Some(prev_inv) = &prev.inverse {
                self.rels.remove(&RelKey { tenant: tenant.to_string(), relation: prev_inv.clone() });
            }
        }
        self.rels.insert(
            fwd.clone(),
            RelationTarget { predicate: lt.predicate.clone(), direction: Direction::Outgoing },
        );
        if let Some(inv) = &lt.inverse {
            self.rels.insert(
                RelKey { tenant: tenant.to_string(), relation: inv.clone() },
                RelationTarget { predicate: lt.predicate.clone(), direction: Direction::Incoming },
            );
        }
        self.defs.insert(fwd, lt);
    }

    fn remove(&self, tenant: &str, name: &str) -> bool {
        let key = RelKey { tenant: tenant.to_string(), relation: name.to_string() };
        if let Some((_, lt)) = self.defs.remove(&key) {
            self.rels.remove(&key);
            if let Some(inv) = lt.inverse {
                self.rels.remove(&RelKey { tenant: tenant.to_string(), relation: inv });
            }
            true
        } else {
            false
        }
    }
}

/// Wire envelope stored as the Kafka value.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkTypeEnvelope {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    pub link_type: LinkType,
}
fn default_schema_version() -> u32 {
    1
}

#[derive(Debug, Clone, PartialEq)]
pub enum OntLinkEvent {
    Upsert { tenant: String, link_type: LinkType },
    Tombstone { tenant: String, name: String },
}

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("topic {0:?} is not a valid ont.links.{{tenant}} topic")]
    BadTopic(String),
    #[error("key is not valid UTF-8: {0}")]
    KeyNotUtf8(String),
    #[error("key (relation name) is empty")]
    EmptyKey,
    #[error("value is not valid JSON: {0}")]
    BadJson(String),
    #[error("name in key ({key:?}) does not match body ({body:?})")]
    KeyBodyMismatch { key: String, body: String },
}

fn tenant_from_topic(topic: &str) -> Result<&str, ParseError> {
    let rest = topic
        .strip_prefix("ont.links.")
        .ok_or_else(|| ParseError::BadTopic(topic.to_string()))?;
    if rest.is_empty() || rest.contains('.') {
        return Err(ParseError::BadTopic(topic.to_string()));
    }
    Ok(rest)
}

/// PURE decode. Key = relation name; null/empty value = tombstone.
pub fn parse_ont_link_record(
    topic: &str,
    key_bytes: &[u8],
    value_bytes: Option<&[u8]>,
) -> Result<OntLinkEvent, ParseError> {
    let tenant = tenant_from_topic(topic)?.to_string();
    let name = std::str::from_utf8(key_bytes)
        .map_err(|e| ParseError::KeyNotUtf8(e.to_string()))?
        .to_string();
    if name.is_empty() {
        return Err(ParseError::EmptyKey);
    }
    match value_bytes {
        None => Ok(OntLinkEvent::Tombstone { tenant, name }),
        Some(v) if v.is_empty() => Ok(OntLinkEvent::Tombstone { tenant, name }),
        Some(v) => {
            let env: LinkTypeEnvelope =
                serde_json::from_slice(v).map_err(|e| ParseError::BadJson(e.to_string()))?;
            if env.link_type.name != name {
                return Err(ParseError::KeyBodyMismatch { key: name, body: env.link_type.name });
            }
            Ok(OntLinkEvent::Upsert { tenant, link_type: env.link_type })
        }
    }
}

/// PURE apply.
pub fn apply_event(index: &LinkTypeIndex, event: OntLinkEvent) -> bool {
    match event {
        OntLinkEvent::Upsert { tenant, link_type } => {
            index.upsert(&tenant, link_type);
            true
        }
        OntLinkEvent::Tombstone { tenant, name } => index.remove(&tenant, &name),
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct OntLinkStats {
    pub records_processed: u64,
    pub upserts: u64,
    pub tombstones: u64,
    pub parse_errors: u64,
}

#[derive(Debug, Clone)]
pub struct OntLinksConsumerConfig {
    pub kafka_brokers: String,
    pub group_id: String,
}
impl OntLinksConsumerConfig {
    pub fn new(kafka_brokers: impl Into<String>) -> Self {
        Self { kafka_brokers: kafka_brokers.into(), group_id: "chronik-ontology-links-consumer".to_string() }
    }
    pub fn with_group_id(mut self, group_id: impl Into<String>) -> Self {
        self.group_id = group_id.into();
        self
    }
}

pub async fn run_consumer(
    config: &OntLinksConsumerConfig,
    index: &LinkTypeIndex,
    stats: &Arc<Mutex<OntLinkStats>>,
) -> Result<(), String> {
    let consumer: StreamConsumer = ClientConfig::new()
        .set("bootstrap.servers", &config.kafka_brokers)
        .set("group.id", &config.group_id)
        .set("enable.auto.commit", "false")
        .set("auto.offset.reset", "earliest")
        .set("topic.metadata.refresh.interval.ms", "5000")
        .create()
        .map_err(|e| format!("ont.links consumer create: {e}"))?;
    consumer
        .subscribe(&[ONT_LINKS_TOPICS_REGEX])
        .map_err(|e| format!("ont.links subscribe: {e}"))?;
    info!(regex = ONT_LINKS_TOPICS_REGEX, "ont.links LinkType registry consumer started");
    loop {
        match consumer.recv().await {
            Ok(msg) => {
                let topic = msg.topic();
                match parse_ont_link_record(topic, msg.key().unwrap_or(&[]), msg.payload()) {
                    Ok(event) => {
                        let mut s = stats.lock();
                        s.records_processed += 1;
                        match &event {
                            OntLinkEvent::Upsert { .. } => s.upserts += 1,
                            OntLinkEvent::Tombstone { .. } => s.tombstones += 1,
                        }
                        apply_event(index, event);
                    }
                    Err(e) => {
                        warn!(error = %e, topic = %topic, "skipping unparseable ont.links record");
                        stats.lock().parse_errors += 1;
                    }
                }
            }
            Err(e) => return Err(format!("ont.links recv: {e}")),
        }
    }
}

pub fn spawn_ont_links_consumer(
    config: OntLinksConsumerConfig,
    index: LinkTypeIndex,
) -> tokio::task::JoinHandle<()> {
    let stats = Arc::new(Mutex::new(OntLinkStats::default()));
    tokio::spawn(async move {
        loop {
            match run_consumer(&config, &index, &stats).await {
                Ok(()) => {
                    info!("ont.links consumer exited cleanly");
                    return;
                }
                Err(e) => {
                    warn!(error = %e, "ont.links consumer errored, retrying in 5s");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lt(name: &str, predicate: &str, inverse: Option<&str>) -> LinkType {
        LinkType { name: name.into(), predicate: predicate.into(), inverse: inverse.map(|s| s.into()), description: None }
    }
    fn env_bytes(lt: &LinkType) -> Vec<u8> {
        serde_json::to_vec(&LinkTypeEnvelope { schema_version: 1, link_type: lt.clone() }).unwrap()
    }

    #[test]
    fn forward_resolves_outgoing_inverse_resolves_incoming() {
        let idx = LinkTypeIndex::new();
        apply_event(&idx, parse_ont_link_record("ont.links.acme", b"blocked_by", Some(&env_bytes(&lt("blocked_by", "blocked_by", Some("blocks"))))).unwrap());
        let fwd = idx.resolve("acme", "blocked_by").unwrap();
        assert_eq!(fwd, RelationTarget { predicate: "blocked_by".into(), direction: Direction::Outgoing });
        let inv = idx.resolve("acme", "blocks").unwrap();
        assert_eq!(inv, RelationTarget { predicate: "blocked_by".into(), direction: Direction::Incoming });
        assert!(idx.resolve("acme", "nonsense").is_none());
        assert!(idx.resolve("other", "blocks").is_none()); // tenant-scoped
    }

    #[test]
    fn link_without_inverse_only_forward() {
        let idx = LinkTypeIndex::new();
        apply_event(&idx, parse_ont_link_record("ont.links.acme", b"parent_of", Some(&env_bytes(&lt("parent_of", "parent_of", None)))).unwrap());
        assert!(idx.resolve("acme", "parent_of").is_some());
        assert_eq!(idx.list_for_tenant("acme").len(), 1);
    }

    #[test]
    fn tombstone_removes_forward_and_inverse() {
        let idx = LinkTypeIndex::new();
        apply_event(&idx, parse_ont_link_record("ont.links.acme", b"blocked_by", Some(&env_bytes(&lt("blocked_by", "blocked_by", Some("blocks"))))).unwrap());
        assert!(idx.resolve("acme", "blocks").is_some());
        let removed = apply_event(&idx, parse_ont_link_record("ont.links.acme", b"blocked_by", None).unwrap());
        assert!(removed);
        assert!(idx.resolve("acme", "blocked_by").is_none());
        assert!(idx.resolve("acme", "blocks").is_none()); // inverse cleaned up too
        assert!(idx.is_empty());
    }

    #[test]
    fn parse_rejects_key_body_mismatch_and_bad_topic() {
        assert!(matches!(
            parse_ont_link_record("ont.links.acme", b"blocks", Some(&env_bytes(&lt("blocked_by", "blocked_by", None)))),
            Err(ParseError::KeyBodyMismatch { .. })
        ));
        assert!(matches!(parse_ont_link_record("mem.fact.acme", b"x", Some(b"{}")), Err(ParseError::BadTopic(_))));
    }

    #[test]
    fn topic_regex() {
        let re = regex::Regex::new(ONT_LINKS_TOPICS_REGEX).unwrap();
        assert!(re.is_match("ont.links.acme"));
        assert!(!re.is_match("ont.links.a.b"));
        assert!(!re.is_match("ont.types.acme"));
    }

    #[test]
    fn changing_inverse_drops_the_old_inverse_mapping() {
        let idx = LinkTypeIndex::new();
        apply_event(&idx, OntLinkEvent::Upsert { tenant: "acme".into(), link_type: lt("r", "p", Some("old_inv")) });
        assert!(idx.resolve("acme", "old_inv").is_some());
        apply_event(&idx, OntLinkEvent::Upsert { tenant: "acme".into(), link_type: lt("r", "p", Some("new_inv")) });
        assert!(idx.resolve("acme", "new_inv").is_some());
        assert!(idx.resolve("acme", "old_inv").is_none(), "stale inverse must be dropped");
    }
}
