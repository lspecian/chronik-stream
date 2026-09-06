//! `mem.fact.{tenant}` → materialized bidirectional edge index (O-1).
//!
//! **8th instance** of the "consumer-maintained keyed in-memory index rebuilt
//! from a compacted Kafka topic" pattern (cf. [`crate::ont_types_consumer`],
//! `chronik-memory/src/mem_config_consumer.rs`). Where [`crate::traverse`]
//! answers *outgoing* edges on demand by BM25-searching `mem.fact` per hop, this
//! index materializes **both directions** from the fact stream — so
//! `incoming(X)` ("what points at X") is O(1), which the on-demand traversal
//! structurally cannot do (search keys on the subject, not the object). Edges
//! carry bi-temporal validity so `as_of` traversal is a filter, and supersession
//! updates an edge in place rather than duplicating it.
//!
//! The five pieces: (a) [`RelationshipIndex`] (bidirectional `Arc<DashMap>`s),
//! (b) [`EdgeEvent`], (c) the pure [`parse_edge_from_fact`], (d) the pure
//! [`apply_edge_event`], (e) the async [`run_edge_consumer`] +
//! [`spawn_edge_consumer`] retry loop.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use parking_lot::Mutex;
use rdkafka::config::ClientConfig;
use rdkafka::consumer::{Consumer, StreamConsumer};
use rdkafka::Message;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::resolve::SourceRef;

/// Regex matching every per-tenant fact topic: `mem.fact.{tenant}` (tenant carries
/// no dots). New tenants are auto-discovered by the pattern subscription.
pub const MEM_FACT_TOPICS_REGEX: &str = r"^mem\.fact\.[^.]+$";

/// A materialized edge `(from) --[edge_type]--> (to)`, with bi-temporal validity
/// and provenance. `to` is always an entity id (string) — non-string fact
/// objects are attributes, not edges, and are not indexed here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IndexedEdge {
    pub namespace: String,
    pub from: String,
    pub edge_type: String,
    pub to: String,
    pub valid_from: Option<DateTime<Utc>>,
    pub valid_to: Option<DateTime<Utc>>,
    pub provenance: Vec<SourceRef>,
}

impl IndexedEdge {
    /// Edge identity for dedup/supersession — a newer record for the same tuple
    /// replaces the older one (never duplicates).
    fn identity(&self) -> (String, String, String, String) {
        (
            self.namespace.clone(),
            self.from.clone(),
            self.edge_type.clone(),
            self.to.clone(),
        )
    }

    /// Bi-temporal validity at `at`: `valid_from <= at < valid_to`. A missing
    /// `valid_from` is lenient (kept); a missing `valid_to` means still valid.
    /// `None` = no time filter.
    pub fn valid_at(&self, at: Option<DateTime<Utc>>) -> bool {
        let Some(t) = at else { return true };
        let after_start = self.valid_from.map(|vf| vf <= t).unwrap_or(true);
        let before_end = self.valid_to.map(|vt| t < vt).unwrap_or(true);
        after_start && before_end
    }
}

/// Traversal direction for [`RelationshipIndex::walk`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Follow edges FROM the node (subject → object).
    Outgoing,
    /// Follow edges TO the node (object ← subject) — the reverse walk.
    Incoming,
}

/// Bidirectional edge index. Cloning yields another handle to the SAME maps (one
/// for the consumer task, one for the API/traversal caller).
#[derive(Debug, Default, Clone)]
pub struct RelationshipIndex {
    /// (namespace, from) -> outgoing edges.
    outgoing: Arc<DashMap<(String, String), Vec<IndexedEdge>>>,
    /// (namespace, to) -> incoming edges.
    incoming: Arc<DashMap<(String, String), Vec<IndexedEdge>>>,
}

impl RelationshipIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of distinct source nodes with at least one outgoing edge.
    pub fn source_nodes(&self) -> usize {
        self.outgoing.len()
    }

    /// Total edges held (sum over outgoing buckets).
    pub fn edge_count(&self) -> usize {
        self.outgoing.iter().map(|e| e.value().len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.outgoing.is_empty()
    }

    /// Outgoing edges from `from`, optionally filtered by `edge_type` and `as_of`.
    pub fn outgoing(
        &self,
        namespace: &str,
        from: &str,
        edge_type: Option<&str>,
        as_of: Option<DateTime<Utc>>,
    ) -> Vec<IndexedEdge> {
        self.query(&self.outgoing, namespace, from, edge_type, as_of)
    }

    /// Incoming edges to `to` — the reverse lookup the on-demand traversal can't
    /// do cheaply — optionally filtered by `edge_type` and `as_of`.
    pub fn incoming(
        &self,
        namespace: &str,
        to: &str,
        edge_type: Option<&str>,
        as_of: Option<DateTime<Utc>>,
    ) -> Vec<IndexedEdge> {
        self.query(&self.incoming, namespace, to, edge_type, as_of)
    }

    fn query(
        &self,
        map: &DashMap<(String, String), Vec<IndexedEdge>>,
        namespace: &str,
        node: &str,
        edge_type: Option<&str>,
        as_of: Option<DateTime<Utc>>,
    ) -> Vec<IndexedEdge> {
        let key = (namespace.to_string(), node.to_string());
        let Some(bucket) = map.get(&key) else {
            return Vec::new();
        };
        bucket
            .iter()
            .filter(|e| edge_type.map(|t| t == e.edge_type).unwrap_or(true))
            .filter(|e| e.valid_at(as_of))
            .cloned()
            .collect()
    }

    /// Multi-hop breadth-first walk over the materialized index, in either
    /// direction, cycle-safe, returning every edge discovered tagged with its
    /// hop depth (1 = direct). This is O(edges) per hop with no `/_search` —
    /// and, uniquely, works in the **incoming** direction, so an agent can walk
    /// "what transitively points at X" which the on-demand traversal cannot.
    pub fn walk(
        &self,
        namespace: &str,
        start: &str,
        edge_type: Option<&str>,
        direction: Direction,
        max_depth: usize,
        as_of: Option<DateTime<Utc>>,
    ) -> Vec<(usize, IndexedEdge)> {
        let mut out = Vec::new();
        let mut visited = std::collections::HashSet::new();
        visited.insert(start.to_lowercase());
        let mut frontier = vec![start.to_string()];
        for depth in 1..=max_depth.max(1) {
            let mut next = Vec::new();
            for node in &frontier {
                let edges = match direction {
                    Direction::Outgoing => self.outgoing(namespace, node, edge_type, as_of),
                    Direction::Incoming => self.incoming(namespace, node, edge_type, as_of),
                };
                for e in edges {
                    // The endpoint to expand from next hop: the "other" side.
                    let nbr = match direction {
                        Direction::Outgoing => e.to.clone(),
                        Direction::Incoming => e.from.clone(),
                    };
                    if visited.insert(nbr.to_lowercase()) {
                        next.push(nbr);
                    }
                    out.push((depth, e));
                }
            }
            if next.is_empty() {
                break;
            }
            frontier = next;
        }
        out
    }

    fn upsert(&self, edge: IndexedEdge) {
        let id = edge.identity();
        // outgoing bucket keyed by (namespace, from)
        let out_key = (edge.namespace.clone(), edge.from.clone());
        let mut out = self.outgoing.entry(out_key).or_default();
        match out.iter_mut().find(|e| e.identity() == id) {
            Some(existing) => *existing = edge.clone(),
            None => out.push(edge.clone()),
        }
        drop(out);
        // incoming bucket keyed by (namespace, to)
        let in_key = (edge.namespace.clone(), edge.to.clone());
        let mut inc = self.incoming.entry(in_key).or_default();
        match inc.iter_mut().find(|e| e.identity() == id) {
            Some(existing) => *existing = edge,
            None => inc.push(edge),
        }
    }

    fn remove(&self, id: &(String, String, String, String)) -> bool {
        let (ns, from, _ty, to) = id;
        let mut removed = false;
        if let Some(mut out) = self.outgoing.get_mut(&(ns.clone(), from.clone())) {
            let before = out.len();
            out.retain(|e| &e.identity() != id);
            removed |= out.len() != before;
        }
        if let Some(mut inc) = self.incoming.get_mut(&(ns.clone(), to.clone())) {
            let before = inc.len();
            inc.retain(|e| &e.identity() != id);
            removed |= inc.len() != before;
        }
        removed
    }
}

/// A decoded edge-index event.
#[derive(Debug, Clone, PartialEq)]
pub enum EdgeEvent {
    Upsert(IndexedEdge),
    Tombstone {
        namespace: String,
        from: String,
        edge_type: String,
        to: String,
    },
}

/// Outcome of applying an event (drives stats).
#[derive(Debug, Clone, PartialEq)]
pub enum EdgeApply {
    Upserted,
    Removed,
    RemovedMissing,
}

/// Decode errors.
#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("topic {0:?} is not a valid mem.fact.{{tenant}} topic")]
    BadTopic(String),
    #[error("value is not valid JSON: {0}")]
    BadJson(String),
}

fn tenant_from_topic(topic: &str) -> Result<&str, ParseError> {
    let rest = topic
        .strip_prefix("mem.fact.")
        .ok_or_else(|| ParseError::BadTopic(topic.to_string()))?;
    if rest.is_empty() || rest.contains('.') {
        return Err(ParseError::BadTopic(topic.to_string()));
    }
    Ok(rest)
}

/// Tolerant envelope extraction: accepts the direct produced shape (envelope
/// fields present) and the wrapped shape (envelope JSON under `value`).
fn envelope(value: &serde_json::Value) -> serde_json::Value {
    if value.get("body").is_some() || value.get("type").is_some() {
        return value.clone();
    }
    for f in ["value", "_value", "_json_content"] {
        if let Some(s) = value.get(f).and_then(|v| v.as_str()) {
            if let Ok(inner) = serde_json::from_str::<serde_json::Value>(s) {
                return inner;
            }
        }
    }
    value.clone()
}

fn parse_ts(env: &serde_json::Value, field: &str) -> Option<DateTime<Utc>> {
    env.get(field)
        .and_then(|v| v.as_str())
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc))
}

/// PURE decode: a `mem.fact` record → an edge event, or `Ok(None)` when the
/// record is not an edge (object is not an entity string). The concurrency /
/// identity model treats `tombstoned: true` as an edge deletion.
///
/// Edges connect entities, so only string-valued fact `object`s are indexed;
/// numeric/boolean attribute values are skipped (`Ok(None)`).
pub fn parse_edge_from_fact(
    topic: &str,
    value_bytes: Option<&[u8]>,
) -> Result<Option<EdgeEvent>, ParseError> {
    let namespace_default = tenant_from_topic(topic)?.to_string();
    let Some(bytes) = value_bytes else {
        // A true Kafka-null in mem.fact carries no identity to remove; the memory
        // model tombstones via a `tombstoned: true` record, so skip nulls.
        return Ok(None);
    };
    if bytes.is_empty() {
        return Ok(None);
    }
    let raw: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|e| ParseError::BadJson(e.to_string()))?;
    let env = envelope(&raw);

    let namespace = env
        .get("namespace")
        .and_then(|v| v.as_str())
        .unwrap_or(&namespace_default)
        .to_string();
    let body = env.get("body").unwrap_or(&env);
    let (Some(from), Some(edge_type)) = (
        body.get("subject").and_then(|v| v.as_str()),
        body.get("predicate").and_then(|v| v.as_str()),
    ) else {
        return Ok(None); // not a fact triple
    };
    // Only entity (string) objects are edges.
    let Some(to) = body.get("object").and_then(|v| v.as_str()) else {
        return Ok(None);
    };
    if edge_type.is_empty() || from.is_empty() || to.is_empty() {
        return Ok(None);
    }

    if env
        .get("tombstoned")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return Ok(Some(EdgeEvent::Tombstone {
            namespace,
            from: from.to_string(),
            edge_type: edge_type.to_string(),
            to: to.to_string(),
        }));
    }

    let provenance = env
        .get("source")
        .map(|s| SourceRef {
            topic: s.get("topic").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            offsets: s
                .get("offsets")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|o| o.as_i64()).collect())
                .unwrap_or_default(),
        })
        .into_iter()
        .collect();

    Ok(Some(EdgeEvent::Upsert(IndexedEdge {
        namespace,
        from: from.to_string(),
        edge_type: edge_type.to_string(),
        to: to.to_string(),
        valid_from: parse_ts(&env, "valid_from"),
        valid_to: parse_ts(&env, "valid_to"),
        provenance,
    })))
}

/// PURE apply: mutate the index, return the outcome. No Kafka.
pub fn apply_edge_event(index: &RelationshipIndex, event: EdgeEvent) -> EdgeApply {
    match event {
        EdgeEvent::Upsert(edge) => {
            index.upsert(edge);
            EdgeApply::Upserted
        }
        EdgeEvent::Tombstone {
            namespace,
            from,
            edge_type,
            to,
        } => {
            if index.remove(&(namespace, from, edge_type, to)) {
                EdgeApply::Removed
            } else {
                EdgeApply::RemovedMissing
            }
        }
    }
}

/// Consumer stats (exposed for observability).
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct EdgeStats {
    pub records_processed: u64,
    pub upserts: u64,
    pub tombstones: u64,
    pub skipped_non_edge: u64,
    pub parse_errors: u64,
}

/// Consumer config.
#[derive(Debug, Clone)]
pub struct EdgeConsumerConfig {
    pub kafka_brokers: String,
    pub group_id: String,
}
impl EdgeConsumerConfig {
    pub fn new(kafka_brokers: impl Into<String>) -> Self {
        Self {
            kafka_brokers: kafka_brokers.into(),
            group_id: "chronik-ontology-edge-consumer".to_string(),
        }
    }
    pub fn with_group_id(mut self, group_id: impl Into<String>) -> Self {
        self.group_id = group_id.into();
        self
    }
}

/// The only rdkafka-touching function. Hydrates from the beginning of the
/// compacted `mem.fact.{tenant}` topics; does not commit offsets (every startup
/// rebuilds the full edge graph deterministically). Bad records are logged +
/// skipped; only a transport `recv` error returns `Err` (→ reconnect in spawn).
pub async fn run_edge_consumer(
    config: &EdgeConsumerConfig,
    index: &RelationshipIndex,
    stats: &Arc<Mutex<EdgeStats>>,
) -> Result<(), String> {
    let consumer: StreamConsumer = ClientConfig::new()
        .set("bootstrap.servers", &config.kafka_brokers)
        .set("group.id", &config.group_id)
        .set("enable.auto.commit", "false")
        .set("auto.offset.reset", "earliest")
        .set("topic.metadata.refresh.interval.ms", "5000")
        .create()
        .map_err(|e| format!("mem.fact edge consumer create: {e}"))?;
    consumer
        .subscribe(&[MEM_FACT_TOPICS_REGEX])
        .map_err(|e| format!("mem.fact edge subscribe: {e}"))?;
    info!(regex = MEM_FACT_TOPICS_REGEX, "ont edge index consumer started");
    loop {
        match consumer.recv().await {
            Ok(msg) => {
                let topic = msg.topic();
                match parse_edge_from_fact(topic, msg.payload()) {
                    Ok(Some(event)) => {
                        let mut s = stats.lock();
                        s.records_processed += 1;
                        match apply_edge_event(index, event) {
                            EdgeApply::Upserted => s.upserts += 1,
                            EdgeApply::Removed | EdgeApply::RemovedMissing => s.tombstones += 1,
                        }
                    }
                    Ok(None) => {
                        let mut s = stats.lock();
                        s.records_processed += 1;
                        s.skipped_non_edge += 1;
                    }
                    Err(e) => {
                        warn!(error = %e, topic = %topic, "skipping unparseable mem.fact edge record");
                        stats.lock().parse_errors += 1;
                    }
                }
            }
            Err(e) => return Err(format!("mem.fact edge recv: {e}")),
        }
    }
}

/// Spawn the consumer with a fixed 5s retry loop (mirrors the other consumers).
pub fn spawn_edge_consumer(
    config: EdgeConsumerConfig,
    index: RelationshipIndex,
) -> tokio::task::JoinHandle<()> {
    let stats = Arc::new(Mutex::new(EdgeStats::default()));
    tokio::spawn(async move {
        loop {
            match run_edge_consumer(&config, &index, &stats).await {
                Ok(()) => {
                    info!("ont edge consumer exited cleanly");
                    return;
                }
                Err(e) => {
                    warn!(error = %e, "ont edge consumer errored, retrying in 5s");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    /// Build a mem.fact record value (direct envelope shape).
    #[allow(clippy::too_many_arguments)] // a test fixture builder — clarity over arity
    fn fact_bytes(
        ns: &str,
        subject: &str,
        predicate: &str,
        object: serde_json::Value,
        valid_from: Option<&str>,
        valid_to: Option<&str>,
        tombstoned: bool,
        off: i64,
    ) -> Vec<u8> {
        let mut v = serde_json::json!({
            "namespace": ns,
            "type": "fact",
            "body": {"subject": subject, "predicate": predicate, "object": object},
            "source": {"topic": format!("mem.raw.{ns}"), "offsets": [off]},
            "tombstoned": tombstoned,
        });
        if let Some(vf) = valid_from {
            v["valid_from"] = serde_json::json!(vf);
        }
        if let Some(vt) = valid_to {
            v["valid_to"] = serde_json::json!(vt);
        }
        serde_json::to_vec(&v).unwrap()
    }

    #[test]
    fn tenant_from_topic_ok_and_bad() {
        assert_eq!(tenant_from_topic("mem.fact.acme").unwrap(), "acme");
        assert!(matches!(tenant_from_topic("ont.types.acme"), Err(ParseError::BadTopic(_))));
        assert!(matches!(tenant_from_topic("mem.fact."), Err(ParseError::BadTopic(_))));
        assert!(matches!(tenant_from_topic("mem.fact.a.b"), Err(ParseError::BadTopic(_))));
    }

    #[test]
    fn topic_regex_matches_expected() {
        let re = regex::Regex::new(MEM_FACT_TOPICS_REGEX).unwrap();
        assert!(re.is_match("mem.fact.acme"));
        assert!(!re.is_match("mem.fact."));
        assert!(!re.is_match("mem.fact.a.b"));
        assert!(!re.is_match("ont.types.acme"));
    }

    #[test]
    fn parse_edge_upsert_string_object() {
        let b = fact_bytes("ns1", "T2", "blocked_by", serde_json::json!("T1"), Some("2026-01-01T00:00:00Z"), None, false, 5);
        let ev = parse_edge_from_fact("mem.fact.ns1", Some(&b)).unwrap().unwrap();
        match ev {
            EdgeEvent::Upsert(e) => {
                assert_eq!((e.namespace.as_str(), e.from.as_str(), e.edge_type.as_str(), e.to.as_str()), ("ns1", "T2", "blocked_by", "T1"));
                assert_eq!(e.valid_from, Some(ts("2026-01-01T00:00:00Z")));
                assert_eq!(e.provenance, vec![SourceRef { topic: "mem.raw.ns1".into(), offsets: vec![5] }]);
            }
            _ => panic!("expected upsert"),
        }
    }

    #[test]
    fn parse_skips_non_string_object_and_empties() {
        // numeric object = attribute, not an edge
        let b = fact_bytes("ns1", "T1", "priority", serde_json::json!(3), None, None, false, 1);
        assert!(parse_edge_from_fact("mem.fact.ns1", Some(&b)).unwrap().is_none());
        // empty value
        assert!(parse_edge_from_fact("mem.fact.ns1", Some(b"")).unwrap().is_none());
        // kafka null
        assert!(parse_edge_from_fact("mem.fact.ns1", None).unwrap().is_none());
    }

    #[test]
    fn parse_tombstone_from_tombstoned_flag() {
        let b = fact_bytes("ns1", "T2", "blocked_by", serde_json::json!("T1"), None, None, true, 9);
        let ev = parse_edge_from_fact("mem.fact.ns1", Some(&b)).unwrap().unwrap();
        assert_eq!(
            ev,
            EdgeEvent::Tombstone { namespace: "ns1".into(), from: "T2".into(), edge_type: "blocked_by".into(), to: "T1".into() }
        );
    }

    #[test]
    fn bidirectional_lookup_and_dedup() {
        let idx = RelationshipIndex::new();
        let up = |s, p, o| apply_edge_event(&idx, parse_edge_from_fact("mem.fact.ns1", Some(&fact_bytes("ns1", s, p, serde_json::json!(o), None, None, false, 0))).unwrap().unwrap());
        up("T3", "blocked_by", "T2");
        up("T2", "blocked_by", "T1");
        up("T2", "blocked_by", "T1"); // duplicate -> dedup, not double

        // outgoing from T2
        let out = idx.outgoing("ns1", "T2", Some("blocked_by"), None);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].to, "T1");
        // incoming to T1 (the reverse lookup) -> T2
        let inc = idx.incoming("ns1", "T1", None, None);
        assert_eq!(inc.len(), 1);
        assert_eq!(inc[0].from, "T2");
        // incoming to T2 -> T3
        assert_eq!(idx.incoming("ns1", "T2", None, None)[0].from, "T3");
        assert_eq!(idx.edge_count(), 2); // dedup held
    }

    #[test]
    fn tombstone_removes_from_both_directions() {
        let idx = RelationshipIndex::new();
        apply_edge_event(&idx, parse_edge_from_fact("mem.fact.ns1", Some(&fact_bytes("ns1", "T2", "blocked_by", serde_json::json!("T1"), None, None, false, 0))).unwrap().unwrap());
        assert_eq!(idx.outgoing("ns1", "T2", None, None).len(), 1);
        assert_eq!(idx.incoming("ns1", "T1", None, None).len(), 1);
        let out = apply_edge_event(&idx, parse_edge_from_fact("mem.fact.ns1", Some(&fact_bytes("ns1", "T2", "blocked_by", serde_json::json!("T1"), None, None, true, 1))).unwrap().unwrap());
        assert_eq!(out, EdgeApply::Removed);
        assert!(idx.outgoing("ns1", "T2", None, None).is_empty());
        assert!(idx.incoming("ns1", "T1", None, None).is_empty());
    }

    #[test]
    fn as_of_filters_by_validity_interval() {
        let idx = RelationshipIndex::new();
        // edge valid [2026-01-01, 2026-06-01)
        apply_edge_event(&idx, parse_edge_from_fact("mem.fact.ns1", Some(&fact_bytes("ns1", "T2", "blocked_by", serde_json::json!("T1"), Some("2026-01-01T00:00:00Z"), Some("2026-06-01T00:00:00Z"), false, 0))).unwrap().unwrap());
        // before start -> none
        assert!(idx.outgoing("ns1", "T2", None, Some(ts("2025-12-01T00:00:00Z"))).is_empty());
        // during -> present
        assert_eq!(idx.outgoing("ns1", "T2", None, Some(ts("2026-03-01T00:00:00Z"))).len(), 1);
        // after end (invalidated) -> none
        assert!(idx.outgoing("ns1", "T2", None, Some(ts("2026-09-01T00:00:00Z"))).is_empty());
        // no filter -> present
        assert_eq!(idx.outgoing("ns1", "T2", None, None).len(), 1);
    }

    #[test]
    fn supersession_updates_valid_to_in_place() {
        let idx = RelationshipIndex::new();
        // first: open-ended edge
        apply_edge_event(&idx, parse_edge_from_fact("mem.fact.ns1", Some(&fact_bytes("ns1", "T2", "blocked_by", serde_json::json!("T1"), Some("2026-01-01T00:00:00Z"), None, false, 0))).unwrap().unwrap());
        // superseding record closes the interval (same identity -> replace, not duplicate)
        apply_edge_event(&idx, parse_edge_from_fact("mem.fact.ns1", Some(&fact_bytes("ns1", "T2", "blocked_by", serde_json::json!("T1"), Some("2026-01-01T00:00:00Z"), Some("2026-05-01T00:00:00Z"), false, 1))).unwrap().unwrap());
        assert_eq!(idx.edge_count(), 1); // updated in place
        // now invalidated after 2026-05-01
        assert!(idx.outgoing("ns1", "T2", None, Some(ts("2026-06-01T00:00:00Z"))).is_empty());
    }

    #[test]
    fn walk_multi_hop_both_directions() {
        let idx = RelationshipIndex::new();
        let up = |s, p, o| apply_edge_event(&idx, parse_edge_from_fact("mem.fact.ns1", Some(&fact_bytes("ns1", s, p, serde_json::json!(o), None, None, false, 0))).unwrap().unwrap());
        // chain: T3 -blocked_by-> T2 -blocked_by-> T1
        up("T3", "blocked_by", "T2");
        up("T2", "blocked_by", "T1");

        // outgoing 2-hop from T3 reaches T2 (depth 1) and T1 (depth 2)
        let fwd = idx.walk("ns1", "T3", Some("blocked_by"), Direction::Outgoing, 2, None);
        let mut reached: Vec<(usize, String)> = fwd.iter().map(|(d, e)| (*d, e.to.clone())).collect();
        reached.sort();
        assert_eq!(reached, vec![(1, "T2".to_string()), (2, "T1".to_string())]);

        // incoming 2-hop from T1 reaches T2 (depth 1) and T3 (depth 2) — the
        // reverse walk on-demand traversal cannot do
        let rev = idx.walk("ns1", "T1", Some("blocked_by"), Direction::Incoming, 2, None);
        let mut back: Vec<(usize, String)> = rev.iter().map(|(d, e)| (*d, e.from.clone())).collect();
        back.sort();
        assert_eq!(back, vec![(1, "T2".to_string()), (2, "T3".to_string())]);

        // depth 1 stops at the first hop
        assert_eq!(idx.walk("ns1", "T3", Some("blocked_by"), Direction::Outgoing, 1, None).len(), 1);
    }

    #[test]
    fn walk_is_cycle_safe() {
        let idx = RelationshipIndex::new();
        let up = |s, p, o| apply_edge_event(&idx, parse_edge_from_fact("mem.fact.ns1", Some(&fact_bytes("ns1", s, p, serde_json::json!(o), None, None, false, 0))).unwrap().unwrap());
        // A -> B -> A cycle
        up("A", "rel", "B");
        up("B", "rel", "A");
        // must terminate and not revisit
        let w = idx.walk("ns1", "A", Some("rel"), Direction::Outgoing, 10, None);
        assert_eq!(w.len(), 2); // A->B (d1), B->A (d2); A already visited, stop
    }

    #[test]
    fn walk_respects_as_of() {
        let idx = RelationshipIndex::new();
        apply_edge_event(&idx, parse_edge_from_fact("mem.fact.ns1", Some(&fact_bytes("ns1", "T2", "blocked_by", serde_json::json!("T1"), Some("2026-01-01T00:00:00Z"), Some("2026-06-01T00:00:00Z"), false, 0))).unwrap().unwrap());
        // during validity
        assert_eq!(idx.walk("ns1", "T2", None, Direction::Outgoing, 3, Some(ts("2026-03-01T00:00:00Z"))).len(), 1);
        // after invalidation
        assert!(idx.walk("ns1", "T2", None, Direction::Outgoing, 3, Some(ts("2026-09-01T00:00:00Z"))).is_empty());
    }

    #[test]
    fn namespace_isolation() {
        let idx = RelationshipIndex::new();
        apply_edge_event(&idx, parse_edge_from_fact("mem.fact.acme", Some(&fact_bytes("acme:c1", "T2", "blocked_by", serde_json::json!("T1"), None, None, false, 0))).unwrap().unwrap());
        // same tenant topic, different namespace record -> isolated by namespace key
        assert!(idx.outgoing("other:c2", "T2", None, None).is_empty());
        assert_eq!(idx.outgoing("acme:c1", "T2", None, None).len(), 1);
    }
}
