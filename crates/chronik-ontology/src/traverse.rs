//! Link traversal — Phase O-1 (Link Types).
//!
//! Dogfood model: an **edge** is a fact whose object is another entity —
//! `(subject) --[predicate]--> (object)`. `mem.fact.{tenant}` already stores
//! these triples with provenance, so a Link Type needs no separate store for
//! O-1: a traversal is a `predicate`-filtered fact query, chained for multi-hop.
//!
//! `assemble_edges` is pure (unit-tested without a broker); `traverse` is the
//! thin async BFS over `/_search`, reusing [`crate::resolve`]'s envelope +
//! provenance + `as_of` machinery.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::resolve::{filter_as_of, SourceRef};

/// One derived edge `(from) --[edge_type]--> (to)`, with provenance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Edge {
    pub from: String,
    pub edge_type: String,
    /// The object of the fact — an entity id (string) is traversable; other
    /// JSON values are terminal.
    pub to: serde_json::Value,
    /// Source event(s) that justify this edge.
    pub provenance: Vec<SourceRef>,
    /// Hop distance from the traversal root (1 = direct).
    pub depth: usize,
}

fn normalize(s: &str) -> String {
    s.trim().to_lowercase()
}

// Re-export the envelope extractor via a local copy of the tolerant logic so
// traverse doesn't depend on resolve's private fn.
fn envelope_from_source(source: &serde_json::Value) -> Option<serde_json::Value> {
    if source.get("body").is_some() || source.get("type").is_some() {
        return Some(source.clone());
    }
    for f in ["value", "_value", "_json_content"] {
        if let Some(s) = source.get(f).and_then(|v| v.as_str()) {
            if let Ok(inner) = serde_json::from_str::<serde_json::Value>(s) {
                return Some(inner);
            }
        }
    }
    None
}

/// PURE: extract the edges `(from_id) --[edge_type]--> (object)` from fact
/// `_source` records, at `depth`. Filters by namespace, subject == from_id, and
/// predicate == edge_type; skips tombstones. `edge_type == "*"` matches any
/// predicate (all outgoing edges).
pub fn assemble_edges(
    sources: &[serde_json::Value],
    namespace: &str,
    from_id: &str,
    edge_type: &str,
    normalize_id: bool,
    depth: usize,
) -> Vec<Edge> {
    let want_from = if normalize_id {
        normalize(from_id)
    } else {
        from_id.to_string()
    };
    let mut edges = Vec::new();
    for src in sources {
        let Some(env) = envelope_from_source(src) else {
            continue;
        };
        if env.get("tombstoned").and_then(|v| v.as_bool()).unwrap_or(false) {
            continue;
        }
        if let Some(rec_ns) = env.get("namespace").and_then(|v| v.as_str()) {
            if rec_ns != namespace {
                continue;
            }
        }
        let fbody = env.get("body").unwrap_or(&env);
        let Some(subject) = fbody.get("subject").and_then(|v| v.as_str()) else {
            continue;
        };
        let subj = if normalize_id {
            normalize(subject)
        } else {
            subject.to_string()
        };
        if subj != want_from {
            continue;
        }
        let predicate = fbody
            .get("predicate")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if edge_type != "*" && predicate != edge_type {
            continue;
        }
        let to = fbody.get("object").cloned().unwrap_or(serde_json::Value::Null);
        let provenance = env
            .get("source")
            .map(|s| SourceRef {
                topic: s
                    .get("topic")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                offsets: s
                    .get("offsets")
                    .and_then(|v| v.as_array())
                    .map(|a| a.iter().filter_map(|o| o.as_i64()).collect())
                    .unwrap_or_default(),
            })
            .into_iter()
            .collect();
        edges.push(Edge {
            from: subject.to_string(),
            edge_type: predicate,
            to,
            provenance,
            depth,
        });
    }
    edges
}

/// Traversal errors (thin wrapper over the search call).
#[derive(Debug, thiserror::Error)]
pub enum TraverseError {
    #[error("edge search request failed: {0}")]
    Http(String),
    #[error("edge search returned status {0}")]
    Status(u16),
    #[error("edge search response unparseable: {0}")]
    BadResponse(String),
}

#[derive(Deserialize)]
struct SearchResponse {
    hits: HitsInfo,
}
#[derive(Deserialize)]
struct HitsInfo {
    hits: Vec<Hit>,
}
#[derive(Deserialize)]
struct Hit {
    #[serde(rename = "_source")]
    source: serde_json::Value,
}

/// One hop: fetch `(from_id) --[edge_type]--> …` edges from the fact projection.
#[allow(clippy::too_many_arguments)]
async fn one_hop(
    http: &reqwest::Client,
    api_base: &str,
    fact_topic: &str,
    namespace: &str,
    from_id: &str,
    edge_type: &str,
    normalize_id: bool,
    max: usize,
    as_of: Option<DateTime<Utc>>,
    depth: usize,
) -> Result<Vec<Edge>, TraverseError> {
    let req = serde_json::json!({
        "index": fact_topic,
        "size": max,
        "query": {"match": {"_all": from_id}}
    });
    let url = format!("{}/_search", api_base.trim_end_matches('/'));
    let resp = http
        .post(&url)
        .json(&req)
        .send()
        .await
        .map_err(|e| TraverseError::Http(e.to_string()))?;
    if !resp.status().is_success() {
        if resp.status().as_u16() == 404 {
            return Ok(vec![]);
        }
        return Err(TraverseError::Status(resp.status().as_u16()));
    }
    let parsed: SearchResponse = resp
        .json()
        .await
        .map_err(|e| TraverseError::BadResponse(e.to_string()))?;
    let sources: Vec<serde_json::Value> =
        filter_as_of(parsed.hits.hits.into_iter().map(|h| h.source).collect(), as_of);
    Ok(assemble_edges(&sources, namespace, from_id, edge_type, normalize_id, depth))
}

/// Breadth-first multi-hop traversal (depth 1..=`max_depth`). Follows string
/// (entity-id) targets; non-string objects are terminal. Cycles are avoided by
/// tracking visited node ids. Returns every edge discovered, each tagged with
/// its hop depth.
#[allow(clippy::too_many_arguments)]
pub async fn traverse(
    http: &reqwest::Client,
    api_base: &str,
    fact_topic_prefix: &str,
    namespace: &str,
    from_id: &str,
    edge_type: &str,
    max_depth: usize,
    max_per_hop: usize,
    as_of: Option<DateTime<Utc>>,
) -> Result<Vec<Edge>, TraverseError> {
    let tenant = namespace.split(':').next().unwrap_or(namespace);
    let fact_topic = format!("{}.{}", fact_topic_prefix, tenant);
    let mut all = Vec::new();
    let mut visited = std::collections::HashSet::new();
    visited.insert(normalize(from_id));
    let mut frontier = vec![from_id.to_string()];
    for depth in 1..=max_depth.max(1) {
        let mut next = Vec::new();
        for node in &frontier {
            let edges = one_hop(
                http, api_base, &fact_topic, namespace, node, edge_type, true, max_per_hop,
                as_of, depth,
            )
            .await?;
            for e in edges {
                // Follow string targets we haven't visited.
                if let Some(t) = e.to.as_str() {
                    let key = normalize(t);
                    if visited.insert(key) {
                        next.push(t.to_string());
                    }
                }
                all.push(e);
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }
    Ok(all)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fact(subject: &str, predicate: &str, object: serde_json::Value, off: i64) -> serde_json::Value {
        serde_json::json!({
            "namespace": "ns1",
            "type": "fact",
            "body": {"subject": subject, "predicate": predicate, "object": object},
            "source": {"topic": "mem.raw.ns1", "offsets": [off], "extractor": "t@1"}
        })
    }

    #[test]
    fn assemble_edges_filters_predicate_and_cites_provenance() {
        let sources = vec![
            fact("Alice", "works_at", serde_json::json!("Acme"), 1),
            fact("Alice", "manages", serde_json::json!("Bob"), 2), // different edge type
            fact("Bob", "works_at", serde_json::json!("Acme"), 3),  // different subject
        ];
        let edges = assemble_edges(&sources, "ns1", "alice", "works_at", true, 1);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].from, "Alice");
        assert_eq!(edges[0].edge_type, "works_at");
        assert_eq!(edges[0].to, serde_json::json!("Acme"));
        assert_eq!(edges[0].provenance, vec![SourceRef { topic: "mem.raw.ns1".into(), offsets: vec![1] }]);
        assert_eq!(edges[0].depth, 1);
    }

    #[test]
    fn wildcard_edge_type_returns_all_outgoing() {
        let sources = vec![
            fact("Alice", "works_at", serde_json::json!("Acme"), 1),
            fact("Alice", "manages", serde_json::json!("Bob"), 2),
        ];
        let mut types: Vec<String> = assemble_edges(&sources, "ns1", "Alice", "*", true, 1)
            .into_iter()
            .map(|e| e.edge_type)
            .collect();
        types.sort();
        assert_eq!(types, vec!["manages".to_string(), "works_at".to_string()]);
    }

    #[test]
    fn cross_namespace_edges_excluded() {
        let mut f = fact("Alice", "works_at", serde_json::json!("Acme"), 1);
        f["namespace"] = serde_json::json!("other");
        assert!(assemble_edges(&[f], "ns1", "Alice", "works_at", true, 1).is_empty());
    }
}
