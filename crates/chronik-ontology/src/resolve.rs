//! Instance resolution — materialize an [`ObjectType`] instance from its backing
//! projection, on demand, with provenance. Milestone B (`get_object`).
//!
//! Reuses Chronik's `/_search` endpoint (the same `_source` shape `recall.rs`
//! parses): query the backing `mem.fact.{namespace}` topic, filter hits to the
//! requested identity, and map each fact's `predicate` to a declared attribute —
//! citing the fact's `source.{topic,offsets}` as the attribute's provenance.
//!
//! The HTTP call is a thin shell around [`assemble_instance`], which is pure and
//! unit-tested without a broker.

use serde::{Deserialize, Serialize};

use crate::object_type::ObjectType;

/// A source-event citation for a resolved attribute value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceRef {
    pub topic: String,
    pub offsets: Vec<i64>,
}

/// One resolved attribute of an object instance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResolvedAttr {
    pub name: String,
    /// Value(s). Single-valued attributes keep the most recent; multi keep all.
    pub values: Vec<serde_json::Value>,
    /// The source event(s) that justify this attribute (the provenance cite).
    pub provenance: Vec<SourceRef>,
}

/// A materialized object instance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObjectInstance {
    pub type_name: String,
    pub id: String,
    pub namespace: String,
    pub attributes: Vec<ResolvedAttr>,
    /// How many backing records contributed (diagnostic).
    pub backing_records: usize,
}

/// Resolution errors.
#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error("backing search request failed: {0}")]
    Http(String),
    #[error("backing search returned status {0}")]
    Status(u16),
    #[error("backing search response unparseable: {0}")]
    BadResponse(String),
}

// ---- /_search wire shapes (mirror recall.rs) --------------------------------

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

fn normalize(s: &str) -> String {
    s.trim().to_lowercase()
}

/// Extract the memory envelope object from a search hit `_source` — tolerates the
/// **direct** shape (envelope fields present) and the **wrapped** shape (the
/// envelope JSON is a string under `value`/`_value`/`_json_content`).
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

/// PURE assembly: given the raw `_source` values of backing fact records, filter
/// to the requested identity and map predicates to the type's declared
/// attributes, carrying provenance. Returns `None` when nothing matches.
pub fn assemble_instance(
    sources: &[serde_json::Value],
    namespace: &str,
    ty: &ObjectType,
    id: &str,
) -> Option<ObjectInstance> {
    let want_id = if ty.identity.normalize {
        normalize(id)
    } else {
        id.to_string()
    };

    // predicate -> (values, provenance), insertion-ordered by first sight.
    use std::collections::BTreeMap;
    let mut by_pred: BTreeMap<String, (Vec<serde_json::Value>, Vec<SourceRef>)> = BTreeMap::new();
    let mut contributing = 0usize;

    for src in sources {
        let Some(env) = envelope_from_source(src) else {
            continue;
        };
        // Skip tombstones.
        if env.get("tombstoned").and_then(|v| v.as_bool()).unwrap_or(false) {
            continue;
        }
        // Fact fields live under the flattened `body`; fall back to the envelope
        // itself if the search layer already unwrapped them.
        let fbody = env.get("body").unwrap_or(&env);
        let Some(subject) = fbody.get(&ty.identity.id_field).and_then(|v| v.as_str()) else {
            continue;
        };
        let subj_key = if ty.identity.normalize {
            normalize(subject)
        } else {
            subject.to_string()
        };
        if subj_key != want_id {
            continue;
        }
        let predicate = fbody
            .get("predicate")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let object = fbody.get("object").cloned().unwrap_or(serde_json::Value::Null);
        let prov = env.get("source").map(|s| SourceRef {
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
        });
        let entry = by_pred.entry(predicate).or_default();
        entry.0.push(object);
        if let Some(p) = prov {
            entry.1.push(p);
        }
        contributing += 1;
    }

    if contributing == 0 {
        return None;
    }

    // Map matched predicates onto the declared attributes (declaration order).
    let mut attributes = Vec::new();
    for attr in &ty.attributes {
        let pred = attr.from_predicate.as_deref().unwrap_or(&attr.name);
        if let Some((values, provenance)) = by_pred.get(pred) {
            let vals = if attr.multi {
                values.clone()
            } else {
                // single-valued: keep the most recent contribution
                values.last().cloned().into_iter().collect()
            };
            attributes.push(ResolvedAttr {
                name: attr.name.clone(),
                values: vals,
                provenance: provenance.clone(),
            });
        }
    }

    Some(ObjectInstance {
        type_name: ty.type_name.clone(),
        id: id.to_string(),
        namespace: namespace.to_string(),
        attributes,
        backing_records: contributing,
    })
}

/// Resolve one object instance by querying its backing projection over `/_search`.
///
/// `api_base` is the Unified API base (e.g. `http://localhost:6092`). Returns
/// `Ok(None)` when the backing topic is absent (404) or nothing matches the id.
pub async fn resolve_object(
    http: &reqwest::Client,
    api_base: &str,
    namespace: &str,
    ty: &ObjectType,
    id: &str,
    max_facts: usize,
) -> Result<Option<ObjectInstance>, ResolveError> {
    let topic = format!("{}.{}", ty.backing.topic_prefix, namespace);
    // BM25 recall on the id narrows the candidate set; `assemble_instance` then
    // applies the EXACT identity filter, so a loose match here is harmless.
    let req = serde_json::json!({
        "index": topic,
        "size": max_facts,
        "query": {"match": {"_all": id}}
    });
    let url = format!("{}/_search", api_base.trim_end_matches('/'));
    let resp = http
        .post(&url)
        .json(&req)
        .send()
        .await
        .map_err(|e| ResolveError::Http(e.to_string()))?;
    if !resp.status().is_success() {
        if resp.status().as_u16() == 404 {
            return Ok(None); // backing topic not created yet
        }
        return Err(ResolveError::Status(resp.status().as_u16()));
    }
    let parsed: SearchResponse = resp
        .json()
        .await
        .map_err(|e| ResolveError::BadResponse(e.to_string()))?;
    let sources: Vec<serde_json::Value> = parsed.hits.hits.into_iter().map(|h| h.source).collect();
    Ok(assemble_instance(&sources, namespace, ty, id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object_type::{AttrSpec, AttrType, BackingBinding, IdentitySpec};

    fn entity_type() -> ObjectType {
        ObjectType {
            type_name: "Entity".to_string(),
            attributes: vec![
                AttrSpec {
                    name: "degree".to_string(),
                    attr_type: AttrType::String,
                    from_predicate: Some("has_degree".to_string()),
                    multi: false,
                },
                AttrSpec {
                    name: "hobbies".to_string(),
                    attr_type: AttrType::String,
                    from_predicate: Some("enjoys".to_string()),
                    multi: true,
                },
            ],
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

    /// A `_source` in the flattened envelope shape.
    fn fact(subject: &str, predicate: &str, object: serde_json::Value, raw_offset: i64) -> serde_json::Value {
        serde_json::json!({
            "memory_id": format!("id-{predicate}-{raw_offset}"),
            "namespace": "ns1",
            "type": "fact",
            "body": {"subject": subject, "predicate": predicate, "object": object, "text": "..."},
            "source": {"topic": "mem.raw.acme.ns1.conv1", "offsets": [raw_offset], "extractor": "x@1"}
        })
    }

    #[test]
    fn resolves_attributes_with_provenance() {
        let ty = entity_type();
        let sources = vec![
            fact("Alice", "has_degree", serde_json::json!("Business Administration"), 12),
            fact("Alice", "enjoys", serde_json::json!("hiking"), 20),
            fact("Alice", "enjoys", serde_json::json!("chess"), 41),
            fact("Bob", "has_degree", serde_json::json!("Physics"), 5), // different subject, excluded
        ];
        let inst = assemble_instance(&sources, "ns1", &ty, "alice").unwrap();
        assert_eq!(inst.type_name, "Entity");
        assert_eq!(inst.backing_records, 3); // 3 Alice facts, Bob excluded

        let degree = inst.attributes.iter().find(|a| a.name == "degree").unwrap();
        assert_eq!(degree.values, vec![serde_json::json!("Business Administration")]);
        // provenance cites the source raw-turn offsets — the hard gate.
        assert_eq!(degree.provenance, vec![SourceRef { topic: "mem.raw.acme.ns1.conv1".into(), offsets: vec![12] }]);

        let hobbies = inst.attributes.iter().find(|a| a.name == "hobbies").unwrap();
        assert_eq!(hobbies.values, vec![serde_json::json!("hiking"), serde_json::json!("chess")]); // multi keeps all
        assert_eq!(hobbies.provenance.len(), 2);
    }

    #[test]
    fn every_resolved_value_carries_provenance() {
        // The ≥95% provenance-cite gate: every attribute here must have a cite.
        let ty = entity_type();
        let sources = vec![fact("Alice", "has_degree", serde_json::json!("BA"), 7)];
        let inst = assemble_instance(&sources, "ns1", &ty, "Alice").unwrap();
        assert!(inst.attributes.iter().all(|a| !a.provenance.is_empty()));
    }

    #[test]
    fn unknown_id_returns_none() {
        let ty = entity_type();
        let sources = vec![fact("Alice", "has_degree", serde_json::json!("BA"), 7)];
        assert!(assemble_instance(&sources, "ns1", &ty, "Nobody").is_none());
    }

    #[test]
    fn tombstoned_facts_are_skipped() {
        let ty = entity_type();
        let mut tomb = fact("Alice", "has_degree", serde_json::json!("BA"), 7);
        tomb["tombstoned"] = serde_json::json!(true);
        assert!(assemble_instance(&[tomb], "ns1", &ty, "Alice").is_none());
    }

    #[test]
    fn wrapped_source_shape_is_parsed() {
        // Some search responses wrap the envelope JSON as a string under `value`.
        let ty = entity_type();
        let env = fact("Alice", "has_degree", serde_json::json!("BA"), 7);
        let wrapped = serde_json::json!({ "value": serde_json::to_string(&env).unwrap() });
        let inst = assemble_instance(&[wrapped], "ns1", &ty, "Alice").unwrap();
        assert_eq!(inst.backing_records, 1);
        assert_eq!(inst.attributes[0].values, vec![serde_json::json!("BA")]);
    }

    #[test]
    fn predicate_without_declared_attribute_is_ignored() {
        // A fact whose predicate maps to no declared attribute contributes to the
        // record count but produces no attribute.
        let ty = entity_type();
        let sources = vec![fact("Alice", "unmapped_predicate", serde_json::json!("x"), 3)];
        let inst = assemble_instance(&sources, "ns1", &ty, "Alice").unwrap();
        assert_eq!(inst.backing_records, 1);
        assert!(inst.attributes.is_empty());
    }
}
